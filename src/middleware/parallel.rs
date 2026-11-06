//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The `parallel` input marker: the middlewares listed above it run on several batches at
//! once, each lane on a task of its own. Batches leave in the order they were read, so the
//! route sees and commits them exactly as it would without the marker.

use super::deferred_commit::{run_all, DeferredCommits};
use super::served_by_plugin;
use crate::errors::InvalidConfig;
use crate::models::{Middleware, ParallelMiddleware};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    ReceivedBatch,
};
use async_trait::async_trait;
use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::task::JoinHandle;
use tracing::Instrument;

type Slot = Arc<Mutex<Option<ReceivedBatch>>>;
type LaneOutput = (Lane, Result<ReceivedBatch, ConsumerError>);
type Recorded = Arc<Mutex<Option<Vec<MessageDisposition>>>>;

/// Lanes for `config`, after checking that every middleware above the marker is stateless.
pub(super) fn lane_count(
    config: &ParallelMiddleware,
    above: &[Middleware],
    route_name: &str,
) -> anyhow::Result<usize> {
    for middleware in above {
        if let Some(reason) = why_serial(middleware) {
            return Err(InvalidConfig(anyhow::anyhow!(
                "[middleware:{route_name}] `{}` cannot run above `parallel`: {reason}. List it below `parallel`, so it runs first, one batch at a time.",
                name_of(middleware)
            ))
            .into());
        }
    }
    match config.max_in_flight {
        Some(0) => Err(InvalidConfig(anyhow::anyhow!(
            "[middleware:{route_name}] `parallel.max_in_flight` must be at least 1"
        ))
        .into()),
        Some(n) => Ok(n),
        None => Ok(std::thread::available_parallelism().map_or(4, |n| n.get())),
    }
}

fn why_serial(middleware: &Middleware) -> Option<&'static str> {
    match middleware {
        _ if served_by_plugin(middleware) => {
            Some("this build runs it in the `builtin_middleware` plugin")
        }
        Middleware::Transform(_)
        | Middleware::Lookup(_)
        | Middleware::Id(_)
        | Middleware::Compression(_)
        | Middleware::Encryption(_) => None,
        Middleware::Parallel(_) => Some("an endpoint takes one `parallel` marker"),
        _ => Some("it keeps state across batches"),
    }
}

fn name_of(middleware: &Middleware) -> String {
    serde_json::to_value(middleware)
        .ok()
        .and_then(|value| value.as_object()?.keys().next().cloned())
        .unwrap_or_else(|| "custom".to_string())
}

/// The source of one lane: the batch it was handed, then an empty one.
pub(super) struct Feed {
    slot: Slot,
    ordered: bool,
}

impl Feed {
    pub(super) fn new(ordered: bool) -> (Self, Slot) {
        let slot = Slot::default();
        (
            Self {
                slot: slot.clone(),
                ordered,
            },
            slot,
        )
    }
}

#[async_trait]
impl MessageConsumer for Feed {
    fn commit_requires_order(&self) -> bool {
        self.ordered
    }

    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let next = self.slot.lock().expect("parallel slot poisoned").take();
        Ok(next.unwrap_or_else(ReceivedBatch::empty))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// One copy of the middlewares above the marker, fed by its own [`Feed`].
pub(super) struct Lane {
    chain: Box<dyn MessageConsumer>,
    slot: Slot,
}

impl Lane {
    pub(super) fn new(chain: Box<dyn MessageConsumer>, slot: Slot) -> Self {
        Self { chain, slot }
    }

    /// The lane's chain sees a commit that only records; the source's own commit is returned.
    fn start(mut self, batch: ReceivedBatch, max_messages: usize) -> (Running, SourceCommit) {
        let ReceivedBatch { messages, commit } = batch;
        let recorded = Recorded::default();
        let record = recorded.clone();
        let batch = ReceivedBatch {
            messages,
            commit: Box::new(move |dispositions| {
                *record.lock().expect("parallel record poisoned") = Some(dispositions);
                Box::pin(async { Ok(()) })
            }),
        };
        *self.slot.lock().expect("parallel slot poisoned") = Some(batch);
        let running = Running(tokio::spawn(
            async move {
                let result = self.chain.receive_batch(max_messages).await;
                (self, result)
            }
            .in_current_span(),
        ));
        (running, SourceCommit { commit, recorded })
    }
}

/// The source's commit for a batch in a lane, run in read order rather than from inside the
/// lane: a batch the lane rejected whole would otherwise be acked ahead of earlier ones.
struct SourceCommit {
    commit: BatchCommitFunc,
    recorded: Recorded,
}

impl SourceCommit {
    fn recorded(&self) -> Option<Vec<MessageDisposition>> {
        self.recorded
            .lock()
            .expect("parallel record poisoned")
            .take()
    }

    /// Commits the source when the route commits `batch`, after the commits held before it.
    fn hand_out(self, batch: ReceivedBatch, deferred: &mut DeferredCommits) -> ReceivedBatch {
        let in_flight = deferred.track();
        let held = deferred.take();
        let ReceivedBatch { messages, commit } = batch;
        ReceivedBatch {
            messages,
            commit: Box::new(move |dispositions| {
                Box::pin(async move {
                    commit(dispositions).await?;
                    run_all(held).await?;
                    if let Some(dispositions) = self.recorded() {
                        (self.commit)(dispositions).await?;
                    }
                    in_flight.settled();
                    Ok(())
                })
            }),
        }
    }
}

/// A lane at work; dropping it stops the task.
struct Running(JoinHandle<LaneOutput>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Future for Running {
    type Output = Result<LaneOutput, tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

enum Item {
    Ready(Result<ReceivedBatch, ConsumerError>),
    Running(Running, SourceCommit),
}

struct State {
    idle: Vec<Lane>,
    queue: VecDeque<Item>,
    reading: Option<BoxFuture<'static, Result<ReceivedBatch, ConsumerError>>>,
    /// A read failed; nothing more is read until that error has been handed out.
    stopped: bool,
    /// Commits of batches a lane rejected whole, held behind batches the route has not committed.
    deferred: DeferredCommits,
}

/// Reads batches one at a time and runs each through a free lane, handing results out in
/// read order. All state lives in `self`, so a cancelled `receive_batch` loses nothing.
pub(super) struct ParallelConsumer {
    source: Arc<tokio::sync::Mutex<Box<dyn MessageConsumer>>>,
    ordered: bool,
    lanes: usize,
    state: Mutex<State>,
}

impl ParallelConsumer {
    pub(super) fn new(source: Box<dyn MessageConsumer>, lanes: Vec<Lane>) -> Self {
        Self {
            ordered: source.commit_requires_order(),
            source: Arc::new(tokio::sync::Mutex::new(source)),
            lanes: lanes.len(),
            state: Mutex::new(State {
                idle: lanes,
                queue: VecDeque::new(),
                reading: None,
                stopped: false,
                deferred: DeferredCommits::new(),
            }),
        }
    }
}

async fn front_lane(queue: &mut VecDeque<Item>) -> Result<LaneOutput, tokio::task::JoinError> {
    match queue.front_mut() {
        Some(Item::Running(running, _)) => running.await,
        _ => std::future::pending().await,
    }
}

async fn pending_read(
    reading: &mut Option<BoxFuture<'static, Result<ReceivedBatch, ConsumerError>>>,
) -> Result<ReceivedBatch, ConsumerError> {
    match reading {
        Some(read) => read.await,
        None => std::future::pending().await,
    }
}

#[async_trait]
impl MessageConsumer for ParallelConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        if let Ok(mut source) = self.source.try_lock() {
            source.set_exit_on_empty(exit_on_empty);
        }
    }

    fn commit_requires_order(&self) -> bool {
        self.ordered
    }

    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let source = self.source.lock().await;
            match source.on_connect_hook() {
                Some(hook) => hook.await,
                None => Ok(()),
            }
        }))
    }

    /// Nacks every finished batch the route has not been handed. Lanes still at work are
    /// stopped; their batches stay uncommitted, as a cancelled read leaves them.
    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let (queue, held) = {
                let mut state = self.state.lock().expect("parallel state poisoned");
                state.reading = None;
                state.stopped = false;
                (std::mem::take(&mut state.queue), state.deferred.take())
            };
            let mut first_error = run_all(held).await.err();
            for item in queue {
                let (batch, source) = match item {
                    Item::Ready(Ok(batch)) => (batch, None),
                    Item::Running(running, source) if running.0.is_finished() => {
                        match running.await {
                            Ok((_, Ok(batch))) => (batch, Some(source)),
                            _ => continue,
                        }
                    }
                    _ => continue,
                };
                let count = batch.messages.len();
                if count == 0 {
                    continue;
                }
                let mut nacked = (batch.commit)(vec![MessageDisposition::Nack; count]).await;
                if let Some(dispositions) = source.as_ref().and_then(SourceCommit::recorded) {
                    nacked = nacked.and((source.expect("just read").commit)(dispositions).await);
                }
                if let Err(error) = nacked {
                    tracing::warn!("parallel: failed to nack an undelivered batch: {error:#}");
                }
            }
            let source = self.source.lock().await;
            if let Some(hook) = source.on_disconnect_hook() {
                if let Err(error) = hook.await {
                    first_error.get_or_insert(error);
                }
            }
            first_error.map_or(Ok(()), Err)
        }))
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let state = self.state.get_mut().expect("parallel state poisoned");
        loop {
            if matches!(state.queue.front(), Some(Item::Ready(_))) {
                let Some(Item::Ready(result)) = state.queue.pop_front() else {
                    unreachable!("front was just checked")
                };
                if result.is_err() {
                    state.stopped = false;
                }
                return result;
            }
            if state.reading.is_none()
                && !state.stopped
                && !state.idle.is_empty()
                && state.queue.len() < self.lanes
            {
                let source = self.source.clone();
                state.reading = Some(Box::pin(async move {
                    source.lock_owned().await.receive_batch(max_messages).await
                }));
            }
            if state.reading.is_none() && state.queue.is_empty() {
                return Err(ConsumerError::Connection(anyhow::anyhow!(
                    "parallel: every lane failed"
                )));
            }

            let queue = &mut state.queue;
            let reading = &mut state.reading;
            tokio::select! {
                biased;
                done = front_lane(queue) => {
                    let Some(Item::Running(_, source)) = state.queue.pop_front() else {
                        unreachable!("the front lane just finished")
                    };
                    let (lane, result) = done.map_err(|error| {
                        ConsumerError::Connection(anyhow::anyhow!("parallel lane failed: {error}"))
                    })?;
                    state.idle.push(lane);
                    let batch = result?;
                    if !batch.messages.is_empty() {
                        return Ok(source.hand_out(batch, &mut state.deferred));
                    }
                    if let Some(dispositions) = source.recorded() {
                        state
                            .deferred
                            .settle_emptied(self.ordered, source.commit, dispositions)
                            .await
                            .map_err(ConsumerError::Connection)?;
                    }
                }
                read = pending_read(reading) => {
                    state.reading = None;
                    let item = match read {
                        Ok(batch) if !batch.messages.is_empty() => {
                            let lane = state.idle.pop().expect("a read starts only with a free lane");
                            let (running, source) = lane.start(batch, max_messages);
                            Item::Running(running, source)
                        }
                        Ok(empty) => Item::Ready(Ok(empty)),
                        Err(error) => {
                            state.stopped = true;
                            Item::Ready(Err(error))
                        }
                    };
                    state.queue.push_back(item);
                }
            }
        }
    }

    async fn status(&self) -> EndpointStatus {
        self.source.lock().await.status().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, Route};
    use crate::CanonicalMessage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    type Commits = Arc<Mutex<Vec<(usize, Vec<MessageDisposition>)>>>;

    /// Batches of `size` numbered messages, then the end of the stream.
    struct Numbered {
        next: usize,
        total: usize,
        size: usize,
        commits: Commits,
    }

    #[async_trait]
    impl MessageConsumer for Numbered {
        fn commit_requires_order(&self) -> bool {
            true
        }

        async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
            if self.next >= self.total {
                return Err(ConsumerError::EndOfStream);
            }
            let first = self.next;
            let end = (first + self.size).min(self.total);
            self.next = end;
            let messages = (first..end)
                .map(|n| CanonicalMessage::new(n.to_string().into_bytes(), None))
                .collect();
            let commits = self.commits.clone();
            Ok(ReceivedBatch {
                messages,
                commit: Box::new(move |dispositions| {
                    commits.lock().unwrap().push((first, dispositions));
                    Box::pin(async { Ok(()) })
                }),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Takes longer for earlier batches, so later lanes finish first.
    struct Slow {
        inner: Box<dyn MessageConsumer>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl MessageConsumer for Slow {
        async fn receive_batch(&mut self, max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let batch = self.inner.receive_batch(max).await?;
            let first: u64 = std::str::from_utf8(&batch.messages[0].payload)
                .unwrap()
                .parse()
                .unwrap();
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(8 - first % 8)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(batch)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn parallel(
        total: usize,
        size: usize,
        lanes: usize,
    ) -> (ParallelConsumer, Commits, Arc<AtomicUsize>) {
        let commits = Commits::default();
        let source = Numbered {
            next: 0,
            total,
            size,
            commits: commits.clone(),
        };
        let (active, peak) = (Arc::default(), Arc::<AtomicUsize>::default());
        let lanes = (0..lanes)
            .map(|_| {
                let (feed, slot) = Feed::new(true);
                let chain = Slow {
                    inner: Box::new(feed),
                    active: Arc::clone(&active),
                    peak: Arc::clone(&peak),
                };
                Lane::new(Box::new(chain), slot)
            })
            .collect();
        (
            ParallelConsumer::new(Box::new(source), lanes),
            commits,
            peak,
        )
    }

    /// Acks the batch starting at `reject` inside the lane and never finishes the one at `stall`.
    struct Scripted {
        inner: Box<dyn MessageConsumer>,
        reject: usize,
        stall: usize,
    }

    #[async_trait]
    impl MessageConsumer for Scripted {
        async fn receive_batch(&mut self, max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let batch = self.inner.receive_batch(max).await?;
            match numbers(&batch).first() {
                Some(&n) if n == self.reject => {
                    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
                    self.inner.receive_batch(max).await
                }
                Some(&n) if n == self.stall => std::future::pending().await,
                _ => Ok(batch),
            }
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn scripted(total: usize, reject: usize, stall: usize) -> (ParallelConsumer, Commits) {
        let commits = Commits::default();
        let source = Numbered {
            next: 0,
            total,
            size: 1,
            commits: commits.clone(),
        };
        let lanes = (0..4)
            .map(|_| {
                let (feed, slot) = Feed::new(true);
                let chain = Scripted {
                    inner: Box::new(feed),
                    reject,
                    stall,
                };
                Lane::new(Box::new(chain), slot)
            })
            .collect();
        (ParallelConsumer::new(Box::new(source), lanes), commits)
    }

    fn committed(commits: &Commits) -> Vec<usize> {
        commits
            .lock()
            .unwrap()
            .iter()
            .map(|(first, _)| *first)
            .collect()
    }

    fn numbers(batch: &ReceivedBatch) -> Vec<usize> {
        batch
            .messages
            .iter()
            .map(|m| std::str::from_utf8(&m.payload).unwrap().parse().unwrap())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn batches_leave_in_read_order_while_lanes_overlap() {
        let (mut consumer, _, peak) = parallel(64, 1, 4);
        let mut seen = Vec::new();
        loop {
            match consumer.receive_batch(1).await {
                Ok(batch) => seen.extend(numbers(&batch)),
                Err(ConsumerError::EndOfStream) => break,
                Err(other) => panic!("{other}"),
            }
        }
        assert_eq!(seen, (0..64).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) > 1, "lanes never overlapped");
    }

    #[tokio::test]
    async fn disconnect_nacks_what_the_route_was_not_handed() {
        let (mut consumer, commits, _) = parallel(12, 2, 4);
        let first = consumer.receive_batch(2).await.unwrap();
        assert_eq!(numbers(&first), [0, 1]);

        consumer.on_disconnect_hook().unwrap().await.unwrap();
        let commits = commits.lock().unwrap().clone();
        assert!(!commits.is_empty(), "nothing read ahead");
        for (first, dispositions) in commits {
            assert!(first > 0, "the handed-out batch belongs to the route");
            assert!(dispositions
                .iter()
                .all(|d| matches!(d, MessageDisposition::Nack)));
        }
    }

    #[tokio::test]
    async fn a_batch_rejected_in_a_lane_is_acked_only_after_the_batches_before_it() {
        let (mut consumer, commits) = scripted(3, 1, usize::MAX);
        let first = consumer.receive_batch(1).await.unwrap();
        let third = consumer.receive_batch(1).await.unwrap();
        assert_eq!((numbers(&first), numbers(&third)), (vec![0], vec![2]));
        assert!(committed(&commits).is_empty(), "acked ahead of batch 0");

        (first.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert_eq!(committed(&commits), [0]);
        (third.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert_eq!(committed(&commits), [0, 1, 2]);
    }

    #[tokio::test]
    async fn disconnect_does_not_wait_for_a_stalled_lane() {
        let (mut consumer, commits) = scripted(4, usize::MAX, 1);
        let first = consumer.receive_batch(1).await.unwrap();
        assert_eq!(numbers(&first), [0]);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let hook = consumer.on_disconnect_hook().unwrap();
        tokio::time::timeout(Duration::from_secs(1), hook)
            .await
            .expect("waited for the stalled lane")
            .unwrap();
        assert!(
            !committed(&commits).contains(&1),
            "the stalled batch stays uncommitted"
        );
    }

    #[tokio::test]
    async fn a_stateful_middleware_above_the_marker_is_refused() {
        let config = ParallelMiddleware::default();
        let above: Vec<Middleware> = serde_json::from_value(
            serde_json::json!([{"buffer": {"max_messages": 2, "max_delay_ms": 5}}]),
        )
        .unwrap();
        let err = lane_count(&config, &above, "r").unwrap_err();
        assert!(err.downcast_ref::<InvalidConfig>().is_some(), "{err}");
        assert!(
            err.to_string().contains("`buffer` cannot run above"),
            "{err}"
        );

        let zero = ParallelMiddleware {
            max_in_flight: Some(0),
        };
        assert!(lane_count(&zero, &[], "r").is_err());
    }

    /// A memory route keeps every message, with a transform in the lanes. Output workers
    /// publish concurrently, so source order is only checked at concurrency 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_route_keeps_every_message_in_read_order() {
        const TOTAL: usize = 10_000;
        for concurrency in [1, 8] {
            let middlewares: Vec<Middleware> = serde_json::from_value(serde_json::json!([
                {"transform": {"expression": "{n: n, tag: 'x'}"}},
                {"parallel": {"max_in_flight": 4}},
            ]))
            .unwrap();
            let name = format!("parallel_order_{concurrency}");
            let mut input = Endpoint::new_memory(&format!("{name}_in"), TOTAL);
            for middleware in middlewares {
                input = input.add_middleware(middleware);
            }
            let output = Endpoint::new_memory(&format!("{name}_out"), TOTAL);
            let channel = input.channel().unwrap();
            let messages = (0..TOTAL)
                .map(|n| CanonicalMessage::new(format!(r#"{{"n":{n}}}"#).into_bytes(), None))
                .collect();
            channel.fill_messages(messages).await.unwrap();
            channel.close();
            let out = output.channel().unwrap();

            Route::new(input, output)
                .with_concurrency(concurrency)
                .with_batch_size(16)
                .run_until_err(&name, None, None)
                .await
                .unwrap();

            let mut got: Vec<u64> = out
                .drain_messages()
                .iter()
                .map(|m| {
                    let doc: serde_json::Value = serde_json::from_slice(&m.payload).unwrap();
                    doc["n"].as_u64().unwrap()
                })
                .collect();
            if concurrency > 1 {
                got.sort_unstable();
            }
            assert_eq!(
                got,
                (0..TOTAL as u64).collect::<Vec<_>>(),
                "concurrency {concurrency}"
            );
        }
    }
}
