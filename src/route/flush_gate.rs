//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Deferred commits: a batch's source commit waits for the publisher's next
//! successful [`MessagePublisher::flush`], for a publisher that declares
//! [`MessagePublisher::deferred_commit_interval`].
//!
//! A held commit asks the gate task for a go. The task flushes once the interval
//! elapses, `max_held` commits wait, a commit is urgent (a reply or a failed send
//! is waiting on it), the source runs dry, or the route winds down. Commits that
//! arrive while a flush runs wait for the next one, so a flush only ever releases
//! batches whose `send_batch` returned before it started.
//!
//! A failed flush, or a route that fails, releases nothing: every held commit fails
//! and the ordered sequencer is closed, so no later batch commits past the gap. The
//! source redelivers those batches after the reconnect.

use super::{report_route_error, SequencerItem};
use crate::traits::{BatchCommitFunc, MessageDisposition, MessagePublisher};
use async_channel::Sender;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

type Go = oneshot::Sender<Result<(), String>>;

enum GateEvent {
    Hold {
        go: Go,
        urgent: bool,
    },
    /// The source returned an empty batch: nothing new is coming soon.
    Nudge,
    /// The route is winding down cleanly: flush now, and at once for every later batch.
    Close,
    /// The route failed: release nothing.
    Abandon,
    Shutdown,
}

/// Why a held commit did not run. The gate reports the cause once, so callers stay quiet.
#[derive(Debug)]
pub(super) struct Unflushed(String);

impl std::fmt::Display for Unflushed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unflushed {}

fn unflushed(reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Unflushed(reason.into()))
}

const UNFLUSHED: &str =
    "the route stopped before the output was flushed; the batch stays uncommitted for redelivery";

pub(super) struct FlushGate {
    tx: mpsc::UnboundedSender<GateEvent>,
    handle: Option<JoinHandle<()>>,
}

impl FlushGate {
    /// `sequencer` is the ordered commit sequencer, closed when held commits are dropped.
    pub(super) fn spawn(
        publisher: Arc<dyn MessagePublisher>,
        interval: Duration,
        max_held: usize,
        sequencer: Option<Sender<(u64, SequencerItem)>>,
        err_tx: Sender<anyhow::Error>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = GateTask {
            publisher,
            interval,
            max_held: max_held.max(1),
            sequencer,
            err_tx,
            held: Vec::new(),
            closing: false,
            failed: None,
        };
        Self {
            tx,
            handle: Some(tokio::spawn(task.run(rx))),
        }
    }

    /// Wraps a batch commit so it runs only after the next successful flush.
    pub(super) fn hold(&self, commit: BatchCommitFunc) -> BatchCommitFunc {
        let tx = self.tx.clone();
        Box::new(move |dispositions| {
            Box::pin(async move {
                // Nothing was written that a flush would have to make durable.
                if dispositions
                    .iter()
                    .all(|d| matches!(d, MessageDisposition::Nack))
                {
                    return commit(dispositions).await;
                }
                let urgent = dispositions
                    .iter()
                    .any(|d| !matches!(d, MessageDisposition::Ack));
                let (go, wait) = oneshot::channel();
                if tx.send(GateEvent::Hold { go, urgent }).is_err() {
                    return Err(unflushed(UNFLUSHED));
                }
                match wait.await {
                    Ok(Ok(())) => commit(dispositions).await,
                    Ok(Err(reason)) => Err(unflushed(reason)),
                    Err(_) => Err(unflushed(UNFLUSHED)),
                }
            })
        })
    }

    pub(super) fn nudge(&self) {
        let _ = self.tx.send(GateEvent::Nudge);
    }

    pub(super) fn close(&self) {
        let _ = self.tx.send(GateEvent::Close);
    }

    pub(super) fn abandon(&self) {
        let _ = self.tx.send(GateEvent::Abandon);
    }

    /// Flushes and releases what is still held (unless abandoned), then ends the task.
    /// Await it before the commit router's shutdown: the task holds a sequencer sender.
    pub(super) async fn shutdown(mut self) {
        let _ = self.tx.send(GateEvent::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for FlushGate {
    fn drop(&mut self) {
        let _ = self.tx.send(GateEvent::Shutdown);
    }
}

struct GateTask {
    publisher: Arc<dyn MessagePublisher>,
    interval: Duration,
    max_held: usize,
    sequencer: Option<Sender<(u64, SequencerItem)>>,
    err_tx: Sender<anyhow::Error>,
    held: Vec<Go>,
    closing: bool,
    /// Set once nothing may be released any more; later holds fail with it.
    failed: Option<String>,
}

impl GateTask {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<GateEvent>) {
        let mut deadline = tokio::time::Instant::now() + self.interval;
        loop {
            let flush_now = tokio::select! {
                event = rx.recv() => match event {
                    Some(GateEvent::Hold { go, urgent }) => {
                        if let Some(reason) = &self.failed {
                            self.close_sequencer();
                            let _ = go.send(Err(reason.clone()));
                            continue;
                        }
                        self.held.push(go);
                        urgent || self.closing || self.held.len() >= self.max_held
                    }
                    Some(GateEvent::Nudge) => !self.held.is_empty(),
                    Some(GateEvent::Close) => {
                        self.closing = true;
                        !self.held.is_empty()
                    }
                    Some(GateEvent::Abandon) => {
                        self.fail(UNFLUSHED.to_string());
                        continue;
                    }
                    Some(GateEvent::Shutdown) | None => {
                        if self.failed.is_none() && !self.held.is_empty() {
                            self.flush().await;
                        }
                        self.fail(UNFLUSHED.to_string());
                        break;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => !self.held.is_empty(),
            };
            if flush_now {
                self.flush().await;
            }
            if flush_now || tokio::time::Instant::now() >= deadline {
                deadline = tokio::time::Instant::now() + self.interval;
            }
        }
    }

    /// Flushes once and releases the commits held before the flush began.
    async fn flush(&mut self) {
        let released = std::mem::take(&mut self.held);
        debug!(
            commits = released.len(),
            "Flushing output before committing"
        );
        match self.publisher.flush().await {
            Ok(()) => {
                for go in released {
                    let _ = go.send(Ok(()));
                }
            }
            Err(e) => {
                self.held = released;
                warn!(error = %e, "Flushing the output failed; held commits are dropped for redelivery");
                self.fail(format!(
                    "flushing the output failed, the batch stays uncommitted for redelivery: {e:#}"
                ));
                report_route_error(
                    &self.err_tx,
                    e.context("Flushing the output before committing failed"),
                    "Could not report the flush error to the main task",
                );
            }
        }
    }

    /// Fails every held commit and closes the sequencer, so nothing commits past them.
    fn fail(&mut self, reason: String) {
        if !self.held.is_empty() {
            self.close_sequencer();
        }
        for go in self.held.drain(..) {
            let _ = go.send(Err(reason.clone()));
        }
        self.failed.get_or_insert(reason);
    }

    fn close_sequencer(&self) {
        if let Some(sequencer) = &self.sequencer {
            sequencer.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::extensions::register_endpoint_factory;
    use crate::models::{Endpoint, EndpointType};
    use crate::traits::{
        ConsumerError, CustomEndpointFactory, MessageConsumer, MessageDisposition,
        MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
    };
    use crate::{CanonicalMessage, Route};
    use std::any::Any;
    use std::collections::{BTreeSet, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A queue that forgets an item only when its commit runs, plus what the sink made durable.
    #[derive(Default)]
    struct World {
        pending: Mutex<BTreeSet<u64>>,
        sent: Mutex<Vec<u64>>,
        durable: Mutex<HashSet<u64>>,
        committed: Mutex<Vec<u64>>,
        /// Commits that ran for a batch no successful flush covered.
        early_commits: AtomicUsize,
        flushes: AtomicUsize,
        fail_next_flush: AtomicBool,
    }

    #[derive(Debug)]
    struct Factory {
        world: Arc<WorldRef>,
        ordered: bool,
        /// After the last item: `true` returns empty batches, `false` blocks.
        ends: bool,
        interval: Duration,
    }

    struct WorldRef(Arc<World>);

    impl std::fmt::Debug for WorldRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("World")
        }
    }

    struct Source {
        world: Arc<World>,
        cursor: std::vec::IntoIter<u64>,
        ordered: bool,
        ends: bool,
    }

    #[async_trait::async_trait]
    impl MessageConsumer for Source {
        async fn receive_batch(&mut self, _: usize) -> Result<ReceivedBatch, ConsumerError> {
            let Some(item) = self.cursor.next() else {
                if !self.ends {
                    std::future::pending::<()>().await;
                }
                return Ok(ReceivedBatch {
                    messages: Vec::new(),
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                });
            };
            let world = Arc::clone(&self.world);
            Ok(ReceivedBatch {
                messages: vec![CanonicalMessage::from(item.to_string().as_str())],
                commit: Box::new(move |dispositions| {
                    Box::pin(async move {
                        if matches!(dispositions[..], [MessageDisposition::Ack]) {
                            if !world.durable.lock().unwrap().contains(&item) {
                                world.early_commits.fetch_add(1, Ordering::SeqCst);
                            }
                            world.pending.lock().unwrap().remove(&item);
                            world.committed.lock().unwrap().push(item);
                        }
                        Ok(())
                    })
                }),
            })
        }

        fn commit_requires_order(&self) -> bool {
            self.ordered
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct Sink {
        world: Arc<World>,
        interval: Duration,
    }

    #[async_trait::async_trait]
    impl MessagePublisher for Sink {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut sent = self.world.sent.lock().unwrap();
            for message in messages {
                sent.push(message.get_payload_str().parse().unwrap());
            }
            Ok(SentBatch::Ack)
        }

        async fn flush(&self) -> anyhow::Result<()> {
            let snapshot = self.world.sent.lock().unwrap().clone();
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.world.flushes.fetch_add(1, Ordering::SeqCst);
            if self.world.fail_next_flush.swap(false, Ordering::SeqCst) {
                anyhow::bail!("disk full");
            }
            self.world.durable.lock().unwrap().extend(snapshot);
            Ok(())
        }

        fn deferred_commit_interval(&self) -> Option<Duration> {
            Some(self.interval)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl CustomEndpointFactory for Factory {
        async fn create_consumer(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            let world = Arc::clone(&self.world.0);
            let items: Vec<u64> = world.pending.lock().unwrap().iter().copied().collect();
            Ok(Box::new(Source {
                world,
                cursor: items.into_iter(),
                ordered: self.ordered,
                ends: self.ends,
            }))
        }

        async fn create_publisher(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(Sink {
                world: Arc::clone(&self.world.0),
                interval: self.interval,
            }))
        }
    }

    fn route(
        items: u64,
        ordered: bool,
        ends: bool,
        interval: Duration,
        concurrency: usize,
    ) -> (Route, Arc<World>) {
        let world = Arc::new(World::default());
        world.pending.lock().unwrap().extend(0..items);
        let name = format!("flush_gate_{}", fast_uuid_v7::gen_id());
        let factory = Factory {
            world: Arc::new(WorldRef(Arc::clone(&world))),
            ordered,
            ends,
            interval,
        };
        register_endpoint_factory(&name, Arc::new(factory)).unwrap();
        let endpoint = || {
            Endpoint::new(EndpointType::Custom {
                name: name.clone(),
                config: serde_json::Value::Null,
            })
        };
        let route = Route::new(endpoint(), endpoint())
            .with_concurrency(concurrency)
            .with_batch_size(1)
            .with_commit_concurrency_limit(8)
            .with_exit_on_empty(ends);
        (route, world)
    }

    /// A long interval: only the held-commit limit and the drain's close can flush.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn commits_wait_for_a_flush_and_never_wedge_the_reader() {
        for (concurrency, ordered) in [(1, true), (1, false), (4, true), (4, false)] {
            let (route, world) = route(200, ordered, true, Duration::from_secs(3600), concurrency);
            let run = route.run_until_err("flush_gate_drain", None, None);
            let result = tokio::time::timeout(Duration::from_secs(20), run)
                .await
                .expect("the route wedged on held commits");
            assert!(result.is_ok(), "{result:?}");

            let case = format!("concurrency {concurrency}, ordered {ordered}");
            assert!(world.pending.lock().unwrap().is_empty(), "{case}");
            assert_eq!(world.early_commits.load(Ordering::SeqCst), 0, "{case}");
            assert!(world.flushes.load(Ordering::SeqCst) > 1, "{case}");
            if ordered {
                let committed = world.committed.lock().unwrap();
                assert!(committed.windows(2).all(|w| w[0] < w[1]), "{case}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_interval_flushes_a_route_whose_source_stays_open() {
        let (route, world) = route(3, true, false, Duration::from_millis(50), 1);
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        let run = tokio::spawn(async move {
            route
                .run_until_err("flush_gate_tick", Some(stop_rx), None)
                .await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !world.pending.lock().unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "never committed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(world.early_commits.load(Ordering::SeqCst), 0);
        stop_tx.send(()).await.unwrap();
        assert!(run.await.unwrap().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_flush_commits_nothing_and_the_source_redelivers() {
        for concurrency in [1, 4] {
            let (route, world) = route(20, true, true, Duration::from_secs(3600), concurrency);
            world.fail_next_flush.store(true, Ordering::SeqCst);

            let err = route
                .run_until_err("flush_gate_fail", None, None)
                .await
                .expect_err("a failed flush fails the pass");
            assert!(format!("{err:#}").contains("disk full"), "{err:#}");
            let committed_before = world.committed.lock().unwrap().len();
            assert_eq!(world.early_commits.load(Ordering::SeqCst), 0);

            // The reconnect: a new pass reads what is still uncommitted.
            route
                .run_until_err("flush_gate_fail", None, None)
                .await
                .unwrap();
            assert!(world.pending.lock().unwrap().is_empty());
            assert_eq!(world.early_commits.load(Ordering::SeqCst), 0);
            assert!(world.sent.lock().unwrap().len() > 20 - committed_before);
            let committed = world.committed.lock().unwrap();
            assert!(committed.windows(2).all(|w| w[0] < w[1]), "{committed:?}");
        }
    }
}
