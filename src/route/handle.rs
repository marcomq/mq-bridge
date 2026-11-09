//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! What a running route hands back: its outcome, status and pending sends.

use super::*;

pub(super) const MAX_DROP_CAUSES: usize = 8;

/// How long a `send_batch` may stay pending before the route reports unhealthy.
pub(super) const SEND_STALL_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(60);

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
pub(super) struct OutcomeGuard {
    pub(super) outcome: Arc<RwLock<Option<RouteOutcome>>>,
    pub(super) status: Arc<RwLock<EndpointStatus>>,
    pub(super) drops: Arc<RwLock<DropReport>>,
    pub(super) resolved: Option<RouteOutcome>,
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
    pub(super) count: u64,
    pub(super) last_cause: Option<String>,
    /// Messages per distinct cause, for the first [`MAX_DROP_CAUSES`] causes seen.
    pub(super) causes: Vec<(String, u64)>,
}

/// Start time of each worker's in-flight `send_batch`, so a send that never
/// returns shows up in [`RouteHandle::status`] instead of looking healthy.
#[derive(Debug)]
pub(crate) struct PendingSends {
    pub(super) epoch: std::time::Instant,
    /// Nanoseconds since `epoch` plus one; zero while the worker is not sending.
    pub(super) slots: Box<[AtomicU64]>,
    /// Set once the sink accepted a batch at least in part: the drain rule's sign of progress.
    pub(super) delivered: std::sync::atomic::AtomicBool,
}

/// Clears its slot on drop, so a cancelled send does not read as stuck.
pub(super) struct PendingSendGuard<'a>(pub(super) &'a AtomicU64);

#[derive(Debug)]
pub struct RouteHandle {
    pub(super) handle: JoinHandle<()>,
    pub(super) shutdown_tx: Sender<()>,
    /// Live connection health of the running route, updated by the reconnect
    /// loop on every (re)connect and failure. Read via [`RouteHandle::status`].
    pub(super) status: Arc<RwLock<EndpointStatus>>,
    /// Terminal outcome, published once by the run task as it exits.
    /// `None` while the route is still running. Read via [`RouteHandle::outcome`].
    pub(super) outcome: Arc<RwLock<Option<RouteOutcome>>>,
    /// In-flight sends, folded into [`RouteHandle::status`].
    pub(super) sends: Option<Arc<PendingSends>>,
}

impl DropReport {
    pub(super) fn add(&mut self, cause: &PublisherError) {
        let cause = cause.to_string();
        self.count += 1;
        if let Some((_, count)) = self.causes.iter_mut().find(|(known, _)| *known == cause) {
            *count += 1;
        } else if self.causes.len() < MAX_DROP_CAUSES {
            self.causes.push((cause.clone(), 1));
        }
        self.last_cause = Some(cause);
    }

    /// The one cause, or the three most frequent with their counts.
    pub(super) fn causes_text(&self) -> String {
        if self.causes.len() <= 1 {
            return self
                .last_cause
                .as_deref()
                .unwrap_or("unknown cause")
                .to_string();
        }
        let mut causes: Vec<&(String, u64)> = self.causes.iter().collect();
        causes.sort_by_key(|c| std::cmp::Reverse(c.1));
        let shown: Vec<String> = causes
            .iter()
            .take(3)
            .map(|(cause, count)| format!("{count} x {cause}"))
            .collect();
        let more = if causes.len() > 3 {
            "; and other causes"
        } else {
            ""
        };
        format!("{}{more}", shown.join("; "))
    }
}

impl PendingSends {
    pub(super) fn new(workers: usize) -> Self {
        Self {
            epoch: std::time::Instant::now(),
            slots: (0..workers.max(1)).map(|_| AtomicU64::new(0)).collect(),
            delivered: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Marks worker `index` as sending until the returned guard drops.
    pub(super) fn begin(&self, index: usize) -> Option<PendingSendGuard<'_>> {
        let slot = self.slots.get(index)?;
        let started = self.epoch.elapsed().as_nanos() as u64 + 1;
        slot.store(started, Ordering::Relaxed);
        Some(PendingSendGuard(slot))
    }

    /// Age of the oldest send still pending.
    pub(super) fn oldest(&self) -> Option<std::time::Duration> {
        let started = self
            .slots
            .iter()
            .map(|slot| slot.load(Ordering::Relaxed))
            .filter(|&started| started != 0)
            .min()?;
        let now = self.epoch.elapsed().as_nanos() as u64 + 1;
        Some(std::time::Duration::from_nanos(now.saturating_sub(started)))
    }
}

impl Drop for PendingSendGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

impl OutcomeGuard {
    pub(super) fn set(&mut self, outcome: RouteOutcome) {
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
                        drops.causes_text()
                    ));
                }
            }
        }
        *recover_write_lock(&self.outcome, "route_handle_outcome") = Some(outcome);
    }
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
    /// set to the last connection error). A `send_batch` pending longer than a
    /// minute also reports unhealthy, with `details.state = "send pending"`.
    pub fn status(&self) -> EndpointStatus {
        self.status_with_stall_threshold(SEND_STALL_THRESHOLD)
    }

    pub(super) fn status_with_stall_threshold(
        &self,
        threshold: std::time::Duration,
    ) -> EndpointStatus {
        let mut status = recover_read_lock(&self.status, "route_handle_status").clone();
        let stalled = self
            .sends
            .as_ref()
            .and_then(|sends| sends.oldest())
            .filter(|age| *age >= threshold && self.outcome().is_none());
        if let Some(age) = stalled {
            status.healthy = false;
            status
                .error
                .get_or_insert_with(|| format!("send pending for {}s", age.as_secs()));
            let details = serde_json::json!({
                "state": "send pending",
                "send_pending_ms": age.as_millis() as u64,
            });
            match &mut status.details {
                serde_json::Value::Object(map) => {
                    if let serde_json::Value::Object(extra) = details {
                        map.extend(extra);
                    }
                }
                other => *other = details,
            }
        }
        status
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
            sends: None,
        }
    }
}
