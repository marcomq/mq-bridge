//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Handlers that keep `&mut` state and block, run on their own thread.
//!
//! [`ThreadedHandler`] adapts a [`BlockingHandler`] to [`Handler`]. Each message is
//! handed to one dedicated OS thread, so the handler needs no `Mutex`, never blocks a
//! Tokio worker and does not hop through the `spawn_blocking` pool (which adds latency
//! jitter). Messages are handled one at a time, in arrival order.
//!
//! The [`HandlerContext`] passed with each message can [`emit`](HandlerContext::emit)
//! any number of messages while the handler runs (e.g. one per generated sentence) and
//! reports whether the message's `correlation_id` was cancelled through a
//! [`CancelHandle`].
//!
//! ```
//! use mq_bridge::blocking_handler::{BlockingHandler, HandlerContext, ThreadedHandler};
//! use mq_bridge::{CanonicalMessage, Handled, HandlerError};
//!
//! struct Counter(u64);
//!
//! impl BlockingHandler for Counter {
//!     fn handle(&mut self, msg: CanonicalMessage, ctx: &HandlerContext) -> Result<Handled, HandlerError> {
//!         self.0 += 1;
//!         if ctx.is_cancelled() {
//!             return Ok(Handled::Ack);
//!         }
//!         Ok(Handled::Publish(msg))
//!     }
//! }
//!
//! let handler = ThreadedHandler::new(Counter(0));
//! let cancel = handler.cancel_handle();
//! cancel.cancel("turn-7");
//! ```

use crate::traits::{Handled, Handler, HandlerError, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use std::collections::{HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::oneshot;

const CORRELATION_ID_KEY: &str = "correlation_id";
/// How many cancelled correlation ids are remembered; older ones are forgotten.
const CANCEL_MEMORY: usize = 1024;

/// A handler that takes `&mut self` and may block. Run it with [`ThreadedHandler`].
pub trait BlockingHandler: Send + 'static {
    fn handle(
        &mut self,
        msg: CanonicalMessage,
        ctx: &HandlerContext,
    ) -> Result<Handled, HandlerError>;
}

impl<F> BlockingHandler for F
where
    F: FnMut(CanonicalMessage, &HandlerContext) -> Result<Handled, HandlerError> + Send + 'static,
{
    fn handle(
        &mut self,
        msg: CanonicalMessage,
        ctx: &HandlerContext,
    ) -> Result<Handled, HandlerError> {
        self(msg, ctx)
    }
}

#[derive(Default)]
struct CancelState {
    generation: AtomicU64,
    ids: Mutex<(HashSet<String>, VecDeque<String>)>,
}

impl CancelState {
    fn contains(&self, id: &str) -> bool {
        self.ids
            .lock()
            .map(|ids| ids.0.contains(id))
            .unwrap_or(false)
    }
}

/// Cancels work by `correlation_id`. Cheap to clone and usable from any thread.
///
/// Queued messages with a cancelled id are acked without running the handler; the
/// running one sees [`HandlerContext::is_cancelled`] turn `true`. The last 1024
/// cancelled ids are remembered.
#[derive(Clone)]
pub struct CancelHandle(Arc<CancelState>);

impl CancelHandle {
    pub fn cancel(&self, correlation_id: &str) {
        let Ok(mut ids) = self.0.ids.lock() else {
            return;
        };
        let (set, order) = &mut *ids;
        if set.insert(correlation_id.to_owned()) {
            order.push_back(correlation_id.to_owned());
            if order.len() > CANCEL_MEMORY {
                if let Some(old) = order.pop_front() {
                    set.remove(&old);
                }
            }
        }
        self.0.generation.fetch_add(1, Ordering::Release);
    }

    pub fn is_cancelled(&self, correlation_id: &str) -> bool {
        self.0.contains(correlation_id)
    }
}

/// Per-message context handed to [`BlockingHandler::handle`].
pub struct HandlerContext {
    correlation_id: Option<String>,
    cancel: Arc<CancelState>,
    seen_generation: AtomicU64,
    cancelled: AtomicBool,
    emitter: Option<Arc<dyn MessagePublisher>>,
    runtime: tokio::runtime::Handle,
}

impl HandlerContext {
    /// The `correlation_id` metadata of the message being handled.
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    /// `true` once this message's `correlation_id` was cancelled. One atomic load
    /// while nothing new was cancelled, so it is cheap to poll per token.
    pub fn is_cancelled(&self) -> bool {
        if self.cancelled.load(Ordering::Relaxed) {
            return true;
        }
        let Some(id) = self.correlation_id.as_deref() else {
            return false;
        };
        let now = self.cancel.generation.load(Ordering::Acquire);
        if self.seen_generation.swap(now, Ordering::Relaxed) == now {
            return false;
        }
        let cancelled = self.cancel.contains(id);
        self.cancelled.store(cancelled, Ordering::Relaxed);
        cancelled
    }

    /// Sends `msg` to the emit publisher right away and waits for it to be accepted.
    /// The message inherits this message's `correlation_id` unless it has one.
    pub fn emit(&self, mut msg: CanonicalMessage) -> Result<(), HandlerError> {
        let Some(emitter) = &self.emitter else {
            return Err(HandlerError::NonRetryable(anyhow!(
                "ThreadedHandler has no emit publisher; use ThreadedHandler::with_emitter"
            )));
        };
        if let Some(id) = &self.correlation_id {
            if !msg.has_metadata(CORRELATION_ID_KEY) {
                msg.set_metadata(CORRELATION_ID_KEY, id.clone());
            }
        }
        self.runtime.block_on(emitter.send(msg)).map(|_| ())
    }
}

struct Job {
    msg: CanonicalMessage,
    runtime: tokio::runtime::Handle,
    reply: oneshot::Sender<Result<Handled, HandlerError>>,
}

/// Runs a [`BlockingHandler`] on a dedicated thread and exposes it as a [`Handler`].
/// The thread ends when the `ThreadedHandler` is dropped.
pub struct ThreadedHandler {
    jobs: mpsc::Sender<Job>,
    cancel: Arc<CancelState>,
}

impl ThreadedHandler {
    pub fn new(handler: impl BlockingHandler) -> Self {
        Self::spawn(handler, None)
    }

    /// Like [`new`](Self::new), with `emit_to` as the target of [`HandlerContext::emit`].
    /// A [`Publisher`](crate::Publisher) passes its [`inner`](crate::Publisher::inner).
    pub fn with_emitter(handler: impl BlockingHandler, emit_to: Arc<dyn MessagePublisher>) -> Self {
        Self::spawn(handler, Some(emit_to))
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(self.cancel.clone())
    }

    fn spawn(
        mut handler: impl BlockingHandler,
        emitter: Option<Arc<dyn MessagePublisher>>,
    ) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let cancel = Arc::new(CancelState::default());
        let state = cancel.clone();
        std::thread::Builder::new()
            .name("mqb-handler".into())
            .spawn(move || {
                for job in rx {
                    // Read before the check, so a cancel racing with it bumps the generation.
                    let generation = state.generation.load(Ordering::Acquire);
                    let correlation_id =
                        job.msg.get_metadata(CORRELATION_ID_KEY).map(str::to_owned);
                    if correlation_id
                        .as_deref()
                        .is_some_and(|id| state.contains(id))
                    {
                        let _ = job.reply.send(Ok(Handled::Ack));
                        continue;
                    }
                    let ctx = HandlerContext {
                        correlation_id,
                        seen_generation: AtomicU64::new(generation),
                        cancelled: AtomicBool::new(false),
                        cancel: state.clone(),
                        emitter: emitter.clone(),
                        runtime: job.runtime,
                    };
                    let result = catch_unwind(AssertUnwindSafe(|| handler.handle(job.msg, &ctx)))
                        .unwrap_or_else(|_| {
                            Err(HandlerError::NonRetryable(anyhow!(
                                "blocking handler panicked"
                            )))
                        });
                    let _ = job.reply.send(result);
                }
            })
            .expect("failed to spawn mqb-handler thread");
        Self { jobs, cancel }
    }
}

#[async_trait]
impl Handler for ThreadedHandler {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        let (reply, result) = oneshot::channel();
        let job = Job {
            msg,
            runtime: tokio::runtime::Handle::current(),
            reply,
        };
        self.jobs
            .send(job)
            .map_err(|_| HandlerError::NonRetryable(anyhow!("handler thread has stopped")))?;
        result
            .await
            .map_err(|_| HandlerError::NonRetryable(anyhow!("handler thread has stopped")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;

    fn with_id(payload: &str, id: &str) -> CanonicalMessage {
        let mut msg = CanonicalMessage::from(payload);
        msg.set_metadata(CORRELATION_ID_KEY, id);
        msg
    }

    #[tokio::test]
    async fn keeps_mut_state_and_emits_many_per_message() {
        let sink = MemoryPublisher::new_local("blocking_handler_emit", 16);
        let channel = sink.channel();
        let emit_to: Arc<dyn MessagePublisher> = Arc::new(sink);
        let handler = ThreadedHandler::with_emitter(
            {
                let mut seen = 0;
                move |msg: CanonicalMessage, ctx: &HandlerContext| {
                    seen += 1;
                    for part in ["a", "b"] {
                        ctx.emit(CanonicalMessage::from(format!("{seen}{part}").as_str()))?;
                    }
                    Ok(Handled::Publish(msg))
                }
            },
            emit_to,
        );

        handler.handle(with_id("x", "t1")).await.unwrap();
        handler.handle(CanonicalMessage::from("y")).await.unwrap();

        let emitted = channel.drain_messages();
        let payloads: Vec<_> = emitted.iter().map(|m| m.get_payload_str()).collect();
        assert_eq!(payloads, ["1a", "1b", "2a", "2b"]);
        assert_eq!(emitted[0].get_metadata(CORRELATION_ID_KEY).unwrap(), "t1");
        assert!(!emitted[2].has_metadata(CORRELATION_ID_KEY));
    }

    #[tokio::test]
    async fn cancel_skips_queued_and_signals_running() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let handler = ThreadedHandler::new(move |msg: CanonicalMessage, ctx: &HandlerContext| {
            if msg.get_payload_str() == "long" {
                started_tx.send(()).unwrap();
                while !ctx.is_cancelled() {
                    std::thread::yield_now();
                }
                assert!(ctx.is_cancelled(), "cancellation must stay visible");
                return Ok(Handled::Ack);
            }
            Ok(Handled::Publish(msg))
        });
        let cancel = handler.cancel_handle();
        let handler = Arc::new(handler);

        let running = tokio::spawn({
            let handler = handler.clone();
            async move { handler.handle(with_id("long", "t1")).await }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        cancel.cancel("t1");
        assert!(matches!(running.await.unwrap(), Ok(Handled::Ack)));

        assert!(matches!(
            handler.handle(with_id("queued", "t1")).await,
            Ok(Handled::Ack)
        ));
        assert!(matches!(
            handler.handle(with_id("other", "t2")).await,
            Ok(Handled::Publish(_))
        ));
    }

    #[tokio::test]
    async fn a_panic_fails_only_its_message() {
        let handler = ThreadedHandler::new(|msg: CanonicalMessage, _: &HandlerContext| {
            if msg.get_payload_str() == "boom" {
                panic!("boom");
            }
            Ok(Handled::Ack)
        });
        assert!(matches!(
            handler.handle(CanonicalMessage::from("boom")).await,
            Err(HandlerError::NonRetryable(_))
        ));
        assert!(matches!(
            handler.handle(CanonicalMessage::from("ok")).await,
            Ok(Handled::Ack)
        ));
    }
}
