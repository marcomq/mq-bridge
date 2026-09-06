//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::{
    check_source_position_available, create_consumer_from_route,
    create_consumer_from_route_with_policy, create_publisher_from_route_with_source_position,
    output_has_write_time_named_object_store, output_passes_through_http_status,
    output_requires_source_metadata, relax_object_naming, supports_source_metadata,
};
use crate::errors::ProcessingError;
pub use crate::models::Route;
use crate::models::{Endpoint, EndpointType, Middleware, NameBy, RouteOptions};
use crate::traits::{
    with_disconnect_outcome, BatchCommitFunc, ConsumerError, DisconnectOutcome, EndpointStatus,
    Handler, HandlerError, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError,
    SentBatch,
};
use async_channel::{bounded, Sender};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::{
    select,
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error, info, trace, warn};

// Re-export extensions for backward compatibility and internal usage
pub use crate::extensions::{
    get_endpoint_factory, get_middleware_factory, register_endpoint_factory,
    register_middleware_factory,
};

/// Why a route's task terminated.
///
/// Read via [`RouteHandle::outcome`], which returns `None` while the route is
/// still running. This is what lets a supervisor of drain-then-exit jobs report
/// whether a batch job succeeded — [`RouteHandle::status`] reports *connection*
/// health, which stays `healthy` for a route that ran cleanly to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteOutcome {
    /// The source drained and the route exited on its own — `exit_on_empty`
    /// or an exhausted stream. The job succeeded.
    ///
    /// "Completed" describes the *route*, not the data: a run whose sink rejected
    /// messages permanently with no `dlq` middleware configured still completes, with
    /// the dropped count and cause reported in [`EndpointStatus::error`] via
    /// [`RouteHandle::status`]. A caller that must distinguish a clean run from a
    /// dirty one has to read that field too.
    Completed,
    /// Terminated by an explicit `stop()` or shutdown signal.
    Stopped,
    /// Terminated by a permanent error; the cause is in
    /// [`EndpointStatus::error`] from [`RouteHandle::status`].
    Failed,
}

/// Publishes the terminal [`RouteOutcome`] when the route task ends.
///
/// Publishing from `Drop` rather than inline keeps the invariant total: a task
/// that panics or is aborted still resolves to `Failed` instead of leaving
/// [`RouteHandle::outcome`] `None` forever, and the outcome becomes visible
/// only once the task is actually done.
struct OutcomeGuard {
    outcome: Arc<RwLock<Option<RouteOutcome>>>,
    status: Arc<RwLock<EndpointStatus>>,
    drops: Arc<RwLock<DropReport>>,
    resolved: Option<RouteOutcome>,
}

/// Messages the route discarded because the sink rejected them permanently and
/// no DLQ was configured.
///
/// Accumulated by the route task and published by [`OutcomeGuard`] as the route
/// ends, rather than written to the status directly: the reconnect loop clears
/// `EndpointStatus::error` when a pass reports ready, so a drop recorded inline
/// races that reset and is usually erased before anyone can read it.
#[derive(Default)]
pub(crate) struct DropReport {
    count: u64,
    last_cause: Option<String>,
}

impl OutcomeGuard {
    fn set(&mut self, outcome: RouteOutcome) {
        self.resolved = Some(outcome);
    }
}

impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        let outcome = self.resolved.unwrap_or_else(|| {
            // Reaching here means the loop never recorded a terminal, so the
            // task panicked or was aborted; any prior `error` is stale.
            let mut s = recover_write_lock(&self.status, "route_handle_status");
            s.healthy = false;
            s.error = Some("route task panicked or was aborted".to_string());
            RouteOutcome::Failed
        });
        // A drained source can finish before the reconnect loop processes its
        // buffered ready signal or before a recovered pass reaches `STABLE_RUN`.
        // Either way, a clean terminal outcome makes any connection error stale.
        if matches!(outcome, RouteOutcome::Completed) {
            let mut s = recover_write_lock(&self.status, "route_handle_status");
            s.healthy = true;
            s.error = None;
        }
        // A route that discarded data did not run clean, whatever its outcome.
        // Only a `Failed` route's cause is kept over the drop report: it explains
        // why the route stopped. On any other outcome a lingering `error` is a
        // stale reconnect failure the route recovered from, so the drops win.
        {
            let drops = recover_read_lock(&self.drops, "route_drop_report");
            if drops.count > 0 {
                let mut s = recover_write_lock(&self.status, "route_handle_status");
                if s.error.is_none() || !matches!(outcome, RouteOutcome::Failed) {
                    s.error = Some(format!(
                        "dropped {} message(s): sink rejected them permanently and no dlq middleware is configured: {}",
                        drops.count,
                        drops.last_cause.as_deref().unwrap_or("unknown cause")
                    ));
                }
            }
        }
        *recover_write_lock(&self.outcome, "route_handle_outcome") = Some(outcome);
    }
}

#[derive(Debug)]
pub struct RouteHandle {
    handle: JoinHandle<()>,
    shutdown_tx: Sender<()>,
    /// Live connection health of the running route, updated by the reconnect
    /// loop on every (re)connect and failure. Read via [`RouteHandle::status`].
    status: Arc<RwLock<EndpointStatus>>,
    /// Terminal outcome, published once by the run task as it exits.
    /// `None` while the route is still running. Read via [`RouteHandle::outcome`].
    outcome: Arc<RwLock<Option<RouteOutcome>>>,
}

impl RouteHandle {
    pub async fn stop(&self) {
        let _ = self.shutdown_tx.send(()).await;
        self.shutdown_tx.close();
    }

    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.handle.await
    }

    /// Returns why the route terminated, or `None` while it is still running.
    ///
    /// `Some` exactly when the route's task has finished, so a supervisor that
    /// keeps handles in a map (to keep offering `stop`/`status`) can poll this
    /// to tell a completed route from a running one — [`join`](Self::join)
    /// consumes the handle, so it can't serve that purpose.
    ///
    /// Unlike [`status`](Self::status), which reports connection health, this
    /// distinguishes a clean drain from a stop and from a permanent failure, so
    /// a supervisor can report whether a batch job actually succeeded.
    pub fn outcome(&self) -> Option<RouteOutcome> {
        *recover_read_lock(&self.outcome, "route_handle_outcome")
    }

    /// Returns the live health of the running route without opening a new connection.
    ///
    /// The reconnect loop updates this on every (re)connect attempt and failure, so a
    /// supervisor can distinguish "running and connected" (`healthy == true`) from
    /// "running but failing to connect / reconnecting" (`healthy == false`, with `error`
    /// set to the last connection error).
    pub fn status(&self) -> EndpointStatus {
        recover_read_lock(&self.status, "route_handle_status").clone()
    }
}

pub(crate) async fn run_publisher_connect_hook(
    route_name: &str,
    publisher: &Arc<dyn MessagePublisher>,
) -> anyhow::Result<()> {
    if let Some(hook) = publisher.on_connect_hook() {
        hook.await.map_err(|err| {
            anyhow::anyhow!(
                "Publisher on_connect hook failed for route '{}': {}",
                route_name,
                err
            )
        })?;
    }
    Ok(())
}

pub(crate) async fn run_consumer_connect_hook(
    route_name: &str,
    consumer: &dyn MessageConsumer,
) -> anyhow::Result<()> {
    if let Some(hook) = consumer.on_connect_hook() {
        hook.await.map_err(|err| {
            anyhow::anyhow!(
                "Consumer on_connect hook failed for route '{}': {}",
                route_name,
                err
            )
        })?;
    }
    Ok(())
}

/// The outcome a runner's return value describes: `Ok(false)` is a natural end,
/// `Ok(true)` a shutdown, `Err` a failure.
pub(crate) fn disconnect_outcome(result: &anyhow::Result<bool>) -> DisconnectOutcome {
    match result {
        Ok(false) => DisconnectOutcome::Completed,
        Ok(true) => DisconnectOutcome::Stopped,
        Err(_) => DisconnectOutcome::Failed,
    }
}

pub(crate) async fn run_publisher_disconnect_hook(
    route_name: &str,
    publisher: &Arc<dyn MessagePublisher>,
    outcome: DisconnectOutcome,
) {
    // Scoped rather than passed as an argument, so it reaches the sink through any depth
    // of middleware without each wrapper having to forward it.
    with_disconnect_outcome(outcome, async {
        if let Some(hook) = publisher.on_disconnect_hook() {
            if let Err(err) = hook.await {
                warn!(
                    "Publisher on_disconnect hook failed for route '{}': {}",
                    route_name, err
                );
            }
        }
    })
    .await;
}

pub(crate) async fn run_consumer_disconnect_hook(route_name: &str, consumer: &dyn MessageConsumer) {
    if let Some(hook) = consumer.on_disconnect_hook() {
        if let Err(err) = hook.await {
            warn!(
                "Consumer on_disconnect hook failed for route '{}': {}",
                route_name, err
            );
        }
    }
}

/// Builds a bare handle from a raw task + shutdown channel.
///
/// NOTE: a handle made this way is not wired to an `OutcomeGuard`, so
/// [`RouteHandle::outcome`] stays `None` for the task's whole lifetime and
/// cannot report completion — the guard can only be attached inside the spawned
/// task body, which this conversion does not own. Use [`Route::run`] when you
/// need outcome polling; this exists only for callers that assemble a task by
/// hand and never inspect the outcome.
impl From<(JoinHandle<()>, Sender<()>)> for RouteHandle {
    fn from(tuple: (JoinHandle<()>, Sender<()>)) -> Self {
        RouteHandle {
            handle: tuple.0,
            shutdown_tx: tuple.1,
            status: Arc::new(RwLock::new(EndpointStatus::default())),
            outcome: Arc::new(RwLock::new(None)),
        }
    }
}

struct ActiveRoute {
    route: Route,
    handle: RouteHandle,
}

static ROUTE_REGISTRY: OnceLock<RwLock<HashMap<String, ActiveRoute>>> = OnceLock::new();
static ENDPOINT_REF_REGISTRY: OnceLock<RwLock<HashMap<String, Endpoint>>> = OnceLock::new();

fn recover_read_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        warn!(lock = name, "Recovering from poisoned read lock");
        poisoned.into_inner()
    })
}

fn recover_write_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        warn!(lock = name, "Recovering from poisoned write lock");
        poisoned.into_inner()
    })
}

/// Registers a named endpoint that can be referenced by other endpoints using `ref: "name"`.
/// This will overwrite any existing endpoint with the same name.
pub fn register_endpoint(name: &str, endpoint: Endpoint) {
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut writer = recover_write_lock(registry, "endpoint_ref_registry");
    if writer.insert(name.to_string(), endpoint).is_some() {
        debug!("Overwriting a registered endpoint named '{}'", name);
    }
}

/// Retrieves a registered endpoint by name.
pub fn get_endpoint(name: &str) -> Option<Endpoint> {
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = recover_read_lock(registry, "endpoint_ref_registry");
    reader.get(name).cloned()
}

fn check_fault_middleware_allowed(
    endpoint: &Endpoint,
    route_name: &str,
    role: &str,
    depth: usize,
    visited: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(anyhow::anyhow!(
            "[route:{}] Endpoint policy recursion depth exceeded limit of {}",
            route_name,
            MAX_DEPTH
        ));
    }

    if endpoint
        .middlewares
        .iter()
        .any(|m| matches!(m, Middleware::RandomPanic(cfg) if cfg.enabled))
    {
        return Err(anyhow::anyhow!(
            "[route:{}] random_panic middleware is disabled by default for {} endpoints; set allow_fault_injection: true to enable it",
            route_name,
            role
        ));
    }

    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            if !visited.insert(name.clone()) {
                return Ok(());
            }
            if let Some(referenced) = get_endpoint(name) {
                check_fault_middleware_allowed(&referenced, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Fanout(endpoints) => {
            for endpoint in endpoints {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Switch(cfg) => {
            for endpoint in cfg.cases.values() {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
            if let Some(endpoint) = &cfg.default {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Reader(inner) => {
            check_fault_middleware_allowed(inner, route_name, role, depth + 1, visited)?;
        }
        _ => {}
    }

    Ok(())
}

fn endpoint_tree_has_buffer(endpoint: &Endpoint, visited_refs: &mut HashSet<String>) -> bool {
    if endpoint
        .middlewares
        .iter()
        .any(|middleware| matches!(middleware, Middleware::Buffer(_)))
    {
        return true;
    }

    if endpoint.middlewares.iter().any(|middleware| {
        matches!(middleware, Middleware::Dlq(config) if endpoint_tree_has_buffer(&config.endpoint, visited_refs))
    }) {
        return true;
    }

    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            visited_refs.insert(name.clone())
                && get_endpoint(name)
                    .is_some_and(|referenced| endpoint_tree_has_buffer(&referenced, visited_refs))
        }
        EndpointType::Fanout(endpoints) => endpoints
            .iter()
            .any(|endpoint| endpoint_tree_has_buffer(endpoint, visited_refs)),
        EndpointType::Switch(config) => config
            .cases
            .values()
            .chain(config.when.iter().map(|case| &case.to))
            .chain(config.default.iter().map(Box::as_ref))
            .any(|endpoint| endpoint_tree_has_buffer(endpoint, visited_refs)),
        EndpointType::Reader(inner) => endpoint_tree_has_buffer(inner, visited_refs),
        EndpointType::Request(config) => {
            endpoint_tree_has_buffer(&config.to, visited_refs)
                || endpoint_tree_has_buffer(&config.forward_to, visited_refs)
        }
        _ => false,
    }
}

/// How many messages a runner may process before it must cooperatively yield.
///
/// The route loops drive `async-channel` recv/send, which complete synchronously
/// on a hot in-memory pipeline and don't count against tokio's cooperative
/// budget. Without an explicit yield a busy route can starve other tasks (other
/// routes, the drain side, shutdown) — but yielding every batch costs a full
/// scheduler round-trip per iteration, which dominates throughput at small batch
/// sizes. Amortizing the yield over this many processed messages keeps the time
/// between yields bounded while making the cost negligible.
///
/// Set to 128 to match tokio's automatic cooperative-scheduling budget, so this
/// manual yield fires at the same cadence tokio uses for its own resources. Only
/// affects `batch_size` smaller than this (larger batches already exceed it in a
/// single iteration and yield once per batch regardless).
const YIELD_EVERY_MSGS: usize = 128;

/// How many reconnects in a row a route with `exit_on_empty` may make before it is
/// declared failed. A drain job that keeps failing on the same pass — an output leg at a
/// dead address, say — never reaches the empty batch it exits on, so retrying forever
/// leaves it running with nothing to show. A continuous route has no such bound: coming
/// back after an outage is the whole point.
const DRAIN_MAX_RECONNECT_ATTEMPTS: usize = 10;

/// How long a connection has to stay up before the route counts as recovered: the flap
/// counter resets and the health cell goes back to healthy. Until then a reconnect keeps
/// the last error visible, so a route that only ever connects and immediately fails is
/// never reported healthy on the strength of the connect alone.
const STABLE_RUN: std::time::Duration = std::time::Duration::from_secs(30);

/// Forward a ready signal the run task already emitted but the reconnect loop
/// never observed.
///
/// The inner task signals ready onto a `bounded(1)` channel *before* its consume
/// loop, so the signal sits buffered. A drain over a small source can then reach
/// its terminal within the same scheduling window, leaving `select!` with two
/// ready branches and free to pick the terminal one at random. Without this, a
/// route that ran to completion is reported to `run()` as one that never started.
async fn forward_buffered_ready(
    startup_notified: &mut bool,
    iter_ready_rx: &async_channel::Receiver<()>,
    ready_tx: &Sender<()>,
) {
    if !*startup_notified && iter_ready_rx.try_recv().is_ok() {
        let _ = ready_tx.send(()).await;
        *startup_notified = true;
    }
}

async fn pause_after_empty_batch(delay_ms: u64) {
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    } else {
        tokio::task::yield_now().await;
    }
}

/// Drained commit and worker tasks yield a `JoinError` only when the task panicked, which the
/// task's own error handling never sees. A panicked commit means a batch was never acked, so
/// these must not be dropped silently.
fn report_join_result<T>(res: std::result::Result<T, tokio::task::JoinError>, what: &str) {
    if let Err(e) = res {
        if e.is_cancelled() {
            debug!("{} was cancelled before completing", what);
        } else {
            error!("{} panicked: {}", what, e);
        }
    }
}

fn report_route_error(err_tx: &Sender<anyhow::Error>, err: anyhow::Error, context: &str) {
    match err_tx.try_send(err) {
        Ok(_) => trace!("Reported error to main task"),
        Err(err_send) => warn!(
            error = ?err_send,
            "{}; main task might be down or busy.",
            context
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransientPublishFailurePolicy {
    StopRoute,
    ReplyBadGateway,
}

fn transient_publish_failure_policy(
    input: &Endpoint,
    pass_through_status: bool,
) -> TransientPublishFailurePolicy {
    const MAX_DEPTH: usize = 16;

    let EndpointType::Ref(name) = &input.endpoint_type else {
        return match &input.endpoint_type {
            EndpointType::Http(cfg)
                if !cfg.fire_and_forget && !cfg.receive_streamable && pass_through_status =>
            {
                TransientPublishFailurePolicy::ReplyBadGateway
            }
            _ => TransientPublishFailurePolicy::StopRoute,
        };
    };
    let mut name = name.clone();
    for _ in 0..MAX_DEPTH {
        let Some(referenced) = get_endpoint(&name) else {
            return TransientPublishFailurePolicy::StopRoute;
        };
        match referenced.endpoint_type {
            EndpointType::Ref(next) => name = next,
            EndpointType::Http(cfg)
                if !cfg.fire_and_forget && !cfg.receive_streamable && pass_through_status =>
            {
                return TransientPublishFailurePolicy::ReplyBadGateway;
            }
            _ => return TransientPublishFailurePolicy::StopRoute,
        }
    }
    TransientPublishFailurePolicy::StopRoute
}

fn bad_gateway_reply(message_id: u128) -> MessageDisposition {
    let mut response = crate::CanonicalMessage::new(b"Bad Gateway".to_vec(), Some(message_id));
    response
        .metadata
        .insert("http_status_code".to_string(), "502".to_string());
    response.metadata.insert(
        "content-type".to_string(),
        "text/plain; charset=utf-8".to_string(),
    );
    MessageDisposition::Reply(response)
}

fn apply_server_failure_policy(
    policy: TransientPublishFailurePolicy,
    message_ids: &[u128],
    dispositions: &mut [MessageDisposition],
) {
    if policy != TransientPublishFailurePolicy::ReplyBadGateway {
        return;
    }
    for (message_id, disposition) in message_ids.iter().zip(dispositions) {
        if matches!(disposition, MessageDisposition::Nack) {
            *disposition = bad_gateway_reply(*message_id);
        }
    }
}

/// Records a batch the route discarded after a permanent sink rejection.
///
/// Dropping is deliberate — it is what stops a poison message from wedging the
/// route — but `err_tx` only carries route-*stopping* errors, so without this a
/// drop is invisible: the route resolves to [`RouteOutcome::Completed`] with
/// `error: None` and the caller cannot tell a clean run from one that threw data
/// away. [`OutcomeGuard`] publishes the tally when the route ends.
fn record_dropped_messages(
    drops: Option<&Arc<RwLock<DropReport>>>,
    dropped: usize,
    cause: &PublisherError,
) {
    let Some(drops) = drops else { return };
    let mut report = recover_write_lock(drops, "route_drop_report");
    report.count += dropped as u64;
    report.last_cause = Some(cause.to_string());
}

struct BatchScratch {
    message_ids: Vec<u128>,
    request_ids: HashSet<u128>,
}

impl BatchScratch {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            message_ids: Vec::with_capacity(capacity),
            request_ids: HashSet::with_capacity(capacity),
        }
    }

    fn fill_from(&mut self, messages: &[crate::CanonicalMessage]) {
        self.message_ids.clear();
        self.message_ids
            .extend(messages.iter().map(|m| m.message_id));
        self.request_ids.clear();
        self.request_ids.extend(
            messages
                .iter()
                .filter(|m| m.metadata.contains_key("reply_to"))
                .map(|m| m.message_id),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_batch_and_commit(
    publisher: &Arc<dyn MessagePublisher>,
    messages: Vec<crate::CanonicalMessage>,
    commit: BatchCommitFunc,
    has_retry_middleware: bool,
    has_dlq_middleware: bool,
    transient_failure_policy: TransientPublishFailurePolicy,
    err_tx: &Sender<anyhow::Error>,
    commit_semaphore: Option<&Arc<tokio::sync::Semaphore>>,
    commit_tasks: &mut JoinSet<()>,
    scratch: &mut BatchScratch,
    drops: Option<&Arc<RwLock<DropReport>>>,
    ticket: Option<OrderTicket>,
) -> anyhow::Result<()> {
    let batch_len = messages.len();
    scratch.fill_from(&messages);

    // An order-sensitive sink admits one send at a time, in source order. Everything
    // above and below this call stays concurrent.
    let sent = {
        let _release = match ticket {
            Some(mut ticket) => {
                if let Some(prev) = ticket.prev.take() {
                    let _ = prev.await;
                }
                Some(ticket)
            }
            None => None,
        };
        publisher.send_batch(messages).await
    };
    match sent {
        Ok(SentBatch::Ack) => {
            for id in scratch.message_ids.iter() {
                if scratch.request_ids.contains(id) {
                    warn!("Message {:032x} expected a reply (reply_to set), but publisher returned Ack. Response loop broken.", id);
                }
            }
            let dispositions = scratch
                .message_ids
                .iter()
                .map(|id| {
                    if scratch.request_ids.contains(id) {
                        MessageDisposition::Nack
                    } else {
                        MessageDisposition::Ack
                    }
                })
                .collect();
            // Acquire the dispatch slot before spawning so a slow commit backstreams
            // pressure to the producer instead of queueing tasks unbounded.
            let permit = acquire_commit_permit(commit_semaphore).await;
            let err_tx = err_tx.clone();
            // Reap finished commits so completed results don't accumulate until shutdown.
            while let Some(res) = commit_tasks.try_join_next() {
                report_join_result(res, "Commit task");
            }
            commit_tasks.spawn(async move {
                let _permit = permit;
                if let Err(e) = commit(dispositions).await {
                    error!("Commit failed: {}", e);
                    report_route_error(&err_tx, e, "Could not send commit error to main task");
                }
            });
            Ok(())
        }
        Ok(SentBatch::Partial { responses, failed }) => {
            let has_transient = failed.iter().any(|(_, e)| {
                matches!(
                    e,
                    PublisherError::Retryable(_) | PublisherError::Connection(_)
                )
            });
            if has_transient {
                let (_, first_err) = failed
                    .iter()
                    .find(|(_, e)| {
                        matches!(
                            e,
                            PublisherError::Retryable(_) | PublisherError::Connection(_)
                        )
                    })
                    .expect("has_transient is true");
                let err = anyhow::anyhow!(
                    "Transient error in batch send ({} messages failed). First error: {}",
                    failed.len(),
                    first_err
                );
                // Non-retryable entries in a mixed batch are Ack'ed (dropped) below,
                // so account for them here before the transient path returns.
                if !has_dlq_middleware {
                    let mut dropped = 0usize;
                    let mut cause = None;
                    for (msg, e) in &failed {
                        if matches!(e, PublisherError::NonRetryable(_)) {
                            error!(
                                "Dropping message (ID: {:032x}) due to non-retryable error: {}",
                                msg.message_id, e
                            );
                            dropped += 1;
                            cause.get_or_insert(e);
                        }
                    }
                    if let Some(cause) = cause {
                        record_dropped_messages(drops, dropped, cause);
                    }
                }
                let mut dispositions = map_responses_to_dispositions(
                    &scratch.message_ids,
                    responses,
                    &failed,
                    &scratch.request_ids,
                    has_dlq_middleware,
                );
                apply_server_failure_policy(
                    transient_failure_policy,
                    &scratch.message_ids,
                    &mut dispositions,
                );
                if let Err(commit_err) = commit(dispositions).await {
                    warn!("Commit after transient failure also failed: {}", commit_err);
                }
                if transient_failure_policy != TransientPublishFailurePolicy::StopRoute {
                    warn!(
                        "Transient publisher error returned to server request; keeping listener active: {}",
                        err
                    );
                    return Ok(());
                }
                if !has_retry_middleware {
                    return Err(err);
                }
                warn!(
                    "Transient error in batch, message(s) Nack'ed for re-delivery: {}",
                    err
                );
                return Ok(());
            }

            for (msg, e) in &failed {
                error!(
                    "Dropping message (ID: {:032x}) due to non-retryable error: {}",
                    msg.message_id, e
                );
            }
            if !has_dlq_middleware {
                if let Some((_, first)) = failed.first() {
                    record_dropped_messages(drops, failed.len(), first);
                }
            }
            let err_tx = err_tx.clone();
            let dispositions = map_responses_to_dispositions(
                &scratch.message_ids,
                responses,
                &failed,
                &scratch.request_ids,
                has_dlq_middleware,
            );
            let permit = acquire_commit_permit(commit_semaphore).await;
            // Reap finished commits so completed results don't accumulate until shutdown.
            while let Some(res) = commit_tasks.try_join_next() {
                report_join_result(res, "Commit task");
            }
            commit_tasks.spawn(async move {
                let _permit = permit;
                if let Err(e) = commit(dispositions).await {
                    error!("Commit failed: {}", e);
                    report_route_error(&err_tx, e, "Could not send commit error to main task");
                }
            });
            Ok(())
        }
        Err(e) => {
            let non_retryable = matches!(e, PublisherError::NonRetryable(_));
            let transient = matches!(
                e,
                PublisherError::Retryable(_) | PublisherError::Connection(_)
            );
            let disposition = if non_retryable && !has_dlq_middleware {
                MessageDisposition::Ack
            } else {
                MessageDisposition::Nack
            };
            let mut dispositions = vec![disposition; batch_len];
            if transient {
                apply_server_failure_policy(
                    transient_failure_policy,
                    &scratch.message_ids,
                    &mut dispositions,
                );
            }
            let commit_result = commit(dispositions).await;
            debug!("Failure commit result: {:?}", commit_result);
            if transient && transient_failure_policy != TransientPublishFailurePolicy::StopRoute {
                commit_result?;
                warn!(
                    "Transient publisher error returned to server request; keeping listener active: {}",
                    e
                );
                return Ok(());
            }
            if non_retryable && !has_dlq_middleware {
                commit_result?;
                // Acking a permanently-rejected batch keeps the route from
                // re-reading the same poison messages forever, but the data is
                // gone. Record it: a drop that leaves no trace is how a route
                // reports a clean success while having discarded messages.
                for id in scratch.message_ids.iter() {
                    error!(
                        "Dropping message (ID: {:032x}) due to non-retryable error: {}",
                        id, e
                    );
                }
                record_dropped_messages(drops, batch_len, &e);
                Ok(())
            } else {
                Err(e.into())
            }
        }
    }
}

impl Route {
    /// Whether this route's input can stamp a replay position, which is what `name_by: auto`
    /// resolves against. A `ref` input that cannot be resolved counts as no position, so `auto`
    /// falls back to `write_time` rather than failing the route on a naming choice.
    fn source_has_position(&self) -> bool {
        // A ref may name another ref, so follow the chain the way `resolve_endpoint_recursive`
        // does. The depth bound doubles as the cycle guard: a loop simply runs out of it.
        const MAX_DEPTH: usize = 16;
        let EndpointType::Ref(name) = &self.input.endpoint_type else {
            return supports_source_metadata(&self.input.endpoint_type);
        };
        let mut name = name.clone();
        for _ in 0..MAX_DEPTH {
            let Some(referenced) = get_endpoint(&name) else {
                return false;
            };
            match referenced.endpoint_type {
                EndpointType::Ref(next) => name = next,
                endpoint_type => return supports_source_metadata(&endpoint_type),
            }
        }
        false
    }

    /// Returns the sink mechanism that makes replayed writes idempotent, when it can be
    /// established from configuration alone. This is informational: the route remains
    /// at-least-once internally, while the observable sink result is effectively-once.
    fn inferred_idempotency_mechanism(&self, output: &Endpoint) -> Option<&'static str> {
        if output.handler.is_some() {
            return None;
        }
        let source_has_position = self.source_has_position();
        match &output.endpoint_type {
            EndpointType::MongoDb(config) if config.id_field.is_some() => {
                Some("MongoDB unique _id")
            }
            EndpointType::Sqlx(config) => {
                let query = config.insert_query.as_deref()?.to_ascii_uppercase();
                query
                    .split_once("ON CONFLICT")
                    .is_some_and(|(_, conflict_action)| conflict_action.contains("DO NOTHING"))
                    .then_some("SQL unique-key conflict handling")
            }
            EndpointType::File(config)
                if config.resolved_name_by(source_has_position) == NameBy::SourcePosition =>
            {
                Some("part names carrying the source range")
            }
            EndpointType::ObjectStore(config)
                if config.resolved_name_by(source_has_position) == NameBy::SourcePosition =>
            {
                Some("object names carrying the source range")
            }
            _ => None,
        }
    }

    /// Creates a new route with default concurrency (1) and batch size (512).
    ///
    /// # Arguments
    /// * `input` - The input/source endpoint for the route
    /// * `output` - The output/sink endpoint for the route
    pub fn new(input: Endpoint, output: Endpoint) -> Self {
        Self {
            input,
            output,
            ..Default::default()
        }
    }

    /// Retrieves a registered (and running) route by name.
    pub fn get(name: &str) -> Option<Self> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = recover_read_lock(registry, "route_registry");
        map.get(name).map(|active| active.route.clone())
    }

    /// Returns a list of all registered route names.
    pub fn list() -> Vec<String> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = recover_read_lock(registry, "route_registry");
        map.keys().cloned().collect()
    }

    /// Returns true if the input is of type ref (and the output isn't)
    pub fn is_ref(&self) -> bool {
        matches!(self.input.endpoint_type, EndpointType::Ref(_))
            && !matches!(self.output.endpoint_type, EndpointType::Ref(_))
    }

    /// Registers the route's output endpoint under the given name.
    /// This allows other routes to reference this output using `ref: "name"`.
    pub fn register_output_endpoint(&self, name: Option<&str>) -> Result<(), anyhow::Error> {
        match name {
            Some(name) => {
                register_endpoint(name, self.output.clone());
            }
            None => {
                if let EndpointType::Ref(name) = &self.input.endpoint_type {
                    register_endpoint(name, self.output.clone());
                } else {
                    return Err(anyhow::anyhow!(
                        "No name and input is not a reference endpoint"
                    ));
                }
            }
        };
        Ok(())
    }

    /// Registers the route and starts it.
    /// If a route with the same name is already running, it will be stopped first.
    ///    
    /// # Examples
    /// ```
    /// use mq_bridge::{Route, models::Endpoint};
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// route.deploy("global_route").await.unwrap();
    /// assert!(Route::get("global_route").is_some());
    /// # });
    /// ```
    pub async fn deploy(&self, name: &str) -> anyhow::Result<()> {
        Self::stop(name).await;

        let handle = self.run(name).await?;
        let active = ActiveRoute {
            route: self.clone(),
            handle,
        };

        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let mut map = recover_write_lock(registry, "route_registry");
        map.insert(name.to_string(), active);
        Ok(())
    }

    /// Stops a running route by name and removes it from the registry.
    /// Waits up to 5 seconds for the route task to join; if the timeout elapses
    /// the task is aborted and the implementation awaits the aborted handle to
    /// ensure the background task has fully terminated before returning.
    pub async fn stop(name: &str) -> bool {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let active_opt = {
            let mut map = recover_write_lock(registry, "route_registry");
            map.remove(name)
        };

        if let Some(active) = active_opt {
            // Move the handle out so we can operate on its internals.
            let handle = active.handle;

            // Signal the route to stop and close the shutdown channel.
            let _ = handle.shutdown_tx.send(()).await;
            handle.shutdown_tx.close();

            // Extract the JoinHandle so we can monitor and, if needed, abort it.
            let mut join_handle = handle.handle;
            tokio::select! {
                res = &mut join_handle => {
                    // The task finished naturally within the 5s window
                    let _ = res;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    // The 5s timer finished first - abort the task to ensure it doesn't linger.
                    join_handle.abort();
                    // Await the handle one last time to ensure the task has fully shut down.
                    let _ = join_handle.await;
                }
            }

            true
        } else {
            false
        }
    }

    /// Creates a new Publisher configured for this route's output.
    /// This is useful if you want to send messages to the same destination as this route.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// use mq_bridge::{Route, models::Endpoint};
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    /// let publisher = route.create_publisher().await;
    /// assert!(publisher.is_ok());
    /// # });
    /// ```
    pub async fn create_publisher(&self) -> anyhow::Result<crate::Publisher> {
        crate::Publisher::new(self.output.clone()).await
    }

    /// Creates a consumer connected to the route's output.
    /// This is primarily useful for integration tests to verify messages reaching the destination.
    pub async fn connect_to_output(
        &self,
        name: &str,
    ) -> anyhow::Result<Box<dyn crate::traits::MessageConsumer>> {
        create_consumer_from_route(name, &self.output).await
    }

    /// Validates the route configuration, checking if endpoints are supported and correctly configured.
    /// Core types like file, memory, and response are always supported.
    /// # Arguments
    /// * `name` - The name of the route
    /// * `allowed_endpoints` - An optional list of allowed endpoint types
    pub fn check(
        &self,
        name: &str,
        allowed_endpoints: Option<&[&str]>,
    ) -> anyhow::Result<Vec<String>> {
        self.options.validate()?;
        if !self.options.allow_fault_injection {
            check_fault_middleware_allowed(
                &self.input,
                name,
                "input",
                0,
                &mut std::collections::HashSet::new(),
            )?;
            check_fault_middleware_allowed(
                &self.output,
                name,
                "output",
                0,
                &mut std::collections::HashSet::new(),
            )?;
        }

        let mut warnings = Vec::new();
        warnings.extend(crate::endpoints::check_consumer(
            name,
            &self.input,
            allowed_endpoints,
        )?);
        warnings.extend(crate::endpoints::check_publisher(
            name,
            &self.output,
            allowed_endpoints,
        )?);
        if self.options.concurrency > 1
            && (endpoint_tree_has_buffer(&self.input, &mut HashSet::new())
                || endpoint_tree_has_buffer(&self.output, &mut HashSet::new()))
        {
            warnings.push(format!(
                "Route '{name}' uses buffer middleware with concurrency {}. Buffering preserves \
                 order within each batch, but concurrent destination writes may complete out of \
                 source order. Use concurrency: 1 when destination order must be preserved.",
                self.options.concurrency
            ));
        }
        Ok(warnings)
    }

    /// Runs the message processing route with concurrency, error handling, and graceful shutdown.
    ///
    /// This function spawns the necessary background tasks to process messages. It waits asynchronously
    /// until the route is successfully initialized (i.e., connections are established) or until
    /// a timeout occurs.
    /// The name_str parameter is just used for logging and tracing.
    ///
    /// It returns a `JoinHandle` for the main route task and a `Sender` channel
    /// that can be used to signal a graceful shutdown. The result is typically converted into a
    /// [`RouteHandle`] for easier management.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use mq_bridge::{Route, route::RouteHandle, models::Endpoint};
    /// # async fn example() -> anyhow::Result<()> {
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    ///
    /// // Start the route (blocks until initialized) and convert to RouteHandle
    /// let handle: RouteHandle = route.run("my_route").await?.into();
    ///
    /// // Stop the route later
    /// handle.stop().await;
    /// handle.join().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(&self, name_str: &str) -> anyhow::Result<RouteHandle> {
        self.run_with_resume_policy(name_str, false).await
    }

    /// Runs the route without optional cursor/checkpoint resume state.
    ///
    /// This is intended for deliberate full copies. It suppresses resume setup,
    /// warnings, and errors for cursor-based sources without changing native
    /// broker offsets or other consumption semantics.
    pub async fn run_without_resume(&self, name_str: &str) -> anyhow::Result<RouteHandle> {
        self.run_with_resume_policy(name_str, true).await
    }

    async fn run_with_resume_policy(
        &self,
        name_str: &str,
        no_resume: bool,
    ) -> anyhow::Result<RouteHandle> {
        let warnings = self.check(name_str, None)?;
        for warning in warnings {
            tracing::warn!(route = name_str, "Configuration warning: {}", warning);
        }
        let relaxed_output = relax_object_naming(
            name_str,
            self.source_has_position(),
            &self.input,
            &self.output,
        )?
        .map(|(output, _)| output);
        let inferred_output = relaxed_output.as_ref().unwrap_or(&self.output);
        if let Some(mechanism) = self.inferred_idempotency_mechanism(inferred_output) {
            tracing::info!(
                route = name_str,
                delivery = "effectively-once",
                mechanism,
                "Inferred delivery guarantee from idempotent sink configuration"
            );
        } else {
            tracing::info!(
                route = name_str,
                delivery = "at-least-once",
                "Inferred delivery guarantee"
            );
        }
        let startup_timeout = std::time::Duration::from_millis(self.options.startup_timeout_ms);
        let reconnect_interval =
            tokio::time::Duration::from_millis(self.options.reconnect_interval_ms);
        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (ready_tx, ready_rx) = bounded(1);
        // Use `Arc` so route/name clones are cheap (pointer copy) in the reconnect loop.
        let route = Arc::new(self.clone());
        let name = Arc::new(name_str.to_string());

        // Live health cell shared with the returned `RouteHandle`. Starts unhealthy
        // ("connecting") and is flipped to healthy once the route reports ready, back
        // to unhealthy (with the last error) whenever the run task fails.
        let status = Arc::new(RwLock::new(EndpointStatus {
            healthy: false,
            target: name_str.to_string(),
            error: Some("connecting".to_string()),
            ..Default::default()
        }));
        let status_loop = Arc::clone(&status);
        // Terminal outcome cell, published by `OutcomeGuard` as the task exits.
        let outcome = Arc::new(RwLock::new(None::<RouteOutcome>));
        // Tally of discarded messages, published alongside the terminal outcome.
        let drops = Arc::new(RwLock::new(DropReport::default()));
        let mut outcome_guard = OutcomeGuard {
            outcome: Arc::clone(&outcome),
            status: Arc::clone(&status),
            drops: Arc::clone(&drops),
            resolved: None,
        };

        let exit_on_empty = self.options.exit_on_empty;
        let handle = tokio::spawn(async move {
            // The startup `ready` channel is consumed once by `run()`; only the first
            // (re)connect needs to notify it.
            let mut startup_notified = false;
            // Reconnects since the route last ran stably. Drives both the drain-mode
            // bound below and the health cell: a route that reconnects on a loop is
            // flapping, and reporting it `healthy` the moment it connects again hides
            // exactly the failure an operator is looking for.
            let mut consecutive_failures = 0usize;
            'reconnect: loop {
                let route_arc = Arc::clone(&route);
                let name_arc = Arc::clone(&name);
                // Create a new, per-iteration internal shutdown channel.
                // This avoids a race where both this loop and the inner task
                // try to consume the same external shutdown signal.
                let (internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
                // Per-iteration ready channel so we can observe every (re)connection,
                // not just the first startup, and update the shared health cell.
                let (iter_ready_tx, iter_ready_rx) = bounded(1);

                // The actual route logic is in `run_until_err`.
                let drops_run = Arc::clone(&drops);
                let mut run_task = tokio::spawn(async move {
                    route_arc
                        .run_until_err_reporting_to(
                            &name_arc,
                            Some(internal_shutdown_rx),
                            Some(iter_ready_tx),
                            Some(&drops_run),
                            no_resume,
                        )
                        .await
                });

                // Inner loop: process ready + result events for this connection attempt.
                loop {
                    select! {
                        _ = shutdown_rx.recv() => {
                            info!("Shutdown signal received for route '{}'.", name);
                            // Notify the inner task to shut down.
                            let _ = internal_shutdown_tx.send(()).await;
                            // Wait for the inner task to finish gracefully.
                            let _ = run_task.await;
                            forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
                            outcome_guard.set(RouteOutcome::Stopped);
                            break 'reconnect;
                        }
                        // A pass that stays up this long has recovered: clear the flap
                        // state and the stale error. Disabled while healthy, so a route
                        // that never fails never arms the timer.
                        _ = tokio::time::sleep(STABLE_RUN), if consecutive_failures > 0 => {
                            consecutive_failures = 0;
                            let mut s = recover_write_lock(&status_loop, "route_handle_status");
                            s.healthy = true;
                            s.error = None;
                        }
                        Ok(_) = iter_ready_rx.recv() => {
                            // The route (re)connected and is ready to process messages.
                            // Connected is not the same as healthy while it is still
                            // failing on every pass: keep the last error visible until the
                            // pass above proves it stayed up.
                            {
                                let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                if consecutive_failures == 0 {
                                    s.healthy = true;
                                    s.error = None;
                                }
                            }
                            // Forward to the startup `ready` channel so `run()` can return.
                            if !startup_notified {
                                let _ = ready_tx.send(()).await;
                                startup_notified = true;
                            }
                            // Keep waiting for the run result / shutdown.
                        }
                        res = &mut run_task => {
                            forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
                            match res {
                                Ok(Ok(should_continue)) if !should_continue => {
                                    info!("Route '{}' completed gracefully. Shutting down.", name);
                                    outcome_guard.set(RouteOutcome::Completed);
                                    break 'reconnect;
                                }
                                Ok(Err(e)) => {
                                    // An exhausted source is a clean terminal, not a failure:
                                    // it ends the route the same way `exit_on_empty` does.
                                    let is_end_of_stream = e.downcast_ref::<ConsumerError>().is_some_and(|ce| matches!(ce, ConsumerError::EndOfStream));
                                    let is_permanent =
                                        e.downcast_ref::<ProcessingError>().is_some_and(|pe| matches!(pe, ProcessingError::NonRetryable(_)))
                                        || e.downcast_ref::<ConsumerError>().is_some_and(|ce| matches!(ce, ConsumerError::Permanent(_)))
                                        || is_end_of_stream;

                                    // EndOfStream is a clean terminal, not a failure, so
                                    // it must not mark the route unhealthy.
                                    if !is_end_of_stream {
                                        let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                        s.healthy = false;
                                        s.error = Some(e.to_string());
                                        consecutive_failures += 1;
                                    }

                                    // A drain job is finite: if it cannot get past the same
                                    // failure it will never reach the empty batch it is
                                    // waiting for, so bound it instead of retrying forever.
                                    // A continuous route is meant to reconnect indefinitely.
                                    if !is_permanent
                                        && exit_on_empty
                                        && consecutive_failures >= DRAIN_MAX_RECONNECT_ATTEMPTS
                                    {
                                        outcome_guard.set(RouteOutcome::Failed);
                                        error!(
                                            "Route '{}' failed {} times in a row while draining; giving up. Last error: {}",
                                            name, consecutive_failures, e
                                        );
                                        break 'reconnect;
                                    }

                                    if is_permanent {
                                        if is_end_of_stream {
                                            outcome_guard.set(RouteOutcome::Completed);
                                            info!("Route '{}' completed: end of stream. Shutting down.", name);
                                        } else {
                                            outcome_guard.set(RouteOutcome::Failed);
                                            error!("Route '{}' failed with a permanent error: {}. Shutting down.", name, e);
                                        }
                                        break 'reconnect;
                                    }

                                    warn!(
                                        "Route '{}' failed: {}. Reconnecting in {}ms...",
                                        name,
                                        e,
                                        reconnect_interval.as_millis()
                                    );
                                    if !reconnect_interval.is_zero() {
                                        tokio::time::sleep(reconnect_interval).await;
                                    }
                                    break; // -> next reconnect iteration
                                }
                                Err(e) => {
                                    {
                                        let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                        s.healthy = false;
                                        s.error = Some(format!("route task panicked: {}", e));
                                        consecutive_failures += 1;
                                    }
                                    // Same bound as the error arm: a drain whose task keeps
                                    // panicking will never reach its empty batch.
                                    if exit_on_empty
                                        && consecutive_failures >= DRAIN_MAX_RECONNECT_ATTEMPTS
                                    {
                                        outcome_guard.set(RouteOutcome::Failed);
                                        error!(
                                            "Route '{}' panicked {} times in a row while draining; giving up. Last error: {}",
                                            name, consecutive_failures, e
                                        );
                                        break 'reconnect;
                                    }
                                    error!(
                                        "Route '{}' task panicked: {}. Reconnecting in {}ms...",
                                        name,
                                        e,
                                        reconnect_interval.as_millis()
                                    );
                                    if !reconnect_interval.is_zero() {
                                        tokio::time::sleep(reconnect_interval).await;
                                    }
                                    break; // -> next reconnect iteration
                                }
                                _ => break, // The route should reconnect and continue running.
                            }
                        }
                    }
                }
            }
        });

        let started = tokio::time::timeout(startup_timeout, ready_rx.recv()).await;
        if let Ok(Ok(_)) = started {
            return Ok(RouteHandle {
                handle,
                shutdown_tx,
                status,
                outcome,
            });
        }
        // The startup failure itself stays inside the reconnect loop, so
        // surface the cause it recorded rather than a bare timeout.
        // "connecting" is the initial marker, not a recorded failure.
        //
        // Read before aborting: the abort drops `OutcomeGuard`, which replaces
        // the recorded cause with its panicked-or-aborted fallback as soon as
        // the task is polled. Under load that lands first and the real reason
        // — an unusable filter expression, say — is lost.
        let cause = recover_read_lock(&status, "route_handle_status")
            .error
            .clone()
            .filter(|e| e != "connecting")
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        handle.abort();
        Err(match started {
            // Every `ready_tx` was dropped: the route task ended without ever
            // signalling ready. That returns immediately, so calling it a timeout
            // sends the reader after a stall that never happened.
            Ok(Err(_)) => {
                let terminal = *recover_read_lock(&outcome, "route_handle_outcome");
                anyhow::anyhow!(
                    "Route '{}' failed to start: ended before it signalled ready (outcome: {}){}",
                    name_str,
                    terminal.map_or("unknown", |o| match o {
                        RouteOutcome::Completed => "completed",
                        RouteOutcome::Stopped => "stopped",
                        RouteOutcome::Failed => "failed",
                    }),
                    cause
                )
            }
            _ => anyhow::anyhow!(
                "Route '{}' failed to start: did not become ready within {}ms{}",
                name_str,
                startup_timeout.as_millis(),
                cause
            ),
        })
    }

    /// The core logic of running the route, designed to be called within a reconnect loop.
    pub async fn run_until_err(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
        ready_tx: Option<Sender<()>>,
    ) -> anyhow::Result<bool> {
        self.run_until_err_reporting_to(name, shutdown_rx, ready_tx, None, false)
            .await
    }

    /// [`run_until_err`](Self::run_until_err) with somewhere to report dropped
    /// messages. A deployed route passes its status cell so that a permanent
    /// sink rejection is still visible after the route completes; a caller that
    /// drives a `Route` directly has no such cell and passes `None`.
    pub(crate) async fn run_until_err_reporting_to(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let (_internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
        let shutdown_rx = shutdown_rx.unwrap_or(internal_shutdown_rx);
        if let Some(result) = crate::endpoints::try_run_fast_path_route(
            self,
            name,
            shutdown_rx.clone(),
            ready_tx.clone(),
        )
        .await
        {
            return result;
        }
        if self.options.concurrency == 1 {
            self.run_sequentially(name, shutdown_rx, ready_tx, drops, no_resume)
                .await
        } else {
            self.run_concurrently(name, shutdown_rx, ready_tx, drops, no_resume)
                .await
        }
    }

    /// A simplified, sequential runner for when concurrency is 1.
    async fn run_sequentially(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let source_has_position = self.source_has_position();
        let relaxed_output;
        let output =
            match relax_object_naming(name, source_has_position, &self.input, &self.output)? {
                Some((endpoint, reason)) => {
                    warn!(route = name, "{reason}");
                    relaxed_output = endpoint;
                    &relaxed_output
                }
                None => &self.output,
            };
        let source_metadata_required =
            output_requires_source_metadata(name, output, source_has_position)?;
        check_source_position_available(name, &self.input, source_metadata_required)?;
        let publisher =
            create_publisher_from_route_with_source_position(name, output, source_has_position)
                .await?;
        let transient_failure_policy = transient_publish_failure_policy(
            &self.input,
            output_passes_through_http_status(name, output)?,
        );
        let mut consumer = create_consumer_from_route_with_policy(
            name,
            &self.input,
            source_metadata_required,
            no_resume,
        )
        .await?;
        consumer.set_exit_on_empty(self.options.exit_on_empty);
        if let Err(err) = run_publisher_connect_hook(name, &publisher).await {
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Err(err) = run_consumer_connect_hook(name, consumer.as_ref()).await {
            run_consumer_disconnect_hook(name, consumer.as_ref()).await;
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        let (err_tx, err_rx) = bounded(1);
        let mut commit_tasks = JoinSet::new();

        // Ordered consumers (cumulative-ack) commit through the serial sequencer;
        // individual-ack consumers commit concurrently under a semaphore.
        let commit_router = CommitRouter::new(
            consumer.commit_requires_order(),
            self.options.commit_concurrency_limit,
        );
        let commit_semaphore = commit_router.dispatch_semaphore();
        let mut seq_counter = 0u64;

        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let mut batch_scratch = BatchScratch::with_capacity(self.options.batch_size);
        // Check if retry middleware is present on output
        let has_retry_middleware = self.output.has_retry_middleware();
        let has_dlq_middleware = self.output.has_dlq_middleware();
        // Messages processed since the last cooperative yield (see YIELD_EVERY_MSGS).
        let mut since_yield = 0usize;
        let mut run_result = loop {
            select! {
                Ok(err) = err_rx.recv() => break Err(err),

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", name);
                    break Ok(true); // Stopped by shutdown signal
                }
                res = consumer.receive_batch(self.options.batch_size) => {
                    let received_batch = match res {
                        Ok(batch) => {
                            if batch.messages.is_empty() {
                                if self.options.exit_on_empty {
                                    info!("Consumer for route '{}' drained (empty batch, exit_on_empty). Shutting down.", name);
                                    break Ok(false); // Graceful drain-then-exit
                                }
                                pause_after_empty_batch(self.options.empty_batch_delay_ms).await;
                                continue; // No messages, loop to select! again
                            }
                            batch
                        }
                        Err(ConsumerError::EndOfStream) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", name);
                            break Ok(false); // Graceful exit
                        }
                        Err(ConsumerError::Connection(e)) => {
                            // Propagate error to trigger reconnect by the outer loop
                            break Err(e);
                        },
                        Err(ConsumerError::Gap { requested, base }) => {
                            // Propagate gap error to trigger reconnect by the outer loop
                            break Err(anyhow::anyhow!("Consumer gap: requested offset {requested} but earliest available is {base}"));
                        }
                        Err(ConsumerError::Permanent(e)) => {
                            // Non-retryable: shut the route down instead of reconnecting
                            // and re-reading the same poison message forever.
                            break Err(ConsumerError::Permanent(e).into());
                        }
                    };
                    debug!("Received a batch of {} messages sequentially", received_batch.messages.len());

                    // Process the batch sequentially without spawning a new task
                    let seq = seq_counter;
                    seq_counter += 1;
                    let batch_len = received_batch.messages.len();
                    let commit = commit_router.wrap(received_batch.commit, seq);
                    if let Err(err) = send_batch_and_commit(
                        &publisher,
                        received_batch.messages,
                        commit,
                        has_retry_middleware,
                        has_dlq_middleware,
                        transient_failure_policy,
                        &err_tx,
                        commit_semaphore.as_ref(),
                        &mut commit_tasks,
                        &mut batch_scratch,
                        drops,
                        // The sequential runner sends one batch at a time already.
                        None,
                    )
                    .await
                    {
                        break Err(err);
                    }

                    // Cooperatively yield, amortized over messages processed so a
                    // hot loop can't starve other tasks (see YIELD_EVERY_MSGS).
                    since_yield += batch_len;
                    if since_yield >= YIELD_EVERY_MSGS {
                        since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
            }
        };

        // Drain errors while waiting for tasks to finish to prevent deadlocks and lost errors
        loop {
            select! {
                biased;
                res = err_rx.recv() => {
                    if let Ok(err) = res {
                        error!("Error reported during shutdown: {}", err);
                        if matches!(&run_result, Ok(false)) {
                            run_result = Err(err);
                        }
                    }
                }
                res = commit_tasks.join_next() => {
                    match res {
                        Some(res) => report_join_result(res, "Commit task"),
                        None => break,
                    }
                }
            }
        }
        drop(err_rx);
        commit_router.shutdown().await;
        run_consumer_disconnect_hook(name, consumer.as_ref()).await;
        run_publisher_disconnect_hook(name, &publisher, disconnect_outcome(&run_result)).await;
        run_result
    }

    /// The main concurrent runner for when concurrency > 1.
    async fn run_concurrently(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let source_has_position = self.source_has_position();
        let relaxed_output;
        let output =
            match relax_object_naming(name, source_has_position, &self.input, &self.output)? {
                Some((endpoint, reason)) => {
                    warn!(route = name, "{reason}");
                    relaxed_output = endpoint;
                    &relaxed_output
                }
                None => &self.output,
            };
        let source_metadata_required =
            output_requires_source_metadata(name, output, source_has_position)?;
        check_source_position_available(name, &self.input, source_metadata_required)?;
        // A write-time name is minted inside the worker pool, so it is arrival order here
        // whether or not the input could have supplied a position instead. Checked after the
        // relaxation above, so a route it just moved onto write-time names is warned about too.
        if output_has_write_time_named_object_store(name, output, source_has_position)? {
            warn!(
                route = name,
                concurrency = self.options.concurrency,
                "object_store sink names objects by write time, which this route's worker pool \
                 makes arrival order rather than source order. Replaying a change stream through \
                 the bucket can reorder updates to the same key. Set name_by: source_position \
                 where the input carries a replay position; otherwise concurrency: 1 is the only \
                 remedy."
            );
        }
        let publisher =
            create_publisher_from_route_with_source_position(name, output, source_has_position)
                .await?;
        let transient_failure_policy = transient_publish_failure_policy(
            &self.input,
            output_passes_through_http_status(name, output)?,
        );
        let mut consumer = create_consumer_from_route_with_policy(
            name,
            &self.input,
            source_metadata_required,
            no_resume,
        )
        .await?;
        consumer.set_exit_on_empty(self.options.exit_on_empty);
        if let Err(err) = run_publisher_connect_hook(name, &publisher).await {
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Err(err) = run_consumer_connect_hook(name, consumer.as_ref()).await {
            run_consumer_disconnect_hook(name, consumer.as_ref()).await;
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let (err_tx, err_rx) = bounded(1); // For critical, route-stopping errors

        // --- Publish Dispatch ---
        // Workers normally send concurrently, which lets whole batches reach the sink out of
        // source order. A sink that is an ordered log (the file sink) declares
        // `requires_ordered_publish()` and gets a per-batch [`OrderTicket`] instead, which
        // sequences the `send_batch` call itself. Only that call: batch prep, disposition
        // mapping and commits still run across the whole pool, which is why an ordered
        // route measures within noise of an unordered one.
        let ordered_publish = publisher.requires_ordered_publish();
        if ordered_publish {
            debug!(
                "Route '{}' publishes to an order-sensitive sink: sends are sequenced.",
                name
            );
        }
        // channel capacity is measured in batches, not messages
        let work_capacity = self.options.concurrency;
        let (work_tx, work_rx) = bounded::<(
            Vec<crate::CanonicalMessage>,
            BatchCommitFunc,
            Option<OrderTicket>,
        )>(work_capacity);
        // --- Commit Dispatch ---
        // Cumulative-ack brokers (Kafka/AMQP) must commit in order, so their commits
        // are funnelled through a single sequencer to prevent data loss. Individual-ack
        // brokers commit concurrently (bounded by commit_concurrency_limit) so the
        // per-batch ack round trip no longer caps throughput.
        let commit_router = CommitRouter::new(
            consumer.commit_requires_order(),
            self.options.commit_concurrency_limit,
        );
        // Shared across workers so the limit bounds total commits in flight, not per-worker.
        let commit_semaphore = commit_router.dispatch_semaphore();

        // --- Worker Pool ---
        let mut join_set = JoinSet::new();
        for i in 0..self.options.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            let commit_semaphore = commit_semaphore.clone();
            let mut commit_tasks = JoinSet::new();
            let has_retry_middleware = self.output.has_retry_middleware();
            let has_dlq_middleware = self.output.has_dlq_middleware();
            let batch_size = self.options.batch_size;
            // Owned per worker: the borrow cannot outlive this loop iteration.
            let drops = drops.cloned();
            join_set.spawn(async move {
                debug!("Starting worker {}", i);
                let mut batch_scratch = BatchScratch::with_capacity(batch_size);
                let mut since_yield = 0usize;
                while let Ok((messages, commit_func, ticket)) = work_rx_clone.recv().await {
                    let batch_len = messages.len();
                    if let Err(err) = send_batch_and_commit(
                        &publisher,
                        messages,
                        commit_func,
                        has_retry_middleware,
                        has_dlq_middleware,
                        transient_failure_policy,
                        &err_tx,
                        commit_semaphore.as_ref(),
                        &mut commit_tasks,
                        &mut batch_scratch,
                        drops.as_ref(),
                        ticket,
                    )
                    .await
                    {
                        error!("Worker failed to process message batch: {}", err);
                        report_route_error(&err_tx, err, "Could not send error to main task");
                        break;
                    }
                    // Amortized cooperative yield (see YIELD_EVERY_MSGS).
                    since_yield += batch_len;
                    if since_yield >= YIELD_EVERY_MSGS {
                        since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
                // Wait for all in-flight commits to complete
                while let Some(res) = commit_tasks.join_next().await {
                    report_join_result(res, "Commit task");
                }
            });
        }

        let mut seq_counter = 0u64;
        // Tail of the publish-ordering chain: what the *next* batch waits on. `None`
        // while the sink tolerates unordered sends.
        let mut prev_publish: Option<tokio::sync::oneshot::Receiver<()>> = None;
        // Messages enqueued to workers since the last cooperative yield (see YIELD_EVERY_MSGS).
        let mut since_yield = 0usize;
        // Holds an error that caused the loop to break, to be returned after graceful shutdown.
        let mut loop_error: Option<anyhow::Error> = None;
        // Set when the loop breaks on a graceful terminal -- an empty batch under
        // exit_on_empty, or a source reporting end of stream -- so we report a
        // completion rather than a shutdown-driven exit.
        let mut drained = false;
        loop {
            select! {
                biased; // Prioritize checking for errors

                Ok(err) = err_rx.recv() => {
                    error!("A worker reported a critical error. Shutting down route.");
                    loop_error = Some(err);
                    break;
                }

                Some(res) = join_set.join_next() => {
                    match res {
                        Ok(_) => {
                            error!("A worker task finished unexpectedly. Shutting down route.");
                            loop_error = Some(anyhow::anyhow!("Worker task finished unexpectedly"));
                        }
                        Err(e) => {
                            error!("A worker task panicked: {}. Shutting down route.", e);
                            loop_error = Some(e.into());
                        }
                    }
                    break;
                }

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in concurrent runner for route '{}'.", name);
                    break;
                }

                res = consumer.receive_batch(self.options.batch_size) => {
                    let (messages, commit) = match res {
                        Ok(batch) => {
                            if batch.messages.is_empty() {
                                if self.options.exit_on_empty {
                                    info!("Consumer for route '{}' drained (empty batch, exit_on_empty). Shutting down.", name);
                                    drained = true;
                                    break; // Graceful drain-then-exit
                                }
                                pause_after_empty_batch(self.options.empty_batch_delay_ms).await;
                                continue; // No messages, loop to select! again
                            }
                            (batch.messages, batch.commit)
                        }
                        Err(ConsumerError::EndOfStream) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", name);
                            // Without this the tail below reads "no shutdown was
                            // requested" as "reconnect", and the outer loop reruns
                            // the whole source again, forever.
                            drained = true;
                            break; // Graceful exit
                        }
                        Err(ConsumerError::Connection(e)) => {
                            // Propagate error to trigger reconnect by the outer loop
                            loop_error = Some(e);
                            break;
                        }
                        Err(ConsumerError::Gap { requested, base }) => {
                            // Propagate gap error to trigger reconnect by the outer loop
                            loop_error = Some(ConsumerError::Gap { requested, base }.into());
                            break;
                        }
                        Err(ConsumerError::Permanent(e)) => {
                            // Non-retryable: shut the route down instead of reconnecting
                            // and re-reading the same poison message forever.
                            loop_error = Some(ConsumerError::Permanent(e).into());
                            break;
                        }
                    };
                    debug!("Received a batch of {} messages concurrently", messages.len());

                    // Wrap the commit function to route it through the sequencer.
                    // Only advance the sequence counter after we've successfully enqueued
                    // the work item to avoid creating sequence gaps if the work channel
                    // is closed while producing batches.
                    let seq = seq_counter;
                    let batch_len = messages.len();
                    let wrapped_commit = commit_router.wrap(commit, seq);
                    let ticket = ordered_publish.then(|| {
                        let (release, next_prev) = tokio::sync::oneshot::channel();
                        let ticket = OrderTicket {
                            prev: prev_publish.take(),
                            _release: release,
                        };
                        prev_publish = Some(next_prev);
                        ticket
                    });

                    match work_tx.send((messages, wrapped_commit, ticket)).await {
                        Ok(()) => {
                            seq_counter += 1;
                        }
                        Err(e) => {
                            warn!("Work channel closed, cannot process more messages concurrently. Shutting down.");
                            // Recover the moved tuple so we can invoke the wrapped commit
                            // and resolve the batch with a NACK. Dropping the ticket
                            // releases whatever batch is queued behind this one.
                            let (msgs_back, wrapped_commit_back, _) = e.into_inner();
                            let _ = (wrapped_commit_back)(vec![crate::traits::MessageDisposition::Nack; msgs_back.len()]).await;
                            break;
                        }
                    }

                    // Amortized cooperative yield (see YIELD_EVERY_MSGS).
                    since_yield += batch_len;
                    if since_yield >= YIELD_EVERY_MSGS {
                        since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        // --- Graceful Shutdown ---
        // Close the work channel so workers drain their current messages and exit the loop.
        // This applies on both normal shutdown AND error paths, ensuring in-flight commits
        // are not aborted mid-sequence.
        drop(work_tx);
        // Wait for all worker tasks to complete.
        while let Some(res) = join_set.join_next().await {
            report_join_result(res, "Worker task");
        }

        // Close sequencer (if any) now that all in-flight commits have drained.
        commit_router.shutdown().await;
        run_consumer_disconnect_hook(name, consumer.as_ref()).await;
        // Decided once so the outcome the publisher is told cannot drift from what the
        // route returns. A drain-then-exit is a graceful completion, not a shutdown-driven
        // exit; what is left after it is a shutdown signal (channel empty means it was
        // closed/consumed) or a dropped connection the outer loop should reconnect.
        let result = if let Some(err) = loop_error {
            Err(err)
        } else if let Ok(err) = err_rx.try_recv() {
            Err(err)
        } else if drained {
            Ok(false)
        } else {
            Ok(shutdown_rx.is_empty())
        };
        run_publisher_disconnect_hook(name, &publisher, disconnect_outcome(&result)).await;
        result
    }

    pub fn with_options(mut self, options: RouteOptions) -> Self {
        self.options = options;
        self
    }
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.options.concurrency = concurrency.max(1);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.options.batch_size = batch_size.max(1);
        self
    }
    pub fn with_commit_concurrency_limit(mut self, limit: usize) -> Self {
        self.options.commit_concurrency_limit = limit.max(1);
        self
    }

    pub fn with_startup_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.options.startup_timeout_ms = timeout_ms;
        self
    }

    pub fn with_reconnect_interval_ms(mut self, interval_ms: u64) -> Self {
        self.options.reconnect_interval_ms = interval_ms;
        self
    }

    pub fn with_empty_batch_delay_ms(mut self, delay_ms: u64) -> Self {
        self.options.empty_batch_delay_ms = delay_ms;
        self
    }

    /// If true, the route exits gracefully once the source yields an empty
    /// batch (drain-then-exit). Off by default — routes normally poll forever.
    pub fn with_exit_on_empty(mut self, exit_on_empty: bool) -> Self {
        self.options.exit_on_empty = exit_on_empty;
        self
    }

    pub fn with_fault_injection(mut self, allow: bool) -> Self {
        self.options.allow_fault_injection = allow;
        self
    }

    pub fn with_handler(mut self, handler: impl Handler + 'static) -> Self {
        self.output.handler = Some(Arc::new(handler));
        self
    }

    /// Registers a typed handler for the route.
    ///
    /// The handler can accept either:
    /// - `fn(T) -> Future<Output = Result<Handled, HandlerError>>`
    /// - `fn(T, MessageContext) -> Future<Output = Result<Handled, HandlerError>>`
    ///
    /// # Examples
    ///
    /// ```
    /// # use mq_bridge::{Route, models::Endpoint};
    /// # use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct MyData { id: u32 }
    ///
    /// async fn my_handler(data: MyData) -> anyhow::Result<()> {
    ///     Ok(())
    /// }
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10))
    ///     .add_handler("my_type", my_handler);
    /// ```
    pub fn add_handler<T, H, Args>(mut self, type_name: &str, handler: H) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: crate::type_handler::IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
    {
        // Create the wrapper closure that handles deserialization and context extraction
        let handler = Arc::new(handler);
        let wrapper = move |msg: crate::CanonicalMessage| {
            let handler = handler.clone();
            async move {
                let data = msg.parse::<T>().map_err(|e| {
                    HandlerError::NonRetryable(anyhow::anyhow!("Deserialization failed: {}", e))
                })?;
                let ctx = crate::MessageContext::from(msg);
                handler.call(data, ctx).await
            }
        };
        let wrapper = Arc::new(wrapper);

        let prev_handler = self.output.handler.take();

        let new_handler = if let Some(h) = prev_handler {
            if let Some(extended) = h.register_handler(type_name, wrapper.clone()) {
                extended
            } else {
                Arc::new(
                    crate::type_handler::TypeHandler::new()
                        .with_fallback(h)
                        .add_handler(type_name, wrapper),
                )
            }
        } else {
            Arc::new(crate::type_handler::TypeHandler::new().add_handler(type_name, wrapper))
        };

        self.output.handler = Some(new_handler);
        self
    }
    pub fn add_handlers<T, H, Args>(mut self, handlers: HashMap<&str, H>) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: crate::type_handler::IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
    {
        for (type_name, handler) in handlers {
            self = self.add_handler(type_name, handler);
        }
        self
    }
}

type SequencerItem = (
    Vec<MessageDisposition>,
    BatchCommitFunc,
    tokio::sync::oneshot::Sender<anyhow::Result<()>>,
);

fn spawn_sequencer(buffer_size: usize) -> (Sender<(u64, SequencerItem)>, JoinHandle<()>) {
    let (seq_tx, seq_rx) = bounded::<(u64, SequencerItem)>(buffer_size);
    let sequencer_handle = tokio::spawn(async move {
        let mut buffer: BTreeMap<u64, SequencerItem> = BTreeMap::new();
        let mut next_seq = 0u64;

        loop {
            // If we have the next item in sequence, execute its commit directly.
            // Using a plain await (no select!) here is essential: if we raced a recv
            // against the commit future, a recv win would drop the commit future and
            // the notify sender, leaving the caller permanently blocked while next_seq
            // stays unadvanced — a deadlock.
            if let Some((dispositions, commit_func, notify)) = buffer.remove(&next_seq) {
                let result = commit_func(dispositions).await;
                let _ = notify.send(result);
                next_seq += 1;
                // Yield to allow other tasks to run, preventing busy-loop when buffer has many messages
                tokio::task::yield_now().await;
                continue;
            }

            // Wait for the next item from any worker.
            match seq_rx.recv().await {
                Ok((seq, item)) => {
                    if seq < next_seq {
                        let (_, _, notify) = item;
                        trace!(
                            seq,
                            next_seq,
                            "Sequencer received late item (seq < next_seq)"
                        );
                        let _ = notify.send(Err(anyhow::anyhow!(
                            "Sequencer received late item (seq {} < next_seq {})",
                            seq,
                            next_seq
                        )));
                    } else {
                        buffer.insert(seq, item);
                    }
                }
                Err(_) => {
                    // seq_tx was dropped — drain and notify any remaining buffered commits.
                    for (_, (_, _, notify)) in buffer {
                        let _ = notify.send(Err(anyhow::anyhow!("Sequencer is shutting down")));
                    }
                    break;
                }
            }
        }
    });
    (seq_tx, sequencer_handle)
}

fn wrap_commit(
    commit: BatchCommitFunc,
    seq: u64,
    seq_tx: Sender<(u64, SequencerItem)>,
) -> BatchCommitFunc {
    Box::new(move |dispositions| {
        Box::pin(async move {
            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
            if seq_tx
                .send((seq, (dispositions, commit, notify_tx)))
                .await
                .is_ok()
            {
                match notify_rx.await {
                    Ok(res) => res,
                    Err(_) => Err(anyhow::anyhow!(
                        "Sequencer dropped the commit channel unexpectedly"
                    )),
                }
            } else {
                Err(anyhow::anyhow!(
                    "Failed to send commit to sequencer, route is likely shutting down"
                ))
            }
        })
    })
}

/// Hands the right to call `send_batch` from one batch to the next when the sink
/// declares [`MessagePublisher::requires_ordered_publish`]. `prev` resolves once the
/// preceding batch's send returned (or its worker went away); dropping `_release` lets
/// the batch behind it in. Only the `send_batch` call is serialised — per-batch prep,
/// disposition mapping and commits still run concurrently across the worker pool.
struct OrderTicket {
    prev: Option<tokio::sync::oneshot::Receiver<()>>,
    _release: tokio::sync::oneshot::Sender<()>,
}

/// Routes batch commits either through the ordered sequencer (cumulative-ack
/// transports) or concurrently under a semaphore (individual-ack transports),
/// chosen once per route from the consumer's `commit_requires_order()`.
enum CommitRouter {
    Ordered {
        seq_tx: Sender<(u64, SequencerItem)>,
        handle: JoinHandle<()>,
    },
    Unordered {
        semaphore: Arc<tokio::sync::Semaphore>,
    },
}

impl CommitRouter {
    fn new(ordered: bool, commit_concurrency_limit: usize) -> Self {
        let commit_concurrency_limit = commit_concurrency_limit.max(1);
        if ordered {
            let (seq_tx, handle) = spawn_sequencer(commit_concurrency_limit);
            CommitRouter::Ordered { seq_tx, handle }
        } else {
            CommitRouter::Unordered {
                semaphore: Arc::new(tokio::sync::Semaphore::new(commit_concurrency_limit)),
            }
        }
    }

    /// Wraps a batch commit for its dispatch mode. `seq` is only used by the
    /// ordered path; it is ignored when commits run concurrently.
    fn wrap(&self, commit: BatchCommitFunc, seq: u64) -> BatchCommitFunc {
        match self {
            CommitRouter::Ordered { seq_tx, .. } => wrap_commit(commit, seq, seq_tx.clone()),
            // Unordered commits need no wrapping: the dispatcher acquires a
            // `commit_concurrency_limit` permit *before* spawning each commit
            // (see `dispatch_semaphore`), so backpressure applies at queue time
            // instead of letting blocked commit tasks pile up unbounded.
            CommitRouter::Unordered { .. } => commit,
        }
    }

    /// Semaphore that bounds how many unordered commits may be queued/in-flight at
    /// once. The dispatcher acquires a permit before spawning a commit task and
    /// holds it for the task's lifetime. `None` for the ordered path, whose
    /// bounded sequencer channel already limits outstanding commits.
    fn dispatch_semaphore(&self) -> Option<Arc<tokio::sync::Semaphore>> {
        match self {
            CommitRouter::Ordered { .. } => None,
            CommitRouter::Unordered { semaphore } => Some(Arc::clone(semaphore)),
        }
    }

    /// Tears down the sequencer (if any). Call only after all workers and their
    /// in-flight commit tasks have drained, so the sequencer's senders are gone.
    async fn shutdown(self) {
        if let CommitRouter::Ordered { seq_tx, handle } = self {
            drop(seq_tx);
            let _ = handle.await;
        }
    }
}

/// Acquires a commit-dispatch permit before a commit task is spawned, so the
/// caller blocks (applies backpressure) once `commit_concurrency_limit` commits
/// are already queued rather than spawning unbounded tasks. The returned permit is
/// held for the spawned task's lifetime. `None` (no semaphore, or a closed one
/// during shutdown) means the commit runs unbounded.
async fn acquire_commit_permit(
    semaphore: Option<&Arc<tokio::sync::Semaphore>>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match semaphore {
        Some(sem) => Arc::clone(sem).acquire_owned().await.ok(),
        None => None,
    }
}

fn map_responses_to_dispositions(
    message_ids: &[u128],
    responses: Option<Vec<crate::CanonicalMessage>>,
    failed: &[(crate::CanonicalMessage, PublisherError)],
    request_ids: &std::collections::HashSet<u128>,
    has_dlq_middleware: bool,
) -> Vec<MessageDisposition> {
    let len = message_ids.len();
    if responses.is_none() && failed.is_empty() && (len == 0 || request_ids.is_empty()) {
        return vec![MessageDisposition::Ack; len];
    }

    // Fast path for single message batches (very common for high-concurrency low-batch setups)
    if len == 1 {
        let id = message_ids[0];
        if let Some((_, error)) = failed.first() {
            return vec![
                if matches!(error, PublisherError::NonRetryable(_)) && !has_dlq_middleware {
                    MessageDisposition::Ack
                } else {
                    MessageDisposition::Nack
                },
            ];
        }
        if let Some(mut resps) = responses {
            if let Some(resp) = resps.pop() {
                return vec![MessageDisposition::Reply(resp)];
            }
        }
        if request_ids.contains(&id) {
            error!("Message {:032x} expected a reply (reply_to set), but publisher returned Ack. Nacking to avoid committing a lost response.", id);
            return vec![MessageDisposition::Nack];
        }
        return vec![MessageDisposition::Ack];
    }

    let mut dispositions = Vec::with_capacity(len);
    // Build failed_ids manually to avoid collect() overhead
    let mut failed_ids = std::collections::HashMap::with_capacity(failed.len());
    for (message, error) in failed {
        failed_ids.insert(message.message_id, error);
    }

    // Create a map from message_id to response message for efficient lookup.
    let mut response_map: std::collections::HashMap<u128, crate::CanonicalMessage> = responses
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.message_id, r))
        .collect();

    for id in message_ids {
        if let Some(error) = failed_ids.get(id) {
            dispositions.push(
                if matches!(error, PublisherError::NonRetryable(_)) && !has_dlq_middleware {
                    MessageDisposition::Ack
                } else {
                    MessageDisposition::Nack
                },
            );
        } else if let Some(resp) = response_map.remove(id) {
            // If a response exists for this specific ID, use it.
            dispositions.push(MessageDisposition::Reply(resp));
        } else if request_ids.contains(id) {
            error!("Message {:032x} expected a reply (reply_to set), but publisher returned Ack. Nacking to avoid committing a lost response.", id);
            dispositions.push(MessageDisposition::Nack);
        } else {
            // Otherwise, it was a successful send that did not produce a response.
            dispositions.push(MessageDisposition::Ack);
        }
    }
    dispositions
}

#[cfg(test)]
fn test_map_responses_to_dispositions_logic() {
    use crate::{traits::PublisherError, CanonicalMessage};
    use anyhow::anyhow;

    let ids = vec![1, 2, 3, 4];

    let mut resp1 = CanonicalMessage::from("resp1");
    resp1.message_id = 1;
    let mut resp4 = CanonicalMessage::from("resp4");
    resp4.message_id = 4;

    let responses = Some(vec![
        resp1, // Corresponds to id 1
        resp4, // Corresponds to id 4
    ]);

    let mut msg2 = CanonicalMessage::from("msg2");
    msg2.message_id = 2;
    let failed = vec![(msg2, PublisherError::NonRetryable(anyhow!("failed")))];

    let mut request_ids = std::collections::HashSet::new();
    request_ids.insert(3); // id 3 expects a reply but won't get one
    let dispositions = map_responses_to_dispositions(&ids, responses, &failed, &request_ids, false);

    assert_eq!(dispositions.len(), 4);
    assert!(matches!(dispositions[0], MessageDisposition::Reply(_))); // from responses
    assert!(matches!(dispositions[1], MessageDisposition::Ack)); // permanent failure dropped
    assert!(matches!(dispositions[2], MessageDisposition::Nack)); // missing reply
    assert!(matches!(dispositions[3], MessageDisposition::Reply(_))); // from responses

    let mut dlq_msg = CanonicalMessage::from("msg2");
    dlq_msg.message_id = 2;
    let dlq_failed = vec![(
        dlq_msg,
        PublisherError::NonRetryable(anyhow!("DLQ send failed")),
    )];
    let dlq_dispositions =
        map_responses_to_dispositions(&ids, None, &dlq_failed, &request_ids, true);
    assert!(matches!(dlq_dispositions[1], MessageDisposition::Nack));
}

pub fn get_route(name: &str) -> Option<Route> {
    Route::get(name)
}

pub fn list_routes() -> Vec<String> {
    Route::list()
}

/// Returns why a deployed route terminated, or `None` while it is still running
/// (or if nothing is deployed under `name`).
///
/// [`Route::deploy`] keeps its [`RouteHandle`] private, so this is how a caller
/// that deployed by name — the language bindings, a supervisor — learns that a
/// drain-then-exit route finished on its own rather than waiting on a `stop()`
/// that never comes.
pub fn route_outcome(name: &str) -> Option<RouteOutcome> {
    let registry = ROUTE_REGISTRY.get()?;
    let map = recover_read_lock(registry, "route_registry");
    map.get(name)?.handle.outcome()
}

/// Returns the connection health of a deployed route, or `None` if nothing is
/// deployed under `name`. Pairs with [`route_outcome`]: after a
/// [`RouteOutcome::Failed`], `error` holds the cause.
pub fn route_status(name: &str) -> Option<EndpointStatus> {
    let registry = ROUTE_REGISTRY.get()?;
    let map = recover_read_lock(registry, "route_registry");
    Some(map.get(name)?.handle.status())
}

pub async fn stop_route(name: &str) -> bool {
    Route::stop(name).await
}

#[cfg(test)]
mod tests {

    /// The publisher opens before the consumer, and a `source_position` file sink creates its
    /// part directory as it opens. A route rejected for its input must not leave that behind
    /// where the operator asked for a file.
    #[tokio::test]
    async fn a_rejected_source_position_file_sink_leaves_no_directory_behind() {
        let dir = tempfile::tempdir().unwrap();
        let sink = dir.path().join("out.jsonl");
        let route = Route {
            input: Endpoint::new(EndpointType::Memory(MemoryConfig::new("probe-in", Some(1)))),
            output: Endpoint::new(EndpointType::File(
                FileConfig::new(sink.to_str().unwrap()).with_name_by(NameBy::SourcePosition),
            )),
            options: RouteOptions::default(),
        };
        let (_tx, rx) = async_channel::bounded(1);
        let error = route
            .run_sequentially("probe", rx, None, None, false)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("replay position"),
            "unexpected error: {error}"
        );
        assert!(
            !sink.is_dir(),
            "the failing route left {sink:?} behind as a directory"
        );
    }
    use super::*;
    use crate::models::{
        Endpoint, EndpointType, FaultMode, FileConfig, MemoryConfig, Middleware, MongoDbConfig,
        NameBy, RandomPanicMiddleware, RouteOptions, SqlxConfig,
    };
    use crate::traits::{
        CustomMiddlewareFactory, MessageConsumer, MessagePublisher, ReceivedBatch,
    };
    use crate::CanonicalMessage;
    use std::any::Any;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn completed_route_clears_stale_connection_status() {
        for error in ["connecting", "temporary disconnect"] {
            let outcome = Arc::new(RwLock::new(None));
            let status = Arc::new(RwLock::new(EndpointStatus {
                healthy: false,
                error: Some(error.to_string()),
                ..Default::default()
            }));

            {
                let mut guard = OutcomeGuard {
                    outcome: Arc::clone(&outcome),
                    status: Arc::clone(&status),
                    drops: Arc::new(RwLock::new(DropReport::default())),
                    resolved: None,
                };
                guard.set(RouteOutcome::Completed);
            }

            let status = recover_read_lock(&status, "test_route_handle_status");
            assert!(status.healthy);
            assert_eq!(status.error, None);
            assert_eq!(
                *recover_read_lock(&outcome, "test_route_handle_outcome"),
                Some(RouteOutcome::Completed)
            );
        }
    }

    #[test]
    fn infers_idempotent_sink_mechanisms() {
        let mongo = Route::new(
            Endpoint::new_memory("delivery_in", 1),
            Endpoint::new(EndpointType::MongoDb(MongoDbConfig {
                id_field: Some("${metadata:mqb.id}".to_string()),
                ..Default::default()
            })),
        );
        assert_eq!(
            mongo.inferred_idempotency_mechanism(&mongo.output),
            Some("MongoDB unique _id")
        );

        let sql = Route::new(
            Endpoint::new_memory("delivery_in", 1),
            Endpoint::new(EndpointType::Sqlx(SqlxConfig {
                insert_query: Some(
                    "INSERT INTO orders (id) VALUES (?) ON CONFLICT (id) DO NOTHING".to_string(),
                ),
                ..Default::default()
            })),
        );
        assert_eq!(
            sql.inferred_idempotency_mechanism(&sql.output),
            Some("SQL unique-key conflict handling")
        );

        for query in [
            "INSERT INTO orders (id) VALUES (?) ON CONFLICT (id) DO UPDATE SET id = excluded.id",
            "INSERT INTO orders (id) VALUES (?) ON DUPLICATE KEY UPDATE id = VALUES(id)",
        ] {
            let updating_sql = Route::new(
                Endpoint::new_memory("delivery_in", 1),
                Endpoint::new(EndpointType::Sqlx(SqlxConfig {
                    insert_query: Some(query.to_string()),
                    ..Default::default()
                })),
            );
            assert_eq!(
                updating_sql.inferred_idempotency_mechanism(&updating_sql.output),
                None
            );
        }

        let ordinary = Route::new(
            Endpoint::new_memory("delivery_in", 1),
            Endpoint::new_memory("delivery_out", 1),
        );
        assert_eq!(
            ordinary.inferred_idempotency_mechanism(&ordinary.output),
            None
        );
    }

    /// The object-store sink reports the mechanism it derived, so `name_by: auto` has to be
    /// resolved against the route's input rather than read off the config.
    #[cfg(feature = "object-store")]
    #[test]
    fn auto_named_object_store_reports_the_mechanism_its_input_earns() {
        fn route_from(input: Endpoint) -> Route {
            Route::new(
                input,
                Endpoint::new(EndpointType::ObjectStore(
                    crate::models::ObjectStoreConfig {
                        url: "s3://bucket/data".to_string(),
                        ..Default::default()
                    },
                )),
            )
        }

        let replayable = route_from(Endpoint::new(EndpointType::File(
            crate::models::FileConfig::new("/tmp/orders.csv"),
        )));
        assert_eq!(
            replayable.inferred_idempotency_mechanism(&replayable.output),
            Some("object names carrying the source range")
        );

        // Memory carries no replay position, so `auto` falls back and claims nothing.
        let positionless = route_from(Endpoint::new_memory("delivery_in", 1));
        assert_eq!(
            positionless.inferred_idempotency_mechanism(&positionless.output),
            None
        );

        let filtered = route_from(Endpoint {
            middlewares: vec![Middleware::Filter("amount > 100".to_string())],
            ..Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
                "/tmp/orders.csv",
            )))
        });
        let (relaxed, _) = relax_object_naming(
            "delivery",
            filtered.source_has_position(),
            &filtered.input,
            &filtered.output,
        )
        .unwrap()
        .expect("filter relaxes object naming");
        assert_eq!(filtered.inferred_idempotency_mechanism(&relaxed), None);
    }

    /// A `ref` may name another `ref`. Resolving only the first hop made a chained input look
    /// positionless, which silently demoted `name_by: auto` to `write_time` — the sink stopped
    /// recognising a replay rather than failing where anyone would notice.
    #[test]
    fn a_chained_ref_input_resolves_to_the_endpoint_at_the_end_of_it() {
        fn route_with_input(name: &str) -> Route {
            Route::new(
                Endpoint::new(EndpointType::Ref(name.to_string())),
                Endpoint::new_memory("chain_out", 1),
            )
        }

        register_endpoint(
            "chain-file",
            Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
                "/tmp/chain.csv",
            ))),
        );
        register_endpoint(
            "chain-middle",
            Endpoint::new(EndpointType::Ref("chain-file".to_string())),
        );
        register_endpoint(
            "chain-outer",
            Endpoint::new(EndpointType::Ref("chain-middle".to_string())),
        );
        assert!(route_with_input("chain-outer").source_has_position());

        // Unresolvable chains keep the old answer: no position, so `auto` falls back.
        assert!(!route_with_input("chain-missing").source_has_position());
        register_endpoint(
            "chain-loop",
            Endpoint::new(EndpointType::Ref("chain-loop".to_string())),
        );
        assert!(!route_with_input("chain-loop").source_has_position());

        // A chain ending somewhere positionless still reports none.
        register_endpoint("chain-memory", Endpoint::new_memory("chain_in", 1));
        register_endpoint(
            "chain-to-memory",
            Endpoint::new(EndpointType::Ref("chain-memory".to_string())),
        );
        assert!(!route_with_input("chain-to-memory").source_has_position());
    }

    #[test]
    fn test_route_check_rejects_zero_execution_options() {
        let route = Route::new(
            Endpoint::new_memory("zero_in", 10),
            Endpoint::new_memory("zero_out", 10),
        )
        .with_options(RouteOptions {
            concurrency: 0,
            batch_size: 1,
            commit_concurrency_limit: 1,
            ..Default::default()
        });

        let err = route.check("zero_options", None).unwrap_err().to_string();
        assert!(err.contains("concurrency must be at least 1"));
    }

    #[test]
    fn test_route_check_warns_when_buffered_writes_are_concurrent() {
        let suffix = fast_uuid_v7::gen_id_str();
        let buffered_name = format!("buffer_warning_leaf_{suffix}");
        register_endpoint(
            &buffered_name,
            Endpoint::new_memory("buffer_warning_out", 10).add_middleware(Middleware::Buffer(
                crate::models::BufferMiddleware {
                    max_messages: 100,
                    max_delay_ms: 10,
                },
            )),
        );
        let output = Endpoint::new(EndpointType::Fanout(vec![Endpoint::new(
            EndpointType::Ref(buffered_name),
        )]));
        let route =
            Route::new(Endpoint::new_memory("buffer_warning_in", 10), output).with_concurrency(4);

        let warnings = route.check("buffer_warning", None).unwrap();
        assert!(warnings.iter().any(|warning| {
            warning.contains("concurrent destination writes") && warning.contains("concurrency: 1")
        }));
    }

    #[test]
    fn buffer_detection_stops_at_reference_cycles() {
        let suffix = fast_uuid_v7::gen_id_str();
        let first = format!("buffer_cycle_first_{suffix}");
        let second = format!("buffer_cycle_second_{suffix}");
        register_endpoint(&first, Endpoint::new(EndpointType::Ref(second.clone())));
        register_endpoint(&second, Endpoint::new(EndpointType::Ref(first.clone())));

        assert!(!endpoint_tree_has_buffer(
            &Endpoint::new(EndpointType::Ref(first)),
            &mut HashSet::new(),
        ));
    }

    #[test]
    fn test_random_panic_requires_fault_injection_opt_in() {
        let fault_config = RandomPanicMiddleware {
            mode: FaultMode::Timeout,
            enabled: true,
            ..Default::default()
        };
        let input = Endpoint::new_memory("fault_policy_in", 10)
            .add_middleware(Middleware::RandomPanic(fault_config));
        let output = Endpoint::new_memory("fault_policy_out", 10);
        let route = Route::new(input, output);

        let err = route.check("fault_policy", None).unwrap_err().to_string();
        assert!(err.contains("allow_fault_injection"));
        assert!(route
            .with_fault_injection(true)
            .check("fault_policy", None)
            .is_ok());
    }

    #[derive(Debug, Default)]
    struct CommitObservation {
        completed: Mutex<Vec<u64>>,
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
    }

    #[derive(Debug)]
    struct CommitTrackingMiddlewareFactory {
        observation: Arc<CommitObservation>,
        // Drives the wrapped consumer's `commit_requires_order()`, letting one
        // factory exercise both the ordered (sequencer) and unordered (semaphore)
        // commit paths.
        requires_order: bool,
    }

    #[derive(Debug)]
    struct ReorderingPublisherMiddlewareFactory;

    struct CommitTrackingConsumer {
        inner: Box<dyn MessageConsumer>,
        observation: Arc<CommitObservation>,
        requires_order: bool,
    }

    struct ReorderingPublisher {
        inner: Box<dyn MessagePublisher>,
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for CommitTrackingMiddlewareFactory {
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(Box::new(CommitTrackingConsumer {
                inner: consumer,
                observation: Arc::clone(&self.observation),
                requires_order: self.requires_order,
            }))
        }
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for ReorderingPublisherMiddlewareFactory {
        async fn apply_publisher(
            &self,
            publisher: Box<dyn MessagePublisher>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(ReorderingPublisher { inner: publisher }))
        }
    }

    #[async_trait::async_trait]
    impl MessageConsumer for CommitTrackingConsumer {
        fn commit_requires_order(&self) -> bool {
            self.requires_order
        }

        async fn receive_batch(
            &mut self,
            max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            let mut batch = self.inner.receive_batch(max_messages).await?;
            let seq = batch
                .messages
                .first()
                .and_then(|message| message.get_payload_str().parse::<u64>().ok())
                .expect("tracking test expects numeric payloads");
            let original_commit = batch.commit;
            let observation = Arc::clone(&self.observation);
            batch.commit = Box::new(move |dispositions| {
                let observation = Arc::clone(&observation);
                Box::pin(async move {
                    let active_now = observation.active.fetch_add(1, Ordering::SeqCst) + 1;
                    let _ = observation.max_active.fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |current| (active_now > current).then_some(active_now),
                    );

                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let result = original_commit(dispositions).await;
                    observation.completed.lock().unwrap().push(seq);
                    observation.active.fetch_sub(1, Ordering::SeqCst);
                    result
                })
            });
            Ok(batch)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl MessagePublisher for ReorderingPublisher {
        async fn send_batch(
            &self,
            messages: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let seq = messages
                .first()
                .and_then(|message| message.get_payload_str().parse::<u64>().ok())
                .expect("tracking test expects numeric payloads");
            let delay_ms = 10 * (6u64.saturating_sub(seq.min(6)));
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            self.inner.send_batch(messages).await
        }

        async fn send(&self, msg: crate::CanonicalMessage) -> Result<Sent, PublisherError> {
            self.inner.send(msg).await
        }

        async fn flush(&self) -> anyhow::Result<()> {
            self.inner.flush().await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    async fn assert_route_commits_are_ordered_and_non_overlapping(concurrency: usize) {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let tracking_name = format!("track_commit_{}", unique_id);
        let reorder_name = format!("reorder_publish_{}", unique_id);
        let in_topic = format!("ordered_commit_in_{}", unique_id);
        let observation = Arc::new(CommitObservation::default());

        register_middleware_factory(
            &tracking_name,
            Arc::new(CommitTrackingMiddlewareFactory {
                observation: Arc::clone(&observation),
                requires_order: true,
            }),
        )
        .unwrap();
        register_middleware_factory(
            &reorder_name,
            Arc::new(ReorderingPublisherMiddlewareFactory),
        )
        .unwrap();

        let input = Endpoint::new_memory(&in_topic, 32).add_middleware(Middleware::Custom {
            name: tracking_name,
            config: serde_json::Value::Null,
        });
        let output = Endpoint::new(EndpointType::Null).add_middleware(Middleware::Custom {
            name: reorder_name,
            config: serde_json::Value::Null,
        });

        let route = Route::new(input.clone(), output)
            .with_concurrency(concurrency)
            .with_batch_size(1)
            .with_commit_concurrency_limit(1);

        let input_channel = input.channel().unwrap();
        let messages = (0..6)
            .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
            .collect();
        input_channel.fill_messages(messages).await.unwrap();
        input_channel.close();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            route.run_until_err("ordered_commit_regression", None, None),
        )
        .await
        .expect("Route should not hang while draining finite input")
        .expect("Route should complete without commit errors");
        assert_eq!(
            *observation.completed.lock().unwrap(),
            vec![0, 1, 2, 3, 4, 5],
            "Commit execution must follow receive order",
        );
        assert_eq!(
            observation.max_active.load(Ordering::SeqCst),
            1,
            "Broker-facing commit functions must never overlap",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sequential_route_commits_are_ordered_and_non_overlapping() {
        assert_route_commits_are_ordered_and_non_overlapping(1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_route_commits_are_ordered_and_non_overlapping() {
        assert_route_commits_are_ordered_and_non_overlapping(4).await;
    }

    #[derive(Debug, Default)]
    struct PublishObservation {
        /// Batch sequence numbers in the order their `send_batch` completed.
        arrived: Mutex<Vec<u64>>,
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
    }

    #[derive(Debug)]
    struct OrderedPublishMiddlewareFactory {
        observation: Arc<PublishObservation>,
        /// Drives the wrapped publisher's `requires_ordered_publish()`, so one factory
        /// exercises both an order-sensitive sink (file, object_store) and a plain one.
        requires_order: bool,
    }

    struct OrderedPublishTracker {
        inner: Box<dyn MessagePublisher>,
        observation: Arc<PublishObservation>,
        requires_order: bool,
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for OrderedPublishMiddlewareFactory {
        async fn apply_publisher(
            &self,
            publisher: Box<dyn MessagePublisher>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(OrderedPublishTracker {
                inner: publisher,
                observation: Arc::clone(&self.observation),
                requires_order: self.requires_order,
            }))
        }
    }

    #[async_trait::async_trait]
    impl MessagePublisher for OrderedPublishTracker {
        fn requires_ordered_publish(&self) -> bool {
            self.requires_order
        }

        async fn send_batch(
            &self,
            messages: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let seq = messages
                .first()
                .and_then(|message| message.get_payload_str().parse::<u64>().ok())
                .expect("tracking test expects numeric payloads");
            let active_now = self.observation.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = self.observation.max_active.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |current| (active_now > current).then_some(active_now),
            );
            // Later batches are faster, so unsequenced sends land in reverse order.
            tokio::time::sleep(Duration::from_millis(
                10 * (6u64.saturating_sub(seq.min(6))),
            ))
            .await;
            let result = self.inner.send_batch(messages).await;
            self.observation.arrived.lock().unwrap().push(seq);
            self.observation.active.fetch_sub(1, Ordering::SeqCst);
            result
        }

        async fn flush(&self) -> anyhow::Result<()> {
            self.inner.flush().await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    async fn run_publish_order_route(requires_order: bool) -> Arc<PublishObservation> {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let tracking_name = format!("track_publish_{}", unique_id);
        let in_topic = format!("publish_order_in_{}", unique_id);
        let observation = Arc::new(PublishObservation::default());

        register_middleware_factory(
            &tracking_name,
            Arc::new(OrderedPublishMiddlewareFactory {
                observation: Arc::clone(&observation),
                requires_order,
            }),
        )
        .unwrap();

        let input = Endpoint::new_memory(&in_topic, 32);
        let output = Endpoint::new(EndpointType::Null).add_middleware(Middleware::Custom {
            name: tracking_name,
            config: serde_json::Value::Null,
        });

        let route = Route::new(input.clone(), output)
            .with_concurrency(4)
            .with_batch_size(1);

        let input_channel = input.channel().unwrap();
        let messages = (0..6)
            .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
            .collect();
        input_channel.fill_messages(messages).await.unwrap();
        input_channel.close();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            route.run_until_err("publish_order_test", None, None),
        )
        .await
        .expect("Route should not hang while draining finite input")
        .expect("Route should complete without errors");

        observation
    }

    // Regression (issue #71): with `concurrency > 1` the workers used to call `send_batch`
    // in parallel, so whole batches reached the sink out of source order — silently
    // shuffling a file/object_store export. A sink that declares
    // `requires_ordered_publish()` now gets its sends sequenced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_order_sensitive_sink_receives_batches_in_source_order() {
        let observation = run_publish_order_route(true).await;

        assert_eq!(
            *observation.arrived.lock().unwrap(),
            vec![0, 1, 2, 3, 4, 5],
            "An order-sensitive sink must see batches in source order",
        );
        assert_eq!(
            observation.max_active.load(Ordering::SeqCst),
            1,
            "Sequenced sends must never overlap",
        );
    }

    // The default stays unordered so concurrent sinks keep their throughput: batches may
    // arrive in any order, but every one of them must arrive exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_unordered_sink_still_receives_every_batch() {
        let observation = run_publish_order_route(false).await;

        let mut arrived = observation.arrived.lock().unwrap().clone();
        arrived.sort_unstable();
        assert_eq!(arrived, vec![0, 1, 2, 3, 4, 5]);
    }

    /// Drives a route whose consumer reports `commit_requires_order() == false`
    /// and returns what the commit tracker observed. Each commit sleeps 20ms, so
    /// overlapping commits push `max_active` above 1.
    async fn run_unordered_commit_route(
        concurrency: usize,
        commit_concurrency_limit: usize,
        message_count: u64,
    ) -> Arc<CommitObservation> {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let tracking_name = format!("track_unordered_{}", unique_id);
        let in_topic = format!("unordered_commit_in_{}", unique_id);
        let observation = Arc::new(CommitObservation::default());

        register_middleware_factory(
            &tracking_name,
            Arc::new(CommitTrackingMiddlewareFactory {
                observation: Arc::clone(&observation),
                requires_order: false,
            }),
        )
        .unwrap();

        let input = Endpoint::new_memory(&in_topic, 64).add_middleware(Middleware::Custom {
            name: tracking_name,
            config: serde_json::Value::Null,
        });
        let output = Endpoint::new(EndpointType::Null);

        let route = Route::new(input.clone(), output)
            .with_concurrency(concurrency)
            .with_batch_size(1)
            .with_commit_concurrency_limit(commit_concurrency_limit);

        let input_channel = input.channel().unwrap();
        let messages = (0..message_count)
            .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
            .collect();
        input_channel.fill_messages(messages).await.unwrap();
        input_channel.close();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            route.run_until_err("unordered_commit_test", None, None),
        )
        .await
        .expect("Route should not hang while draining finite input")
        .expect("Route should complete without commit errors");

        observation
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_unordered_commits_run_concurrently_without_loss() {
        let observation = run_unordered_commit_route(4, 4, 6).await;

        // Every batch must still be committed exactly once (no data loss), even
        // though completion order is not guaranteed.
        let mut completed = observation.completed.lock().unwrap().clone();
        completed.sort_unstable();
        assert_eq!(
            completed,
            (0..6).collect::<Vec<u64>>(),
            "Every message must be committed exactly once",
        );

        // The whole point of the unordered path: commits overlap instead of being
        // serialized one-at-a-time by the sequencer.
        assert!(
            observation.max_active.load(Ordering::SeqCst) > 1,
            "Unordered commits must run concurrently (max_active was {})",
            observation.max_active.load(Ordering::SeqCst),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_unordered_commits_respect_concurrency_limit() {
        // commit_concurrency_limit caps how many commits run at once even when the
        // route concurrency is higher.
        let observation = run_unordered_commit_route(4, 2, 8).await;

        let mut completed = observation.completed.lock().unwrap().clone();
        completed.sort_unstable();
        assert_eq!(completed, (0..8).collect::<Vec<u64>>());

        assert!(
            observation.max_active.load(Ordering::SeqCst) <= 2,
            "Concurrent commits must stay within commit_concurrency_limit (max_active was {})",
            observation.max_active.load(Ordering::SeqCst),
        );
    }

    // Helper function to run a fault injection test on the consumer side.
    async fn run_consumer_fault_test(
        mode: FaultMode,
        expected_payload: &str,
        route_should_restart: bool,
        concurrency: usize,
    ) {
        let unique_suffix = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("fault_in_{}_{}_{}", mode, concurrency, unique_suffix);
        let out_topic = format!("fault_out_{}_{}_{}", mode, concurrency, unique_suffix);

        let fault_config = RandomPanicMiddleware {
            mode,
            trigger_on_message: Some(1), // Panic on the first message
            enabled: true,
            ..Default::default()
        };

        let input = Endpoint::new_memory(&in_topic, 10)
            .add_middleware(Middleware::RandomPanic(fault_config));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route_name = format!("fault_test_{}_{}", mode, concurrency);
        let route = Route::new(input.clone(), output.clone())
            .with_concurrency(concurrency)
            .with_fault_injection(true)
            .with_reconnect_interval_ms(100);

        // Start the route
        route
            .deploy(&route_name)
            .await
            .expect("Failed to deploy route");
        // Send a message. The consumer will inject a fault when it tries to receive it.
        let input_ch = input.channel().unwrap();
        input_ch
            .send_message("persistent_msg".into())
            .await
            .unwrap();

        if route_should_restart {
            // The route's worker will fail, then the supervisor will restart it.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        } else {
            // Route doesn't restart, just wait a bit for the (faulty) message to pass through.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Verify the outcome.
        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for message after fault")
            .expect("Stream closed while waiting for message");

        assert_eq!(received.message.get_payload_str(), expected_payload);
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        // Cleanup
        Route::stop(&route_name).await;
    }

    // Helper function to run a fault injection test on the publisher side.
    async fn run_publisher_fault_test(
        mode: FaultMode,
        expected_payload: &str,
        route_should_restart: bool,
    ) {
        let unique_suffix = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("pub_fault_in_{}_{}", mode, unique_suffix);
        let out_topic = format!("pub_fault_out_{}_{}", mode, unique_suffix);

        let fault_config = RandomPanicMiddleware {
            mode,
            trigger_on_message: Some(1), // Trigger on the first message
            enabled: true,
            ..Default::default()
        };

        let mut input = Endpoint::new_memory(&in_topic, 10);
        // Enable NACK on input so messages aren't lost when publisher crashes
        if let EndpointType::Memory(ref mut cfg) = input.endpoint_type {
            cfg.enable_nack = true;
        }
        // Apply fault middleware to output
        let output = Endpoint::new_memory(&out_topic, 10)
            .add_middleware(Middleware::RandomPanic(fault_config));

        let route_name = format!("pub_fault_test_{}", mode);
        let route = Route::new(input.clone(), output.clone())
            .with_fault_injection(true)
            .with_reconnect_interval_ms(100);

        route
            .deploy(&route_name)
            .await
            .expect("Failed to deploy route");

        let input_ch = input.channel().unwrap();
        input_ch
            .send_message(expected_payload.into())
            .await
            .unwrap();

        if route_should_restart {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for message after publisher fault")
            .expect("Stream closed");

        assert_eq!(received.message.get_payload_str(), expected_payload);
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        Route::stop(&route_name).await;
    }

    // No Docker. Ignored to keep default `cargo test` focused on the fast path.
    // Locally measured on 2026-06-21: this plus the sequential variant ran in ~2s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_route_recovery_from_faults() {
        let original_payload = "persistent_msg";

        // Test with concurrency > 1
        run_consumer_fault_test(FaultMode::Panic, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Timeout, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Nack, original_payload, true, 2).await;

        // This fault replaces the message but does not restart the route.
        run_consumer_fault_test(FaultMode::JsonFormatError, "{invalid json}", false, 2).await;
    }

    // No Docker. Run with `cargo test test_route_recovery_from_faults -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_route_recovery_from_faults_sequential() {
        let original_payload = "persistent_msg";

        // Test with concurrency = 1
        run_consumer_fault_test(FaultMode::Panic, original_payload, true, 1).await;
        run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 1).await;
    }

    // No Docker. Locally measured on 2026-06-21 at ~1s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_publisher_recovery_from_faults() {
        let original_payload = "persistent_msg";
        // Test publisher-side faults causing restart/retry.
        // `FaultMode::Panic` is not tested here because the `MemoryConsumer` used for input
        // does not support crash-safe at-least-once delivery. A panic in the publisher
        // worker would cause the in-flight message to be lost.
        run_publisher_fault_test(FaultMode::Disconnect, original_payload, true).await;
        run_publisher_fault_test(FaultMode::Timeout, original_payload, true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_route_sequencer_deadlock_fix() {
        // This test ensures that when a worker fails to send a batch (and thus drops the commit handle),
        // the sequencer doesn't deadlock waiting for that sequence number.
        // The fix ensures that even on failure, the commit function is called (with Nack) to fill the sequence gap.

        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("fail_factory_{}", unique_id);
        let in_topic = format!("deadlock_in_{}", unique_id);
        let out_topic = format!("deadlock_out_{}", unique_id);

        #[derive(Debug)]
        struct FailingMiddlewareFactory {
            fail_flag: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl CustomMiddlewareFactory for FailingMiddlewareFactory {
            async fn apply_publisher(
                &self,
                publisher: Box<dyn MessagePublisher>,
                _route_name: &str,
                _config: &serde_json::Value,
            ) -> anyhow::Result<Box<dyn MessagePublisher>> {
                Ok(Box::new(FailingPublisher {
                    inner: publisher,
                    fail_flag: self.fail_flag.clone(),
                }))
            }
            async fn apply_consumer(
                &self,
                consumer: Box<dyn MessageConsumer>,
                _route_name: &str,
                _config: &serde_json::Value,
            ) -> anyhow::Result<Box<dyn MessageConsumer>> {
                Ok(consumer)
            }
        }

        struct FailingPublisher {
            inner: Box<dyn MessagePublisher>,
            fail_flag: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl MessagePublisher for FailingPublisher {
            async fn send_batch(
                &self,
                messages: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                // We want to fail one batch to trigger the error path in the worker.
                // We use compare_exchange to ensure only one failure happens.
                if self
                    .fail_flag
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "Simulated failure"
                    )));
                }
                // Add a small delay for successful batches to ensure the failed one (if it created a gap)
                // would block the sequencer if the gap wasn't filled.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                self.inner.send_batch(messages).await
            }
            async fn send(
                &self,
                msg: crate::CanonicalMessage,
            ) -> Result<crate::traits::Sent, PublisherError> {
                self.inner.send(msg).await
            }
            async fn flush(&self) -> anyhow::Result<()> {
                self.inner.flush().await
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let fail_flag = Arc::new(AtomicBool::new(true));
        register_middleware_factory(
            &factory_name,
            Arc::new(FailingMiddlewareFactory {
                fail_flag: fail_flag.clone(),
            }),
        )
        .unwrap();

        let input = Endpoint::new_memory(&in_topic, 100);
        let output = Endpoint::new_memory(&out_topic, 100).add_middleware(Middleware::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        });

        // Concurrency > 1 is required to have multiple workers and potential out-of-order completion
        let route = Route::new(input.clone(), output.clone())
            .with_concurrency(2)
            .with_batch_size(1);

        // Send messages
        let input_ch = input.channel().unwrap();
        input_ch.send_message("msg1".into()).await.unwrap();
        input_ch.send_message("msg2".into()).await.unwrap();
        input_ch.send_message("msg3".into()).await.unwrap();

        // Run the route. It should fail eventually due to the simulated error,
        // but it MUST NOT deadlock.
        let run_fut = async {
            let (_shutdown_tx, shutdown_rx) = async_channel::bounded(1);
            route
                .run_until_err("deadlock_test", Some(shutdown_rx), None)
                .await
        };

        // If deadlock exists, this timeout will trigger.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_fut).await;

        match result {
            Ok(res) => {
                // We expect an error because the publisher returns Err.
                assert!(
                    res.is_err(),
                    "Route should have failed with simulated error"
                );
            }
            Err(_) => {
                panic!("Route deadlocked! The sequencer likely didn't receive the Nack for the failed batch.");
            }
        }
    }

    #[tokio::test]
    async fn test_sequencer_ordered_commits() {
        use std::time::Duration;
        use tokio::time::timeout;

        let (seq_tx, sequencer_handle) = spawn_sequencer(16);
        let processed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        // Send sequences out of order to ensure the sequencer enforces ordering.
        let seqs = [2u64, 0u64, 1u64, 3u64];
        let mut receivers = Vec::new();

        for seq in seqs.iter().cloned() {
            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
            let processed_clone = processed.clone();
            let commit: BatchCommitFunc = Box::new(move |_dispositions| {
                let processed = processed_clone.clone();
                Box::pin(async move {
                    // Simulate variable work durations
                    tokio::time::sleep(Duration::from_millis(10 * seq)).await;
                    processed.lock().unwrap().push(seq);
                    Ok(())
                })
            });
            seq_tx
                .send((seq, (Vec::new(), commit, notify_tx)))
                .await
                .unwrap();
            receivers.push(notify_rx);
        }

        // Wait for all commits to complete (with timeout to catch deadlocks)
        for rx in receivers {
            let res = timeout(Duration::from_secs(2), rx)
                .await
                .expect("Sequencer notify timed out");
            assert!(res.is_ok(), "Sequencer reported an error on commit");
            assert!(res.unwrap().is_ok(), "Commit returned an error");
        }

        // Close sender to allow sequencer task to exit and await it.
        drop(seq_tx);
        let _ = sequencer_handle.await;

        let result = processed.lock().unwrap().clone();
        assert_eq!(
            result,
            vec![0u64, 1u64, 2u64, 3u64],
            "Sequencer must process commits in order"
        );
    }

    #[tokio::test]
    async fn test_sequencer_shutdown_notifies_pending() {
        use std::time::Duration;
        use tokio::time::timeout;

        let (seq_tx, sequencer_handle) = spawn_sequencer(8);

        // Prepare two pending items for sequences 1 and 2 while sequence 0 is missing.
        let (notify_tx1, notify_rx1) = tokio::sync::oneshot::channel();
        let (notify_tx2, notify_rx2) = tokio::sync::oneshot::channel();

        let commit1: BatchCommitFunc = Box::new(|_dispositions| {
            Box::pin(async move {
                // Should not be executed because next_seq is missing (0)
                panic!("Commit should not be executed during shutdown drain");
                #[allow(unreachable_code)]
                Ok(())
            })
        });

        let commit2: BatchCommitFunc = Box::new(|_dispositions| {
            Box::pin(async move {
                panic!("Commit should not be executed during shutdown drain");
                #[allow(unreachable_code)]
                Ok(())
            })
        });

        seq_tx
            .send((1u64, (Vec::new(), commit1, notify_tx1)))
            .await
            .unwrap();
        seq_tx
            .send((2u64, (Vec::new(), commit2, notify_tx2)))
            .await
            .unwrap();

        // Trigger shutdown of the sequencer by dropping the sender.
        drop(seq_tx);

        // Sequencer should drain buffered items and reply with an error to the notifiers.
        let r1 = timeout(Duration::from_secs(1), notify_rx1)
            .await
            .expect("Timeout waiting for notify_rx1")
            .expect("Sequencer closed notify channel");
        assert!(
            r1.is_err(),
            "Pending commit should receive Err on sequencer shutdown"
        );

        let r2 = timeout(Duration::from_secs(1), notify_rx2)
            .await
            .expect("Timeout waiting for notify_rx2")
            .expect("Sequencer closed notify channel");
        assert!(
            r2.is_err(),
            "Pending commit should receive Err on sequencer shutdown"
        );

        let _ = sequencer_handle.await;
    }

    use crate::traits::{BoxFuture, CustomEndpointFactory, Sent};
    use std::sync::Mutex;

    type ConsumerBehavior =
        Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessageConsumer>, anyhow::Error> + Send + Sync>>;
    type PublisherBehavior =
        Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessagePublisher>, anyhow::Error> + Send + Sync>>;

    struct MockEndpointFactory {
        create_consumer_fail: bool,
        consumer_behavior: ConsumerBehavior,
        publisher_behavior: PublisherBehavior,
    }

    impl std::fmt::Debug for MockEndpointFactory {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockEndpointFactory")
                .field("create_consumer_fail", &self.create_consumer_fail)
                .finish()
        }
    }

    impl MockEndpointFactory {
        fn new() -> Self {
            Self {
                create_consumer_fail: false,
                consumer_behavior: Arc::new(Mutex::new(|| Err(anyhow::anyhow!("Not implemented")))),
                publisher_behavior: Arc::new(Mutex::new(|| {
                    Ok(Box::new(NoOpPublisher) as Box<dyn MessagePublisher>)
                })),
            }
        }
    }

    #[derive(Clone)]
    struct NoOpPublisher;
    #[async_trait::async_trait]
    impl MessagePublisher for NoOpPublisher {
        async fn send_batch(
            &self,
            _: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }
        async fn send(&self, _: crate::CanonicalMessage) -> Result<Sent, PublisherError> {
            Ok(Sent::Ack)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl CustomEndpointFactory for MockEndpointFactory {
        async fn create_consumer(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            if self.create_consumer_fail {
                return Err(anyhow::anyhow!("Endpoint unavailable"));
            }
            (self.consumer_behavior.lock().unwrap())()
        }
        async fn create_publisher(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            (self.publisher_behavior.lock().unwrap())()
        }
    }

    #[derive(Clone, Default)]
    struct HookState {
        consumer_connects: Arc<AtomicUsize>,
        consumer_disconnects: Arc<AtomicUsize>,
        publisher_connects: Arc<AtomicUsize>,
        publisher_disconnects: Arc<AtomicUsize>,
        shared_mutations: Arc<AtomicUsize>,
        fail_consumer_connect: Arc<AtomicBool>,
        fail_consumer_disconnect: Arc<AtomicBool>,
        fail_publisher_disconnect: Arc<AtomicBool>,
    }

    struct HookConsumer {
        state: HookState,
    }

    struct HookPublisher {
        state: HookState,
    }

    #[async_trait::async_trait]
    impl MessageConsumer for HookConsumer {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.state.consumer_connects.fetch_add(1, Ordering::SeqCst);
                self.state.shared_mutations.fetch_add(1, Ordering::SeqCst);
                if self.state.fail_consumer_connect.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("consumer hook failed"));
                }
                Ok(())
            }))
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.state
                    .consumer_disconnects
                    .fetch_add(1, Ordering::SeqCst);
                if self.state.fail_consumer_disconnect.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("consumer disconnect hook failed"));
                }
                Ok(())
            }))
        }

        async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
            Err(ConsumerError::EndOfStream)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl MessagePublisher for HookPublisher {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.state.publisher_connects.fetch_add(1, Ordering::SeqCst);
                self.state.shared_mutations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.state
                    .publisher_disconnects
                    .fetch_add(1, Ordering::SeqCst);
                if self.state.fail_publisher_disconnect.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("publisher disconnect hook failed"));
                }
                Ok(())
            }))
        }

        async fn send_batch(
            &self,
            _: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn hook_route(state: HookState, concurrency: usize) -> Route {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("hooks_{}", unique_id);
        let mut factory = MockEndpointFactory::new();

        let consumer_state = state.clone();
        factory.consumer_behavior = Arc::new(Mutex::new(move || {
            Ok(Box::new(HookConsumer {
                state: consumer_state.clone(),
            }) as Box<dyn MessageConsumer>)
        }));

        let publisher_state = state;
        factory.publisher_behavior = Arc::new(Mutex::new(move || {
            Ok(Box::new(HookPublisher {
                state: publisher_state.clone(),
            }) as Box<dyn MessagePublisher>)
        }));

        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name.clone(),
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        Route::new(input, output).with_concurrency(concurrency)
    }

    #[tokio::test]
    async fn test_lifecycle_hooks_called_once_sequentially() {
        let state = HookState::default();
        let route = hook_route(state.clone(), 1);

        let stopped_by_shutdown = route
            .run_until_err("test_lifecycle_sequential", None, None)
            .await
            .unwrap();

        assert!(!stopped_by_shutdown);
        assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
        assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
        assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
        assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
        assert_eq!(state.shared_mutations.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_final_commit_failure_after_end_of_stream_fails_sequential_route() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("final_commit_failure_{unique_id}");
        let eof_seen = Arc::new(tokio::sync::Notify::new());
        let consumer_eof_seen = eof_seen.clone();
        let mut factory = MockEndpointFactory::new();
        let reads = Arc::new(AtomicUsize::new(0));
        let consumer_reads = reads.clone();
        factory.consumer_behavior = Arc::new(Mutex::new(move || {
            struct FiniteConsumer {
                reads: Arc<AtomicUsize>,
                eof_seen: Arc<tokio::sync::Notify>,
            }
            #[async_trait::async_trait]
            impl MessageConsumer for FiniteConsumer {
                async fn receive_batch(
                    &mut self,
                    _: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    if self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
                        self.eof_seen.notify_one();
                        return Err(ConsumerError::EndOfStream);
                    }
                    let eof_seen = self.eof_seen.clone();
                    Ok(ReceivedBatch {
                        messages: vec![crate::CanonicalMessage::from("only")],
                        commit: Box::new(move |_| {
                            Box::pin(async move {
                                eof_seen.notified().await;
                                Err(anyhow::anyhow!("final commit failed"))
                            })
                        }),
                    })
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(FiniteConsumer {
                reads: consumer_reads.clone(),
                eof_seen: consumer_eof_seen.clone(),
            }) as Box<dyn MessageConsumer>)
        }));

        let disconnect_outcome = Arc::new(Mutex::new(None));
        let publisher_outcome = disconnect_outcome.clone();
        factory.publisher_behavior = Arc::new(Mutex::new(move || {
            struct OutcomePublisher(Arc<Mutex<Option<DisconnectOutcome>>>);
            #[async_trait::async_trait]
            impl MessagePublisher for OutcomePublisher {
                fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
                    Some(Box::pin(async move {
                        *self.0.lock().unwrap() = crate::traits::disconnect_outcome();
                        Ok(())
                    }))
                }
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Ok(SentBatch::Ack)
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(OutcomePublisher(publisher_outcome.clone())) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let endpoint = |name: String| Endpoint {
            endpoint_type: EndpointType::Custom {
                name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let route = Route::new(endpoint(factory_name.clone()), endpoint(factory_name));

        let err = route
            .run_until_err("test_final_commit_failure", None, None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("final commit failed"));
        assert_eq!(
            *disconnect_outcome.lock().unwrap(),
            Some(DisconnectOutcome::Failed)
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_lifecycle_hooks_called_once_concurrently() {
        let state = HookState::default();
        let route = hook_route(state.clone(), 4);

        route
            .run_until_err("test_lifecycle_concurrent", None, None)
            .await
            .unwrap();

        assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
        assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
        assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
        assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_lifecycle_on_connect_failure_stops_route() {
        let state = HookState::default();
        state.fail_consumer_connect.store(true, Ordering::SeqCst);
        let route = hook_route(state.clone(), 1);

        let err = route
            .run_until_err("test_lifecycle_connect_failure", None, None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("on_connect hook failed"));
        assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
        assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_lifecycle_on_disconnect_failure_does_not_stop_route() {
        let state = HookState::default();
        state.fail_consumer_disconnect.store(true, Ordering::SeqCst);
        state
            .fail_publisher_disconnect
            .store(true, Ordering::SeqCst);
        let route = hook_route(state.clone(), 1);

        let stopped_by_shutdown = route
            .run_until_err("test_lifecycle_disconnect_failure", None, None)
            .await
            .unwrap();

        assert!(!stopped_by_shutdown);
        assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
        assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_forward_buffered_ready_delivers_a_signal_the_loop_never_observed() {
        // The terminal arms of the reconnect loop can win the `select!` race against a
        // ready signal the run task already buffered. Forwarding it from there is what
        // keeps a route that ran to completion from being reported as one that never
        // started.
        let (iter_ready_tx, iter_ready_rx) = bounded::<()>(1);
        let (ready_tx, ready_rx) = bounded::<()>(1);
        iter_ready_tx.send(()).await.unwrap();

        let mut startup_notified = false;
        forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
        assert!(startup_notified);
        assert!(
            ready_rx.try_recv().is_ok(),
            "buffered ready must reach `run()`"
        );

        // `run()` consumes the startup channel once, so a second send would sit unread.
        iter_ready_tx.send(()).await.unwrap();
        forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
        assert!(
            ready_rx.try_recv().is_err(),
            "startup must not be notified twice"
        );
    }

    #[tokio::test]
    async fn test_forward_buffered_ready_keeps_a_never_ready_route_a_failure() {
        let (_iter_ready_tx, iter_ready_rx) = bounded::<()>(1);
        let (ready_tx, ready_rx) = bounded::<()>(1);

        let mut startup_notified = false;
        forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
        assert!(!startup_notified);
        assert!(ready_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_start_fails_on_unavailable_endpoint() {
        // tokio::time::pause();
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("unavailable_{}", unique_id);

        let factory = Arc::new(MockEndpointFactory {
            create_consumer_fail: true,
            ..MockEndpointFactory::new()
        });
        register_endpoint_factory(&factory_name, factory).unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint::new_memory("out", 10);
        let route = Route::new(input, output);

        // The route should fail to start because the input endpoint fails to create.
        // The run() method waits for a ready signal which never comes.
        let result = route.run("test_start_fail").await;
        let err = result.expect_err("must fail").to_string();
        // A stalled connect is a timeout, not a route that ended: the two now read
        // differently, and the recorded cause rides along.
        assert!(
            err.contains("failed to start: did not become ready within 5000ms")
                && err.contains("Endpoint unavailable"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_start_failure_distinguishes_an_ended_route_from_a_timeout() {
        // A drain gives up after DRAIN_MAX_RECONNECT_ATTEMPTS, so the route task ends
        // long before the startup timeout. `ready_rx` then fails immediately, and
        // calling that a 5000ms timeout sends the reader after a stall that never
        // happened.
        let factory_name = format!("ended_before_ready_{}", fast_uuid_v7::gen_id());
        register_endpoint_factory(
            &factory_name,
            Arc::new(MockEndpointFactory {
                create_consumer_fail: true,
                ..MockEndpointFactory::new()
            }),
        )
        .unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let route = Route::new(input, Endpoint::new_memory("out", 10))
            .with_exit_on_empty(true)
            .with_reconnect_interval_ms(1);

        let err = route
            .run("test_ended_before_ready")
            .await
            .expect_err("must fail")
            .to_string();
        assert!(
            err.contains("failed to start: ended before it signalled ready")
                && err.contains("outcome: failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_reconnect_on_consumer_error() {
        // tokio::time::pause();
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("reconnect_{}", unique_id);

        // Shared state to track connection attempts
        let connection_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = connection_attempts.clone();

        let consumer_logic = move || -> Result<Box<dyn MessageConsumer>, anyhow::Error> {
            let attempt = attempts_clone.fetch_add(1, Ordering::SeqCst);

            struct FlakyConsumer {
                attempt: usize,
            }
            #[async_trait::async_trait]
            impl MessageConsumer for FlakyConsumer {
                async fn receive_batch(
                    &mut self,
                    _max: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    if self.attempt == 0 {
                        // First connection works for one batch, then fails
                        self.attempt = 999; // prevent infinite loop in this instance
                        Ok(ReceivedBatch {
                            messages: vec![crate::CanonicalMessage::from("msg1")],
                            commit: Box::new(|_| Box::pin(async { Ok(()) })),
                        })
                    } else if self.attempt == 999 {
                        // Simulate connection drop
                        Err(ConsumerError::Connection(anyhow::anyhow!(
                            "Connection dropped"
                        )))
                    } else {
                        // Subsequent connections work
                        // Sleep a bit to prevent busy loop in test
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok(ReceivedBatch {
                            messages: vec![crate::CanonicalMessage::from("msg2")],
                            commit: Box::new(|_| Box::pin(async { Ok(()) })),
                        })
                    }
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(FlakyConsumer { attempt }))
        };

        let mut factory = MockEndpointFactory::new();
        factory.consumer_behavior = Arc::new(Mutex::new(consumer_logic));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint::new_memory(&format!("out_{}", unique_id), 10);
        let route = Route::new(input, output.clone());

        route.deploy("test_reconnect").await.unwrap();

        // Wait for reconnection and messages
        let mut verifier = create_consumer_from_route("verifier", &output)
            .await
            .unwrap();

        // Should receive msg1
        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for msg1")
            .unwrap();
        assert_eq!(msg1.message.get_payload_str(), "msg1");

        // Route encounters error, sleeps 5s (skipped by pause), reconnects.
        // Should receive msg2
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for msg2")
            .unwrap();
        assert_eq!(msg2.message.get_payload_str(), "msg2");

        assert!(connection_attempts.load(Ordering::SeqCst) >= 2);
        Route::stop("test_reconnect").await;
    }

    #[tokio::test]
    async fn test_route_handle_status_reports_async_connection_failure() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("status_fail_{}", unique_id);

        // Consumer is created successfully (so `run()` reports ready and returns Ok),
        // but every `receive_batch` fails with a retryable Connection error — mirroring
        // an endpoint that connects on a background thread and fails asynchronously.
        let consumer_logic = move || -> Result<Box<dyn MessageConsumer>, anyhow::Error> {
            struct FailingConsumer;
            #[async_trait::async_trait]
            impl MessageConsumer for FailingConsumer {
                async fn receive_batch(
                    &mut self,
                    _max: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    Err(ConsumerError::Connection(anyhow::anyhow!(
                        "queue manager unreachable"
                    )))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(FailingConsumer))
        };

        let mut factory = MockEndpointFactory::new();
        factory.consumer_behavior = Arc::new(Mutex::new(consumer_logic));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint::new_memory(&format!("out_{}", unique_id), 10);
        let mut route = Route::new(input, output);
        // Long enough to observe the unhealthy/reconnecting state, short enough that the
        // backoff sleep (which the loop can't interrupt) doesn't slow the test down.
        route.options.reconnect_interval_ms = 2_000;

        // `run()` returns Ok because the consumer was created and ready was signalled.
        let handle = route.run("test_status_fail").await.unwrap();

        // The async receive failure should flip the live status to unhealthy.
        let mut status = handle.status();
        for _ in 0..50 {
            if !status.healthy {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            status = handle.status();
        }

        assert!(!status.healthy, "expected route to report unhealthy status");
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("queue manager unreachable"),
            "expected last connection error in status, got {:?}",
            status.error
        );

        handle.stop().await;
        let _ = handle.join().await;
    }

    // N3 (regression): `retry` used to wrap the handler, so a sink that retried N times
    // ran the handler N times for the same message — every side effect in it repeated.
    // The handler now sits outside the middlewares and runs once; only the publish is
    // retried.
    #[tokio::test]
    async fn test_retry_does_not_re_invoke_the_handler() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("retry_handler_{}", unique_id);

        let sends = Arc::new(AtomicUsize::new(0));
        let sends_pub = sends.clone();
        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(move || {
            struct CountingFailPublisher(Arc<AtomicUsize>);
            #[async_trait::async_trait]
            impl MessagePublisher for CountingFailPublisher {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Err(PublisherError::Retryable(anyhow::anyhow!("sink down")))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(CountingFailPublisher(sends_pub.clone())) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let handler_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls_inner = handler_calls.clone();
        let handler = move |msg: crate::CanonicalMessage| {
            let calls = handler_calls_inner.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::Handled::Publish(msg))
            }
        };

        let in_topic = format!("retry_handler_in_{}", unique_id);
        let input = Endpoint::new_memory(&in_topic, 10);
        let input_ch = input.channel().unwrap();

        const ATTEMPTS: usize = 4;
        let mut output = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        output
            .middlewares
            .push(Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: ATTEMPTS,
                initial_interval_ms: 1,
                max_interval_ms: 2,
                multiplier: 1.0,
            }));

        let route = Route::new(input, output).with_handler(handler);
        route.deploy("test_retry_handler_once").await.unwrap();
        input_ch.send_message("one".into()).await.unwrap();

        // Wait for the sink to have exhausted its attempts.
        for _ in 0..200 {
            if sends.load(Ordering::SeqCst) >= ATTEMPTS {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            sends.load(Ordering::SeqCst) >= ATTEMPTS,
            "expected the publish itself to be retried"
        );
        assert_eq!(
            handler_calls.load(Ordering::SeqCst),
            1,
            "the handler must run once per message, not once per retry attempt"
        );

        Route::stop("test_retry_handler_once").await;
    }

    // N5 (regression): a drain route whose output leg is at a dead address used to
    // reconnect forever — `finished: false`, no outcome, and `healthy: true` on every
    // poll, because reconnecting flipped health back the moment the consumer was ready.
    // It must now report unhealthy while it flaps and end as `Failed`.
    #[tokio::test]
    async fn test_drain_route_with_a_dead_output_fails_instead_of_reconnecting_forever() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("dead_output_{}", unique_id);

        // Constructs fine (so the route reports ready), then fails every send with a
        // connection error — an HTTP sink pointed at a closed port behaves this way.
        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(|| {
            struct DeadPublisher;
            #[async_trait::async_trait]
            impl MessagePublisher for DeadPublisher {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Err(PublisherError::Connection(anyhow::anyhow!(
                        "connection refused"
                    )))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(DeadPublisher) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        // A file source, like the reported repro: it re-reads the same records after a
        // reconnect, so the failing batch comes back around every pass. That is what
        // turned "retry the connection" into "never finish".
        let dir = tempfile::tempdir().unwrap();
        let in_path = dir.path().join("in.jsonl");
        std::fs::write(&in_path, "a\nb\nc\n").unwrap();
        let input = Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: in_path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        }));

        let output = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let route = Route::new(input, output)
            .with_exit_on_empty(true)
            .with_reconnect_interval_ms(10);

        let handle = route.run("test_dead_output_drain").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("drain route reconnected forever instead of giving up");

        assert_eq!(handle.outcome(), Some(RouteOutcome::Failed));
        let status = handle.status();
        assert!(!status.healthy, "a wedged route must not report healthy");
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("connection refused"),
            "status should carry the leg's error, got {:?}",
            status.error
        );

        Route::stop("test_dead_output_drain").await;
    }

    // Regression: with `concurrency > 1`, a source that reports `EndOfStream` used to
    // fall through to `Ok(shutdown_rx.is_empty())` at the end of `run_concurrently`.
    // No shutdown had been requested, so that read as "reconnect" and the outer loop
    // reran the source from the top, forever — `copy --drain` from a MongoDB snapshot
    // never exited and rewrote its whole sink on every pass. The sequential runner
    // always got this right, so the bug only showed above concurrency 1.
    #[tokio::test]
    async fn test_end_of_stream_completes_a_concurrent_route_instead_of_reconnecting() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("eos_concurrent_{}", unique_id);

        // One batch, then end of stream — a finite source, like a snapshot read.
        let batches = Arc::new(AtomicUsize::new(0));
        let consumer_batches = batches.clone();
        let mut factory = MockEndpointFactory::new();
        factory.consumer_behavior = Arc::new(Mutex::new(move || {
            struct FiniteConsumer {
                batches: Arc<AtomicUsize>,
            }
            #[async_trait::async_trait]
            impl MessageConsumer for FiniteConsumer {
                async fn receive_batch(
                    &mut self,
                    _: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    if self.batches.fetch_add(1, Ordering::SeqCst) > 0 {
                        return Err(ConsumerError::EndOfStream);
                    }
                    Ok(ReceivedBatch {
                        messages: vec![crate::CanonicalMessage::from("only")],
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    })
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(FiniteConsumer {
                batches: consumer_batches.clone(),
            }) as Box<dyn MessageConsumer>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let route = Route::new(input, Endpoint::new(EndpointType::Null))
            .with_concurrency(4)
            .with_reconnect_interval_ms(10);

        let handle = route.run("test_eos_concurrent").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a drained source must end the route, not restart it");

        assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));
        // Two reads: the batch and the end-of-stream. A restart would read again.
        assert_eq!(
            batches.load(Ordering::SeqCst),
            2,
            "the route reconnected and reread the source after end of stream"
        );

        Route::stop("test_eos_concurrent").await;
    }

    // N5 as originally reported: the dead leg sits inside a `fanout` next to a working
    // one. `FanoutPublisher::send_batch` propagates a hard error from any leg, so this
    // must reach the same bound rather than wedging at `healthy: true` forever.
    #[tokio::test]
    async fn test_drain_fanout_with_one_dead_leg_fails_instead_of_wedging() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("dead_leg_{}", unique_id);

        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(|| {
            struct DeadLeg;
            #[async_trait::async_trait]
            impl MessagePublisher for DeadLeg {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Err(PublisherError::Connection(anyhow::anyhow!(
                        "dead leg refused"
                    )))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(DeadLeg) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let in_path = dir.path().join("in5.jsonl");
        std::fs::write(&in_path, "a\nb\nc\nd\ne\n").unwrap();
        let good_path = dir.path().join("fan_ok.jsonl");

        let input = Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: in_path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        }));
        let output = Endpoint::new(EndpointType::Fanout(vec![
            Endpoint::new(EndpointType::File(crate::models::FileConfig {
                path: good_path.to_str().unwrap().to_string(),
                format: crate::models::FileFormat::Raw,
                ..Default::default()
            })),
            Endpoint::new(EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            }),
        ]));

        let handle = Route::new(input, output)
            .with_exit_on_empty(true)
            .with_batch_size(1)
            .with_reconnect_interval_ms(10)
            .run("test_dead_fanout_leg")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(15), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fanout with a dead leg wedged instead of giving up");

        assert_eq!(handle.outcome(), Some(RouteOutcome::Failed));
        let status = handle.status();
        assert!(!status.healthy, "a wedged fanout must not report healthy");
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("dead leg refused"),
            "status should carry the failing leg's error, got {:?}",
            status.error
        );

        Route::stop("test_dead_fanout_leg").await;
    }

    /// A fanout leg carrying its own `retry` + `dlq` must dead-letter its failures exactly
    /// like the same endpoint would as a route's sole output.
    #[tokio::test]
    async fn test_fanout_leg_middleware_dead_letters_its_own_failures() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("failing_leg_{}", unique_id);

        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(|| {
            struct AlwaysFails;
            #[async_trait::async_trait]
            impl MessagePublisher for AlwaysFails {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Err(PublisherError::Retryable(anyhow::anyhow!("leg refused")))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(AlwaysFails) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let in_path = dir.path().join("fan_in.jsonl");
        std::fs::write(&in_path, "a\n").unwrap();
        let good_path = dir.path().join("fan_good.jsonl");
        let dlq_path = dir.path().join("fan_dlq.jsonl");

        let raw_file = |path: &std::path::Path| {
            Endpoint::new(EndpointType::File(crate::models::FileConfig {
                path: path.to_str().unwrap().to_string(),
                format: crate::models::FileFormat::Raw,
                ..Default::default()
            }))
        };

        let mut failing_leg = Endpoint::new(EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        });
        failing_leg.middlewares = vec![
            crate::models::Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                max_interval_ms: 2,
                multiplier: 1.0,
            }),
            crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: raw_file(&dlq_path),
            })),
        ];

        let output = Endpoint::new(EndpointType::Fanout(vec![
            raw_file(&good_path),
            failing_leg,
        ]));

        let handle = Route::new(raw_file(&in_path), output)
            .with_exit_on_empty(true)
            .with_batch_size(1)
            .with_reconnect_interval_ms(10)
            .run("test_fanout_leg_dlq")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(15), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("route never finished");
        Route::stop("test_fanout_leg_dlq").await;

        let dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
        assert!(
            dlq.contains('a'),
            "the fanout leg's own dlq never received the failed message, got {dlq:?}"
        );
    }

    /// The reported N20 shape verbatim: a real `http` leg pointed at a closed port. The mock
    /// above only proves the wiring; this proves the classification an unreachable sink
    /// actually produces still reaches that leg's dlq.
    #[cfg(feature = "http")]
    #[tokio::test]
    async fn test_fanout_http_leg_dead_letters_a_connection_refusal() {
        #[cfg(feature = "rustls-aws-lc")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = tempfile::tempdir().unwrap();
        let in_path = dir.path().join("http_fan_in.jsonl");
        std::fs::write(&in_path, "a\n").unwrap();
        let good_path = dir.path().join("http_fan_good.jsonl");
        let dlq_path = dir.path().join("http_fan_dlq.jsonl");

        let raw_file = |path: &std::path::Path| {
            Endpoint::new(EndpointType::File(crate::models::FileConfig {
                path: path.to_str().unwrap().to_string(),
                format: crate::models::FileFormat::Raw,
                ..Default::default()
            }))
        };

        let mut dead_leg = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "http://127.0.0.1:1/dead".to_string(),
            request_timeout_ms: Some(500),
            ..Default::default()
        }));
        dead_leg.middlewares = vec![
            crate::models::Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                max_interval_ms: 2,
                multiplier: 1.0,
            }),
            crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: raw_file(&dlq_path),
            })),
        ];

        let output = Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), dead_leg]));

        let handle = Route::new(raw_file(&in_path), output)
            .with_exit_on_empty(true)
            .with_batch_size(1)
            .with_reconnect_interval_ms(10)
            .run("test_fanout_http_leg_dlq")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(30), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("route never finished");
        Route::stop("test_fanout_http_leg_dlq").await;

        let dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
        assert!(
            dlq.contains('a'),
            "the http leg's own dlq never received the failed message, got {dlq:?}"
        );
    }

    /// A `Connection` failure is deliberately never dead-lettered: `retry` does not retry it
    /// and `dlq` propagates it so the route reconnects instead of dead-lettering a whole
    /// backlog while a sink is merely down. That has to hold identically inside and outside a
    /// fanout — a leg must not be the reason a message stops reaching its dlq.
    #[tokio::test]
    async fn test_connection_failures_bypass_the_dlq_identically_in_and_out_of_a_fanout() {
        async fn dlq_content_for(in_fanout: bool) -> String {
            let factory_name = format!("conn_fail_{}", fast_uuid_v7::gen_id());
            let mut factory = MockEndpointFactory::new();
            factory.publisher_behavior = Arc::new(Mutex::new(|| {
                struct Disconnected;
                #[async_trait::async_trait]
                impl MessagePublisher for Disconnected {
                    async fn send_batch(
                        &self,
                        _: Vec<crate::CanonicalMessage>,
                    ) -> Result<SentBatch, PublisherError> {
                        Err(PublisherError::Connection(anyhow::anyhow!(
                            "Simulated connection loss"
                        )))
                    }
                    fn as_any(&self) -> &dyn Any {
                        self
                    }
                }
                Ok(Box::new(Disconnected) as Box<dyn MessagePublisher>)
            }));
            register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

            let dir = tempfile::tempdir().unwrap();
            let in_path = dir.path().join("in.jsonl");
            std::fs::write(&in_path, "a\n").unwrap();
            let dlq_path = dir.path().join("dlq.jsonl");
            let good_path = dir.path().join("good.jsonl");
            let raw_file = |path: &std::path::Path| {
                Endpoint::new(EndpointType::File(crate::models::FileConfig {
                    path: path.to_str().unwrap().to_string(),
                    format: crate::models::FileFormat::Raw,
                    ..Default::default()
                }))
            };

            let mut failing = Endpoint::new(EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            });
            failing.middlewares = vec![
                crate::models::Middleware::Retry(crate::models::RetryMiddleware {
                    max_attempts: 2,
                    initial_interval_ms: 1,
                    max_interval_ms: 2,
                    multiplier: 1.0,
                }),
                crate::models::Middleware::Dlq(Box::new(
                    crate::models::DeadLetterQueueMiddleware {
                        endpoint: raw_file(&dlq_path),
                    },
                )),
            ];
            let output = if in_fanout {
                Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), failing]))
            } else {
                failing
            };

            let route_name = format!("conn_dlq_{in_fanout}");
            let handle = Route::new(raw_file(&in_path), output)
                .with_exit_on_empty(true)
                .with_batch_size(1)
                .with_reconnect_interval_ms(10)
                .run(&route_name)
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(15), async {
                while handle.outcome().is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            Route::stop(&route_name).await;
            std::fs::read_to_string(&dlq_path).unwrap_or_default()
        }

        assert_eq!(
            dlq_content_for(true).await,
            dlq_content_for(false).await,
            "a fanout leg must dead-letter a connection failure the same way a sole output does"
        );
    }

    /// Same as above but driven by an `http` webhook input, which is the shape the finding
    /// was reported in — a request/reply consumer, not a file drain.
    #[cfg(feature = "http")]
    #[tokio::test]
    async fn test_fanout_leg_dlq_works_behind_an_http_webhook_input() {
        #[cfg(feature = "rustls-aws-lc")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = tempfile::tempdir().unwrap();
        let good_path = dir.path().join("wh_good.jsonl");
        let dlq_path = dir.path().join("wh_dlq.jsonl");
        let raw_file = |path: &std::path::Path| {
            Endpoint::new(EndpointType::File(crate::models::FileConfig {
                path: path.to_str().unwrap().to_string(),
                format: crate::models::FileFormat::Raw,
                ..Default::default()
            }))
        };

        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let input = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: format!("127.0.0.1:{port}"),
            fire_and_forget: true,
            ..Default::default()
        }));

        let mut dead_leg = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "http://127.0.0.1:1/dead".to_string(),
            request_timeout_ms: Some(500),
            ..Default::default()
        }));
        dead_leg.middlewares = vec![
            crate::models::Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                max_interval_ms: 2,
                multiplier: 1.0,
            }),
            crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: raw_file(&dlq_path),
            })),
        ];
        let output = Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), dead_leg]));

        let _handle = Route::new(input, output)
            .run("test_fanout_webhook_dlq")
            .await
            .unwrap();

        let addr = format!("127.0.0.1:{port}");
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let client = reqwest::Client::new();
        client
            .post(format!("http://{addr}/"))
            .body(r#"{"order_id":"o1"}"#)
            .send()
            .await
            .expect("webhook POST failed");

        let mut dlq = String::new();
        for _ in 0..200 {
            dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
            if dlq.contains("o1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Route::stop("test_fanout_webhook_dlq").await;

        assert!(
            dlq.contains("o1"),
            "the fanout leg's dlq never received the failed message, got {dlq:?}"
        );
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn referenced_http_input_survives_transient_sink_failure_sequentially_and_concurrently() {
        #[cfg(feature = "rustls-aws-lc")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
        let _ = rustls::crypto::ring::default_provider().install_default();

        for concurrency in [1, 4] {
            let port = {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            };
            let addr = format!("127.0.0.1:{port}");
            let input_name = format!("referenced_http_{}", fast_uuid_v7::gen_id());
            register_endpoint(
                &input_name,
                Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
                    url: addr.clone(),
                    request_timeout_ms: Some(1_000),
                    ..Default::default()
                })),
            );
            let input = Endpoint::new(EndpointType::Ref(input_name));
            let output = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
                url: "http://127.0.0.1:1/dead".to_string(),
                request_timeout_ms: Some(250),
                pass_through_status: true,
                ..Default::default()
            }));
            let route_name = format!(
                "referenced_http_listener_survives_{concurrency}_{}",
                fast_uuid_v7::gen_id()
            );
            let handle = Route::new(input, output)
                .with_concurrency(concurrency)
                .with_reconnect_interval_ms(10)
                .run(&route_name)
                .await
                .unwrap();

            for _ in 0..200 {
                if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            for attempt in 1..=2 {
                let response = reqwest::Client::new()
                    .post(format!("http://{addr}/test"))
                    .body("x")
                    .send()
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "concurrency {concurrency} request {attempt} could not reach listener: {e}"
                        )
                    });
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::BAD_GATEWAY,
                    "concurrency {concurrency} request {attempt} should receive a request-level failure"
                );
                assert_eq!(
                    handle.outcome(),
                    None,
                    "concurrency {concurrency} route stopped after request {attempt}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            Route::stop(&route_name).await;
        }
    }

    #[test]
    fn transient_sink_failures_keep_existing_etl_reconnect_policy() {
        let synchronous_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "127.0.0.1:8081".to_string(),
            ..Default::default()
        }));
        let fire_and_forget_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "127.0.0.1:8080".to_string(),
            fire_and_forget: true,
            ..Default::default()
        }));
        let streamable_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "127.0.0.1:8082".to_string(),
            receive_streamable: true,
            ..Default::default()
        }));
        let grpc_server = Endpoint::new(EndpointType::Grpc(crate::models::GrpcConfig {
            url: "127.0.0.1:50051".to_string(),
            server_mode: true,
            ..Default::default()
        }));
        let websocket_server =
            Endpoint::new(EndpointType::WebSocket(crate::models::WebSocketConfig {
                url: "127.0.0.1:9000".to_string(),
                ..Default::default()
            }));

        for input in [
            &fire_and_forget_http,
            &streamable_http,
            &grpc_server,
            &websocket_server,
        ] {
            assert!(matches!(
                transient_publish_failure_policy(input, false),
                TransientPublishFailurePolicy::StopRoute
            ));
        }
        assert!(matches!(
            transient_publish_failure_policy(&synchronous_http, false),
            TransientPublishFailurePolicy::StopRoute
        ));

        let opted_in_nested_output = Endpoint::new(EndpointType::Fanout(vec![Endpoint::new(
            EndpointType::Http(crate::models::HttpConfig {
                url: "http://127.0.0.1:9001".to_string(),
                pass_through_status: true,
                ..Default::default()
            }),
        )]));
        assert!(output_passes_through_http_status("proxy", &opted_in_nested_output).unwrap());
        let mixed_output = Endpoint::new(EndpointType::Fanout(vec![
            Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
                url: "http://127.0.0.1:9001".to_string(),
                pass_through_status: true,
                ..Default::default()
            })),
            Endpoint::new(EndpointType::Kafka(crate::models::KafkaConfig {
                url: "127.0.0.1:9092".to_string(),
                ..Default::default()
            })),
        ]));
        assert!(!output_passes_through_http_status("proxy", &mixed_output).unwrap());
        assert!(matches!(
            transient_publish_failure_policy(&synchronous_http, true),
            TransientPublishFailurePolicy::ReplyBadGateway
        ));

        let suffix = fast_uuid_v7::gen_id();
        let http_name = format!("policy_http_{suffix}");
        let http_alias = format!("policy_http_alias_{suffix}");
        register_endpoint(&http_name, synchronous_http);
        register_endpoint(&http_alias, Endpoint::new(EndpointType::Ref(http_name)));
        assert!(matches!(
            transient_publish_failure_policy(&Endpoint::new(EndpointType::Ref(http_alias)), true),
            TransientPublishFailurePolicy::ReplyBadGateway
        ));

        let memory_name = format!("policy_memory_{suffix}");
        register_endpoint(&memory_name, Endpoint::new_memory("policy-memory", 1));
        for input in [
            Endpoint::new(EndpointType::Ref(memory_name)),
            Endpoint::new(EndpointType::Ref(format!("missing_policy_input_{suffix}"))),
        ] {
            assert!(matches!(
                transient_publish_failure_policy(&input, true),
                TransientPublishFailurePolicy::StopRoute
            ));
        }
    }

    // The flip side of the test above: gating health on "the pass stayed up" must not
    // leave a route that recovered stuck reporting the failure forever.
    #[tokio::test(start_paused = true)]
    async fn test_route_reports_healthy_again_after_it_recovers() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("recovering_{}", unique_id);

        // Fails the first connection, then serves a working publisher.
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(move || {
            struct OneShotFail;
            #[async_trait::async_trait]
            impl MessagePublisher for OneShotFail {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Err(PublisherError::Connection(anyhow::anyhow!("first flap")))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::new(OneShotFail) as Box<dyn MessagePublisher>)
            } else {
                Ok(Box::new(NoOpPublisher) as Box<dyn MessagePublisher>)
            }
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let in_topic = format!("recovering_in_{}", unique_id);
        let input = Endpoint::new_memory(&in_topic, 10);
        let input_ch = input.channel().unwrap();
        let output = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };

        let handle = Route::new(input, output)
            .with_reconnect_interval_ms(10)
            .run("test_route_recovers")
            .await
            .unwrap();
        input_ch.send_message("one".into()).await.unwrap();

        // Provoke the failure, then let the replacement connection run past STABLE_RUN.
        tokio::time::sleep(Duration::from_millis(200)).await;
        tokio::time::sleep(STABLE_RUN + Duration::from_secs(1)).await;

        let status = handle.status();
        assert!(
            status.healthy,
            "a route that recovered must not stay unhealthy: {:?}",
            status.error
        );
        assert!(status.error.is_none(), "stale error: {:?}", status.error);

        Route::stop("test_route_recovers").await;
    }

    #[tokio::test]
    async fn test_non_retryable_handler_error_does_not_crash_route() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("bad_input_in_{}", unique_id);
        let out_topic = format!("bad_input_out_{}", unique_id); // Not used, but good practice

        let input = Endpoint::new_memory(&in_topic, 10);
        let output = Endpoint::new_memory(&out_topic, 10);

        // A handler that fails on specific input
        let handler = |msg: crate::CanonicalMessage| async move {
            if msg.get_payload_str() == "poison" {
                Err(HandlerError::NonRetryable(anyhow::anyhow!("Invalid input")))
            } else {
                Ok(crate::Handled::Publish(msg))
            }
        };

        let route = Route::new(input.clone(), output).with_handler(handler);
        route.deploy("test_invalid_input").await.unwrap();

        let input_ch = input.channel().unwrap();
        let out_channel = route.output.channel().unwrap();

        input_ch.send_message("poison".into()).await.unwrap();

        input_ch.send_message("valid".into()).await.unwrap();

        // Verify the valid message was processed and published
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(msg) = out_channel.drain_messages().pop() {
                    return msg;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Timed out waiting for valid message to be processed");
        assert_eq!(received.get_payload_str(), "valid");
        Route::stop("test_invalid_input").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dlq_and_retry_batch_integration() {
        use crate::models::{DeadLetterQueueMiddleware, Middleware, RetryMiddleware};
        use crate::traits::{MessagePublisher, PublisherError, SentBatch};
        use std::collections::HashMap;
        use std::sync::Mutex;

        // Mock publisher that fails messages with even-numbered IDs
        #[derive(Clone)]
        struct PartialFailPublisher {
            attempts: Arc<Mutex<HashMap<u128, usize>>>,
        }

        #[async_trait::async_trait]
        impl MessagePublisher for PartialFailPublisher {
            async fn send_batch(
                &self,
                messages: Vec<CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                let mut failed = Vec::new();
                let mut attempts = self.attempts.lock().unwrap();

                for msg in messages {
                    let msg_num: u32 = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                        .unwrap()["id"]
                        .as_u64()
                        .unwrap() as u32;

                    let attempt_count = attempts.entry(msg.message_id).or_insert(0);
                    *attempt_count += 1;

                    if msg_num % 2 == 0 {
                        // Fail even numbers
                        failed.push((
                            msg,
                            PublisherError::Retryable(anyhow::anyhow!("simulated failure")),
                        ));
                    }
                    // Odd numbers succeed implicitly by not being in `failed`
                }

                if failed.is_empty() {
                    Ok(SentBatch::Ack)
                } else {
                    Ok(SentBatch::Partial {
                        responses: None,
                        failed,
                    })
                }
            }
            async fn send(
                &self,
                _msg: CanonicalMessage,
            ) -> Result<crate::traits::Sent, PublisherError> {
                unimplemented!()
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let in_topic = "batch_retry_dlq_in";
        let out_topic = "batch_retry_dlq_out";
        let dlq_topic = "batch_retry_dlq_dlq";

        let input = Endpoint::new_memory(in_topic, 10);
        let dlq_endpoint = Endpoint::new_memory(dlq_topic, 10);

        let mock_publisher = PartialFailPublisher {
            attempts: Arc::new(Mutex::new(HashMap::new())),
        };

        let mut output_with_middlewares = Endpoint::new_memory(out_topic, 10);
        output_with_middlewares.middlewares = vec![
            Middleware::Retry(RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                ..Default::default()
            }),
            Middleware::Dlq(Box::new(DeadLetterQueueMiddleware {
                endpoint: dlq_endpoint.clone(),
            })),
        ];

        let route = Route::new(input.clone(), output_with_middlewares).with_batch_size(4);
        // Inject the mock publisher into the route's output
        let final_publisher = crate::middleware::apply_middlewares_to_publisher(
            Box::new(mock_publisher.clone()),
            &route.output,
            "test_route",
        )
        .await
        .unwrap();

        // We need a way to run the route with our mocked publisher.
        // The simplest way is to manually drive the core logic.
        let (work_tx, work_rx) =
            async_channel::bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(1);
        let (seq_tx, _sequencer_handle) = spawn_sequencer(1);

        // Spawn a worker to process one batch
        tokio::spawn(async move {
            if let Ok((messages, commit)) = work_rx.recv().await {
                let batch_len = messages.len();
                match final_publisher.send_batch(messages).await {
                    Ok(SentBatch::Ack) => {
                        let _ = commit(vec![MessageDisposition::Ack; batch_len]).await;
                    }
                    Ok(SentBatch::Partial { failed, .. }) => {
                        // In a real route, we'd map responses, but here we just care about failure.
                        let dispositions = if failed.is_empty() {
                            vec![MessageDisposition::Ack; batch_len]
                        } else {
                            // This is a simplification for the test. A real implementation
                            // would map dispositions based on message IDs.
                            vec![MessageDisposition::Nack; batch_len]
                        };
                        let _ = commit(dispositions).await;
                    }
                    Err(_) => {
                        let _ = commit(vec![MessageDisposition::Nack; batch_len]).await;
                    }
                }
            }
        });

        let mut messages = Vec::new();
        for i in 1..=4 {
            // 1 (ok), 2 (fail), 3 (ok), 4 (fail)
            messages.push(CanonicalMessage::from_json(serde_json::json!({"id": i})).unwrap());
        }
        let commit = wrap_commit(Box::new(|_| Box::pin(async { Ok(()) })), 0, seq_tx.clone());
        work_tx.send((messages, commit)).await.unwrap();

        let dlq_channel = dlq_endpoint.channel().unwrap();

        let start = std::time::Instant::now();
        while dlq_channel.len() < 2 {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let dlq_msgs = dlq_channel.drain_messages();

        assert_eq!(dlq_msgs.len(), 2, "Expected 2 messages to go to DLQ");

        let dlq_ids: std::collections::HashSet<u32> = dlq_msgs
            .iter()
            .map(|m| {
                serde_json::from_slice::<serde_json::Value>(&m.payload).unwrap()["id"]
                    .as_u64()
                    .unwrap() as u32
            })
            .collect();

        assert!(dlq_ids.contains(&2));
        assert!(dlq_ids.contains(&4));

        // Verify retry attempts
        let attempts = mock_publisher.attempts.lock().unwrap();
        // Messages 2 and 4 should be tried `max_attempts` times.
        assert_eq!(attempts.values().filter(|&&c| c == 2).count(), 2);
        // Messages 1 and 3 should be tried once.
        assert_eq!(attempts.values().filter(|&&c| c == 1).count(), 2);
    }

    /// A sink that rejects permanently with no DLQ configured drops the batch so
    /// the route is not wedged re-reading a poison message — but the drop must
    /// not be silent. The route still completes; it just carries the cause, so a
    /// caller polling only `outcome` cannot mistake a discarded batch for a
    /// clean delivery.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_dropped_poison_batch_is_recorded_on_the_route_status() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("drop_in_{}", unique_id);
        let out_topic = format!("drop_out_{}", unique_id);
        let input = Endpoint::new_memory(&in_topic, 10);

        let mut output = Endpoint::new_memory(&out_topic, 10);
        output.middlewares = vec![Middleware::RandomPanic(RandomPanicMiddleware {
            mode: FaultMode::JsonFormatError,
            trigger_on_message: None,
            enabled: true,
            ..Default::default()
        })];

        let input_ch = input.channel().unwrap();
        input_ch.send_message("poison".into()).await.unwrap();

        let route = Route::new(input, output)
            .with_fault_injection(true)
            .with_exit_on_empty(true);
        let handle = route.run("test_dropped_poison_recorded").await.unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(outcome) = handle.outcome() {
                    return outcome;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the drain finishes rather than retrying the poison batch forever");

        assert_eq!(
            outcome,
            RouteOutcome::Completed,
            "dropping the batch lets the drain finish"
        );
        let error = handle.status().error.unwrap_or_default();
        assert!(
            error.contains("dropped 1 message"),
            "the discarded batch is reported, not silently swallowed: {error:?}"
        );
        assert!(
            error.contains("JSON format error"),
            "the reported cause names the underlying rejection: {error:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_route_dlq_integration() {
        // Setup: Input -> [Panic(Disconnect) -> Retry -> DLQ] -> Output
        // Panic(Disconnect) simulates transient failure.
        // Retry handles it up to N times.
        // If max attempts reached, DLQ catches it.
        // Note: Middleware application order is [Panic, Retry, DLQ] in list to wrap as DLQ(Retry(Panic(Endpoint))).

        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("dlq_in_{}", unique_id);
        let out_topic = format!("dlq_out_{}", unique_id);
        let dlq_topic = format!("dlq_target_{}", unique_id);
        let input = Endpoint::new_memory(&in_topic, 10);
        let dlq_endpoint = Endpoint::new_memory(&dlq_topic, 10);

        let mut output = Endpoint::new_memory(&out_topic, 10);
        output.middlewares = vec![
            // Inner-most: Fail always
            Middleware::RandomPanic(RandomPanicMiddleware {
                mode: FaultMode::Timeout, // Returns Retryable error, does NOT cause route restart
                trigger_on_message: None, // Fail always
                enabled: true,
                ..Default::default()
            }),
            // Middle: Retry
            Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 10,
                max_interval_ms: 100,
                multiplier: 1.0,
            }),
            // Outer-most: DLQ
            Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: dlq_endpoint.clone(),
            })),
        ];

        let route = Route::new(input.clone(), output).with_fault_injection(true);
        route.deploy("test_dlq_integration").await.unwrap();

        // Send message
        let input_ch = input.channel().unwrap();
        input_ch.send_message("fail_msg".into()).await.unwrap();

        // Verify:
        // Output channel is empty (msg failed to go there)
        // DLQ channel has message

        let dlq_ch = dlq_endpoint.channel().unwrap();

        // Wait for DLQ
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let batch = dlq_ch.drain_messages();
                if !batch.is_empty() {
                    return batch[0].clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Timed out waiting for DLQ");

        assert_eq!(received.get_payload_str(), "fail_msg");

        let out_ch_target = mq_bridge::endpoints::memory::get_or_create_channel(
            &mq_bridge::models::MemoryConfig::new(&out_topic, None),
        );
        assert!(out_ch_target.is_empty(), "Message should not reach target");

        Route::stop("test_dlq_integration").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_large_message_handling() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("large_in_{}", unique_id);
        let out_topic = format!("large_out_{}", unique_id);

        let input = Endpoint::new_memory(&in_topic, 5); // Small capacity
        let output = Endpoint::new_memory(&out_topic, 5);

        let route = Route::new(input.clone(), output.clone());
        route.deploy("test_large_msg").await.unwrap();

        let large_payload = vec![b'x'; 5 * 1024 * 1024]; // 5MB
        let input_ch = input.channel().unwrap();

        input_ch
            .send_message(large_payload.clone().into())
            .await
            .unwrap();

        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out receiving large message")
            .unwrap();

        assert_eq!(received.message.payload.len(), large_payload.len());
        assert_eq!(received.message.payload, large_payload.as_slice());

        Route::stop("test_large_msg").await;
    }

    #[test]
    fn test_map_responses_to_dispositions_unit() {
        test_map_responses_to_dispositions_logic();
    }

    // Creates a temporary SQLite file with a `messages` table, mirroring the
    // setup used by the sqlx endpoint tests.
    #[cfg(feature = "sqlx")]
    async fn setup_drain_db() -> (tempfile::TempDir, String) {
        use sqlx::Connection;
        sqlx::any::install_default_drivers();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drain.db");

        #[cfg(windows)]
        let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
        #[cfg(not(windows))]
        let url = format!("sqlite://{}", path.to_str().unwrap());

        drop(tokio::fs::File::create(&path).await.unwrap());

        let mut conn = sqlx::AnyConnection::connect(&url).await.unwrap();
        sqlx::query(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                payload BLOB NOT NULL,
                locked_until DATETIME,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        (dir, url)
    }

    // A sqlx -> memory route with exit_on_empty drains the source table and then
    // completes on its own; all rows must arrive at the output.
    #[cfg(feature = "sqlx")]
    #[tokio::test]
    async fn test_route_exit_on_empty_drains_and_stops() {
        use crate::endpoints::sqlx::SqlxPublisher;
        use crate::models::SqlxConfig;

        let (_dir, url) = setup_drain_db().await;
        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            delete_after_read: true,
            ..Default::default()
        };

        const N: usize = 25;
        let publisher = SqlxPublisher::new(&config).await.unwrap();
        for i in 0..N {
            let msg = CanonicalMessage::new(format!("row-{i}").into_bytes(), None);
            publisher.send(msg).await.unwrap();
        }

        let out_topic = format!("drain_out_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::Sqlx(config));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input, output)
            .with_batch_size(10)
            .with_exit_on_empty(true);

        // Drain the output concurrently so the route is never blocked on
        // memory-channel backpressure while it forwards rows.
        let mut verifier = route.connect_to_output("drain_verifier").await.unwrap();
        let collector = tokio::spawn(async move {
            let mut received = Vec::new();
            while received.len() < N {
                let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                    .await
                    .expect("timed out draining output")
                    .expect("output stream closed early");
                received.push(item.message.get_payload_str().to_string());
                (item.commit)(MessageDisposition::Ack).await.unwrap();
            }
            received
        });

        let handle = route.run("drain_test").await.unwrap();

        // A supervisor holding the handle (rather than consuming it via `join`)
        // must be able to observe the drain, so poll the borrowing accessor.
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("route did not exit on its own after draining");

        // A drained batch job must report success, not just "no longer running".
        assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));

        // The route future must complete on its own once the table is drained.
        handle.join().await.expect("route task panicked");

        // All N rows must have been forwarded to the memory output.
        let received = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("timed out collecting output")
            .expect("collector task panicked");
        assert_eq!(received.len(), N);
    }

    // Regression: without exit_on_empty, a drained route keeps polling and does
    // not terminate on an empty batch.
    #[cfg(feature = "sqlx")]
    #[tokio::test]
    async fn test_route_without_exit_on_empty_keeps_running() {
        use crate::models::SqlxConfig;

        let (_dir, url) = setup_drain_db().await;
        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            delete_after_read: true,
            ..Default::default()
        };

        let out_topic = format!("drain_out_running_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::Sqlx(config));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input, output).with_batch_size(10);
        let handle = route.run("no_drain_test").await.unwrap();

        // Give the route time to poll past the drained (empty) table.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The route must still be running after the source is empty.
        assert_eq!(
            handle.outcome(),
            None,
            "route exited on empty batch without exit_on_empty set"
        );

        handle.stop().await;

        // An explicit stop must be distinguishable from a drain.
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("route did not exit after stop()");
        assert_eq!(handle.outcome(), Some(RouteOutcome::Stopped));

        Route::stop("no_drain_test").await;
    }

    // A plain SQLite table with an `id`/`payload` schema but NO `locked_until` lease
    // column — used to exercise both queue-mode failure (Issue 3) and cursor-mode
    // full-table reads (Issue 2).
    #[cfg(feature = "sqlx")]
    async fn setup_plain_db(rows: usize) -> (tempfile::TempDir, String) {
        use sqlx::Connection;
        sqlx::any::install_default_drivers();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.db");

        #[cfg(windows)]
        let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
        #[cfg(not(windows))]
        let url = format!("sqlite://{}", path.to_str().unwrap());

        drop(tokio::fs::File::create(&path).await.unwrap());

        let mut conn = sqlx::AnyConnection::connect(&url).await.unwrap();
        sqlx::query(
            "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL)",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        for i in 0..rows {
            sqlx::query("INSERT INTO items (payload) VALUES (?)")
                .bind(format!("row-{i}"))
                .execute(&mut conn)
                .await
                .unwrap();
        }
        conn.close().await.unwrap();
        (dir, url)
    }

    // Issue 3 (regression): a queue-mode SQLx source (`delete_after_read`, no
    // `cursor_column`) on a table lacking the `locked_until` lease column hits an
    // unrecoverable "no such column" error. It must fail fast (RouteOutcome::Failed)
    // instead of reconnecting every reconnect_interval_ms forever.
    #[cfg(feature = "sqlx")]
    #[tokio::test]
    async fn test_route_sqlite_missing_locked_until_fails_fast() {
        use crate::models::SqlxConfig;

        let (_dir, url) = setup_plain_db(5).await;
        let config = SqlxConfig {
            url,
            table: "items".to_string(),
            delete_after_read: true, // queue-lease mode
            ..Default::default()
        };

        let out_topic = format!("plain_fail_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::Sqlx(config));
        let output = Endpoint::new_memory(&out_topic, 10);
        let route = Route::new(input, output)
            .with_exit_on_empty(true)
            // Short interval: a reconnect-loop regression times out fast instead of hanging.
            .with_reconnect_interval_ms(100);

        let handle = route.run("plain_fail_test").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("route did not fail fast on the missing-column error");
        assert_eq!(
            handle.outcome(),
            Some(RouteOutcome::Failed),
            "an unrecoverable schema error must terminate the route as Failed"
        );

        Route::stop("plain_fail_test").await;
    }

    // Issue 2: SQLite gets a full-table read via `cursor_column`, exactly like
    // Postgres (the ETL path) — read mode is config-driven, not driver-specific. A
    // cursor-mode drain of the plain table reads every row and exits Completed.
    #[cfg(feature = "sqlx")]
    #[tokio::test]
    async fn test_route_sqlite_cursor_column_drains_full_table() {
        use crate::models::SqlxConfig;

        const N: usize = 25;
        let (_dir, url) = setup_plain_db(N).await;
        let config = SqlxConfig {
            url,
            table: "items".to_string(),
            cursor_column: Some("id".to_string()),
            cursor_id: Some(format!("plain-cur-{}", fast_uuid_v7::gen_id())),
            ..Default::default()
        };

        let out_topic = format!("plain_cursor_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::Sqlx(config));
        let output = Endpoint::new_memory(&out_topic, 10);
        let route = Route::new(input, output)
            .with_batch_size(10)
            .with_exit_on_empty(true);

        let mut verifier = route
            .connect_to_output("plain_cursor_verifier")
            .await
            .unwrap();
        let collector = tokio::spawn(async move {
            let mut received = 0usize;
            while received < N {
                let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                    .await
                    .expect("timed out draining output")
                    .expect("output stream closed early");
                received += 1;
                (item.commit)(MessageDisposition::Ack).await.unwrap();
            }
            received
        });

        let handle = route.run("plain_cursor_test").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cursor route did not drain and exit");
        assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));
        handle.join().await.expect("route task panicked");

        let received = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("timed out collecting output")
            .expect("collector task panicked");
        assert_eq!(received, N);
    }

    // exit_on_empty must stop the route in concurrent mode too, not just sequential:
    // the drain reports empty so the reconnect loop stops instead of looping forever.
    #[cfg(feature = "sqlx")]
    #[tokio::test]
    async fn test_route_exit_on_empty_drains_and_stops_concurrent() {
        use crate::endpoints::sqlx::SqlxPublisher;
        use crate::models::SqlxConfig;

        let (_dir, url) = setup_drain_db().await;
        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            delete_after_read: true,
            ..Default::default()
        };

        const N: usize = 25;
        let publisher = SqlxPublisher::new(&config).await.unwrap();
        for i in 0..N {
            let msg = CanonicalMessage::new(format!("row-{i}").into_bytes(), None);
            publisher.send(msg).await.unwrap();
        }

        let out_topic = format!("drain_out_conc_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::Sqlx(config));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input, output)
            .with_batch_size(10)
            .with_concurrency(4)
            .with_exit_on_empty(true);

        let mut verifier = route
            .connect_to_output("drain_verifier_conc")
            .await
            .unwrap();
        let collector = tokio::spawn(async move {
            let mut received = Vec::new();
            while received.len() < N {
                let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                    .await
                    .expect("timed out draining output")
                    .expect("output stream closed early");
                received.push(item.message.get_payload_str().to_string());
                (item.commit)(MessageDisposition::Ack).await.unwrap();
            }
            received
        });

        let handle = route.run("drain_test_conc").await.unwrap();

        // The route future must complete on its own once the table is drained.
        tokio::time::timeout(Duration::from_secs(10), handle.join())
            .await
            .expect("concurrent route did not exit on its own after draining")
            .expect("route task panicked");

        let received = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("timed out collecting output")
            .expect("collector task panicked");
        assert_eq!(received.len(), N);
    }

    // A file -> memory route with exit_on_empty must drain the file and then stop
    // on its own. Parameterized over the two watcher-backed consumer modes:
    // Consume{delete:false} (Tail backend) and Consume{delete:true} (Queue backend).
    async fn run_file_exit_on_empty(delete: bool) {
        use crate::models::{FileConfig, FileConsumerMode};

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("drain.log");
        const N: usize = 25;
        let contents: String = (0..N).map(|i| format!("row-{i}\n")).collect();
        tokio::fs::write(&file_path, contents.as_bytes())
            .await
            .unwrap();

        let input = Endpoint::new(EndpointType::File(FileConfig {
            path: file_path.to_str().unwrap().to_string(),
            mode: Some(FileConsumerMode::Consume { delete }),
            ..Default::default()
        }));
        let out_topic = format!("file_drain_out_{}", fast_uuid_v7::gen_id());
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input, output)
            .with_batch_size(10)
            .with_exit_on_empty(true);

        let mut verifier = route
            .connect_to_output("file_drain_verifier")
            .await
            .unwrap();
        let collector = tokio::spawn(async move {
            let mut received = Vec::new();
            while received.len() < N {
                let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                    .await
                    .expect("timed out draining output")
                    .expect("output stream closed early");
                received.push(item.message.get_payload_str().to_string());
                (item.commit)(MessageDisposition::Ack).await.unwrap();
            }
            received
        });

        let handle = route.run("file_drain_test").await.unwrap();

        // The route future must complete on its own once the file is drained.
        tokio::time::timeout(Duration::from_secs(10), handle.join())
            .await
            .expect("file route did not exit on its own after draining")
            .expect("route task panicked");

        let received = tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("timed out collecting output")
            .expect("collector task panicked");
        assert_eq!(received.len(), N);
    }

    #[tokio::test]
    async fn test_route_exit_on_empty_file_tail() {
        run_file_exit_on_empty(false).await;
    }

    #[tokio::test]
    async fn test_route_exit_on_empty_file_queue() {
        run_file_exit_on_empty(true).await;
    }
}
