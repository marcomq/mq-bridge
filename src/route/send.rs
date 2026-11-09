//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Sending one batch to the output and committing it, with the failure policy.

use super::*;
use crate::endpoints::memory::failure_reply;

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
pub(super) const YIELD_EVERY_MSGS: usize = 128;

/// How many reconnects in a row a route with `exit_on_empty` may make before it is
/// declared failed, once it has delivered something. A drain job that keeps failing on
/// the same pass never reaches the empty batch it exits on. A drain that has delivered
/// nothing does not reconnect at all: see [`drain_gives_up`]. A continuous route has no
/// bound: coming back after an outage is the whole point.
pub(super) const DRAIN_MAX_RECONNECT_ATTEMPTS: usize = 10;

/// How long a connection has to stay up before the route counts as recovered: the flap
/// counter resets and the health cell goes back to healthy. Until then a reconnect keeps
/// the last error visible, so a route that only ever connects and immediately fails is
/// never reported healthy on the strength of the connect alone.
pub(super) const STABLE_RUN: std::time::Duration = std::time::Duration::from_secs(30);

tokio::task_local! {
    pub(crate) static RUN_POSITIONS: RunPositions;
}

/// Read positions of sources without a durable checkpoint, kept for one route run so a
/// reconnect continues where it was instead of starting over.
pub(crate) type RunPositions = Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>;

pub(super) type SendTaskOutput = (anyhow::Result<()>, JoinSet<()>, BatchScratch);

/// A finished send, taken from its task.
pub(super) type Reaped = Option<Result<SendTaskOutput, tokio::task::JoinError>>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransientPublishFailurePolicy {
    StopRoute,
    ReplyBadGateway,
    /// A memory input with `request_reply`: every failed message is answered with its
    /// error, and the route keeps running.
    ReplyError,
}

pub(super) struct BatchScratch {
    pub(super) message_ids: Vec<u128>,
    pub(super) request_ids: HashSet<u128>,
}

/// What a sequential send needs, owned so it can run in its own task.
pub(super) struct SendContext {
    pub(super) publisher: Arc<dyn MessagePublisher>,
    pub(super) has_retry_middleware: bool,
    pub(super) has_dlq_middleware: bool,
    pub(super) transient_failure_policy: TransientPublishFailurePolicy,
    pub(super) err_tx: Sender<anyhow::Error>,
    pub(super) commit_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    pub(super) drops: Option<Arc<RwLock<DropReport>>>,
    pub(super) sends: Option<Arc<PendingSends>>,
}

/// Whether a failed drain pass ends the route. A drain copies what is there and does not
/// wait for an endpoint to appear, so a failure before anything was delivered is final.
pub(super) fn drain_gives_up(delivered_anything: bool, consecutive_failures: usize) -> bool {
    !delivered_anything || consecutive_failures >= DRAIN_MAX_RECONNECT_ATTEMPTS
}

/// Forward a ready signal the run task already emitted but the reconnect loop
/// never observed.
///
/// The inner task signals ready onto a `bounded(1)` channel *before* its consume
/// loop, so the signal sits buffered. A drain over a small source can then reach
/// its terminal within the same scheduling window, leaving `select!` with two
/// ready branches and free to pick the terminal one at random. Without this, a
/// route that ran to completion is reported to `run()` as one that never started.
pub(super) async fn forward_buffered_ready(
    startup_notified: &mut bool,
    iter_ready_rx: &async_channel::Receiver<()>,
    ready_tx: &Sender<()>,
) {
    if !*startup_notified && iter_ready_rx.try_recv().is_ok() {
        let _ = ready_tx.send(()).await;
        *startup_notified = true;
    }
}

pub(super) async fn pause_after_empty_batch(delay_ms: u64) {
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    } else {
        tokio::task::yield_now().await;
    }
}

/// Drained commit and worker tasks yield a `JoinError` only when the task panicked, which the
/// task's own error handling never sees. A panicked commit means a batch was never acked, so
/// these must not be dropped silently.
pub(super) fn report_join_result<T>(
    res: std::result::Result<T, tokio::task::JoinError>,
    what: &str,
) {
    if let Err(e) = res {
        if e.is_cancelled() {
            debug!("{} was cancelled before completing", what);
        } else {
            error!("{} panicked: {}", what, e);
        }
    }
}

pub(super) fn report_commit_failure(err_tx: &Sender<anyhow::Error>, err: anyhow::Error) {
    if err.is::<flush_gate::Unflushed>() {
        debug!("Commit skipped: {}", err);
        return;
    }
    error!("Commit failed: {}", err);
    report_route_error(err_tx, err, "Could not send commit error to main task");
}

pub(super) fn report_route_error(
    err_tx: &Sender<anyhow::Error>,
    err: anyhow::Error,
    context: &str,
) {
    match err_tx.try_send(err) {
        Ok(_) => trace!("Reported error to main task"),
        Err(err_send) => warn!(
            error = ?err_send,
            "{}; main task might be down or busy.",
            context
        ),
    }
}

pub(super) fn transient_publish_failure_policy(
    input: &Endpoint,
    pass_through_status: bool,
) -> TransientPublishFailurePolicy {
    const MAX_DEPTH: usize = 16;

    let of = |endpoint_type: &EndpointType| match endpoint_type {
        EndpointType::Http(cfg)
            if !cfg.fire_and_forget && !cfg.receive_streamable && pass_through_status =>
        {
            TransientPublishFailurePolicy::ReplyBadGateway
        }
        EndpointType::Memory(cfg)
            if cfg.request_reply && crate::endpoints::memory::is_in_process(cfg) =>
        {
            TransientPublishFailurePolicy::ReplyError
        }
        _ => TransientPublishFailurePolicy::StopRoute,
    };
    let EndpointType::Ref(name) = &input.endpoint_type else {
        return of(&input.endpoint_type);
    };
    let mut name = name.clone();
    for _ in 0..MAX_DEPTH {
        let Some(referenced) = get_endpoint(&name) else {
            return TransientPublishFailurePolicy::StopRoute;
        };
        match referenced.endpoint_type {
            EndpointType::Ref(next) => name = next,
            other => return of(&other),
        }
    }
    TransientPublishFailurePolicy::StopRoute
}

pub(super) fn bad_gateway_reply(message_id: u128) -> MessageDisposition {
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

pub(super) fn apply_server_failure_policy(
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

/// Rejected credentials (or a server that can never serve the request): retrying a one-shot
/// job against them only delays the error. A continuous route keeps retrying, since
/// credentials can be rotated under it.
pub(super) fn is_authentication_failure(error: &anyhow::Error) -> bool {
    const MARKERS: &[&str] = &[
        "password authentication failed",
        "access_refused",
        "authentication failed",
        "access denied for user",
        "invalid username or password",
        "wrongpass",
        "needs a replica set",
    ];
    let text = format!("{error:#}").to_ascii_lowercase();
    let generic_marker_counts = !is_other_grpc_status(error);
    MARKERS.iter().any(|marker| {
        text.contains(marker) && (generic_marker_counts || *marker != "authentication failed")
    })
}

/// A gRPC status other than `Unauthenticated`, whose message may only quote a peer's failure.
#[cfg(feature = "grpc")]
pub(super) fn is_other_grpc_status(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::endpoints::grpc::GrpcStatusError>()
            .is_some_and(|status| status.code() != tonic::Code::Unauthenticated)
    })
}

#[cfg(not(feature = "grpc"))]
pub(super) fn is_other_grpc_status(_error: &anyhow::Error) -> bool {
    false
}

/// Records a batch the route discarded after a permanent sink rejection.
///
/// Dropping is deliberate — it is what stops a poison message from wedging the
/// route — but `err_tx` only carries route-*stopping* errors, so without this a
/// drop is invisible: the route resolves to [`RouteOutcome::Completed`] with
/// `error: None` and the caller cannot tell a clean run from one that threw data
/// away. [`OutcomeGuard`] publishes the tally when the route ends.
pub(super) fn record_dropped_messages(
    drops: Option<&Arc<RwLock<DropReport>>>,
    dropped: usize,
    cause: &PublisherError,
) {
    record_dropped_causes(drops, std::iter::repeat_n(cause, dropped));
}

/// [`record_dropped_messages`] with a cause per message.
pub(super) fn record_dropped_causes<'a>(
    drops: Option<&Arc<RwLock<DropReport>>>,
    causes: impl Iterator<Item = &'a PublisherError>,
) {
    let Some(drops) = drops else { return };
    let mut report = recover_write_lock(drops, "route_drop_report");
    for cause in causes {
        report.add(cause);
    }
}

impl BatchScratch {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        // Grows on demand: a large `batch_size` must not cost memory per worker up front.
        let capacity = capacity.min(32768);
        Self {
            message_ids: Vec::with_capacity(capacity),
            request_ids: HashSet::with_capacity(capacity),
        }
    }

    pub(super) fn fill_from(&mut self, messages: &[crate::CanonicalMessage]) {
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

/// Waits for the read, moving a send that finishes meanwhile into `reaped`, where it outlives
/// this future. Many sources' reads are not cancel-safe (a queue may have locked rows
/// already), so only a failed send cancels the read.
pub(super) async fn next_step(
    receive: Option<
        impl std::future::Future<Output = Result<crate::traits::ReceivedBatch, ConsumerError>>,
    >,
    sending: &mut Option<JoinHandle<SendTaskOutput>>,
    reaped: &mut Reaped,
) -> Option<Result<crate::traits::ReceivedBatch, ConsumerError>> {
    let Some(receive) = receive else {
        let joined = sending.as_mut().expect("guarded by is_some").await;
        *sending = None;
        *reaped = Some(joined);
        return None;
    };
    tokio::pin!(receive);
    loop {
        select! {
            received = &mut receive => return Some(received),
            joined = async { sending.as_mut().expect("guarded by is_some").await },
                if sending.is_some() =>
            {
                let failed = !matches!(joined, Ok((Ok(()), _, _)));
                *sending = None;
                *reaped = Some(joined);
                if failed {
                    return None;
                }
            }
        }
    }
}

/// Sends one batch in its own task and hands the commit tasks and scratch back.
pub(super) fn spawn_send(
    context: &Arc<SendContext>,
    messages: Vec<crate::CanonicalMessage>,
    commit: BatchCommitFunc,
    mut commit_tasks: JoinSet<()>,
    mut scratch: BatchScratch,
) -> JoinHandle<SendTaskOutput> {
    let context = Arc::clone(context);
    tokio::spawn(async move {
        let result = send_batch_and_commit(
            &context.publisher,
            messages,
            commit,
            context.has_retry_middleware,
            context.has_dlq_middleware,
            context.transient_failure_policy,
            &context.err_tx,
            context.commit_semaphore.as_ref(),
            &mut commit_tasks,
            &mut scratch,
            context.drops.as_ref(),
            context.sends.as_deref().map(|sends| (sends, 0)),
            // Sends never overlap, so no ticket is needed.
            None,
        )
        .await;
        (result, commit_tasks, scratch)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn send_batch_and_commit(
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
    send_slot: Option<(&PendingSends, usize)>,
    ticket: Option<OrderTicket>,
) -> anyhow::Result<()> {
    let batch_len = messages.len();
    scratch.fill_from(&messages);

    // An order-sensitive sink admits one send at a time, in source order. The wait wraps
    // the whole output chain, unless a `parallel` marker in it waits in its place.
    let sent = match ticket {
        Some(ticket) if publisher.takes_publish_turn() => {
            let (waiting, mut waiting_rx) = tokio::sync::oneshot::channel();
            let (reached, mut reached_rx) = tokio::sync::oneshot::channel();
            let turn = crate::middleware::PublishTurn {
                prev: ticket.prev,
                _release: ticket._release,
                waiting: Some(waiting),
                reached: Some(reached),
            };
            let send = crate::middleware::with_publish_turn(turn, publisher.send_batch(messages));
            tokio::pin!(send);
            // Pending while working, not while queued for the turn.
            let mut _pending = send_slot.and_then(|(sends, index)| sends.begin(index));
            let (mut waited, mut reached_turn) = (false, false);
            loop {
                tokio::select! {
                    biased;
                    sent = &mut send => break sent,
                    signal = &mut waiting_rx, if !waited => {
                        waited = true;
                        if signal.is_ok() {
                            _pending = None;
                        }
                    }
                    signal = &mut reached_rx, if !reached_turn => {
                        reached_turn = true;
                        if signal.is_ok() {
                            // Dropping a guard clears the slot, so drop the old one first.
                            _pending = None;
                            _pending = send_slot.and_then(|(sends, index)| sends.begin(index));
                        }
                    }
                }
            }
        }
        ticket => {
            let _release = match ticket {
                Some(mut ticket) => {
                    if let Some(prev) = ticket.prev.take() {
                        let _ = prev.await;
                    }
                    Some(ticket)
                }
                None => None,
            };
            let _pending = send_slot.and_then(|(sends, index)| sends.begin(index));
            publisher.send_batch(messages).await
        }
    };
    // Read-mostly: written once per run, so workers do not contend on it.
    if let Some((sends, _)) = send_slot.filter(|(s, _)| !s.delivered.load(Ordering::Relaxed)) {
        let accepted = match &sent {
            Ok(SentBatch::Ack) => true,
            Ok(SentBatch::Partial { failed, .. }) => failed.len() < batch_len,
            Err(_) => false,
        };
        if accepted {
            sends.delivered.store(true, Ordering::Relaxed);
        }
    }
    match sent {
        Ok(SentBatch::Partial { responses, failed })
            if transient_failure_policy == TransientPublishFailurePolicy::ReplyError
                && !failed.is_empty() =>
        {
            let mut dispositions = map_responses_to_dispositions(
                &scratch.message_ids,
                responses,
                &failed,
                &scratch.request_ids,
                has_dlq_middleware,
            );
            for (msg, e) in &failed {
                warn!(
                    "Answering request (ID: {:032x}) with its error: {}",
                    msg.message_id, e
                );
                if let Some(i) = scratch
                    .message_ids
                    .iter()
                    .position(|id| *id == msg.message_id)
                {
                    dispositions[i] = MessageDisposition::Reply(failure_reply(msg.message_id, e));
                }
            }
            commit(dispositions).await?;
            Ok(())
        }
        Err(e) if transient_failure_policy == TransientPublishFailurePolicy::ReplyError => {
            warn!("Answering {} request(s) with their error: {}", batch_len, e);
            let dispositions = scratch
                .message_ids
                .iter()
                .map(|id| MessageDisposition::Reply(failure_reply(*id, &e)))
                .collect();
            commit(dispositions).await?;
            Ok(())
        }
        Ok(SentBatch::Ack) => {
            for id in scratch.message_ids.iter() {
                if scratch.request_ids.contains(id) {
                    warn_reply_missing(*id);
                }
            }
            let dispositions = vec![MessageDisposition::Ack; batch_len];
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
                    report_commit_failure(&err_tx, e);
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
                    let permanent =
                        |e: &&PublisherError| matches!(e, PublisherError::NonRetryable(_));
                    for (msg, e) in failed.iter().filter(|(_, e)| permanent(&e)) {
                        error!(
                            "Dropping message (ID: {:032x}) due to non-retryable error: {}",
                            msg.message_id, e
                        );
                    }
                    record_dropped_causes(drops, failed.iter().map(|(_, e)| e).filter(permanent));
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
                record_dropped_causes(drops, failed.iter().map(|(_, e)| e));
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
                    report_commit_failure(&err_tx, e);
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
