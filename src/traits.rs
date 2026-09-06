//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

pub use crate::errors::{ConsumerError, HandlerError, PublisherError};
pub use crate::outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
pub use futures::future::BoxFuture;
use std::any::Any;
use std::sync::Arc;
use tracing::warn;

/// The disposition of a processed message.
///
/// Implements `From<Option<CanonicalMessage>>` for compatibility:
/// `None` maps to `Ack`, `Some(msg)` maps to `Reply(msg)`.
#[derive(Default, Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum MessageDisposition {
    /// Acknowledge processing (success).
    #[default]
    Ack,
    /// Acknowledge processing and send a reply.
    Reply(CanonicalMessage),
    /// Negative acknowledgement (failure).
    Nack,
}

/// How a route pass ended, for an endpoint whose close is a signal to someone else.
///
/// Read with [`disconnect_outcome`] from inside a disconnect hook. Every teardown path runs
/// the disconnect hooks — a clean finish, a shutdown and a failure alike — so an endpoint
/// that has to tell the three apart cannot do it by being closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectOutcome {
    /// The input reached its natural end and the route drained it. The only outcome that
    /// means "all the work there was to do is done".
    Completed,
    /// The route was asked to stop, or is tearing down in order to reconnect. Work may
    /// well remain: nothing says the input was exhausted.
    Stopped,
    /// The pass ended with an error.
    Failed,
}

tokio::task_local! {
    static DISCONNECT_OUTCOME: DisconnectOutcome;
}

/// How the route pass that is tearing down ended, or `None` outside a teardown.
///
/// Read it from inside a publisher's [`MessagePublisher::on_disconnect_hook`] future. The
/// route establishes it for the whole teardown, so it reaches a publisher through any depth
/// of middleware without those wrappers passing it along. A publisher reached outside a
/// route teardown — or from a task the hook spawned itself — sees `None` and should treat
/// the close as [`DisconnectOutcome::Stopped`], the reading that claims least.
pub fn disconnect_outcome() -> Option<DisconnectOutcome> {
    DISCONNECT_OUTCOME.try_with(|outcome| *outcome).ok()
}

/// Runs `teardown` with [`disconnect_outcome`] set to `outcome`.
pub(crate) async fn with_disconnect_outcome<F: std::future::Future>(
    outcome: DisconnectOutcome,
    teardown: F,
) -> F::Output {
    DISCONNECT_OUTCOME.scope(outcome, teardown).await
}

impl From<Option<CanonicalMessage>> for MessageDisposition {
    fn from(opt: Option<CanonicalMessage>) -> Self {
        match opt {
            Some(msg) => MessageDisposition::Reply(msg),
            None => MessageDisposition::Ack,
        }
    }
}

impl From<Handled> for MessageDisposition {
    fn from(handled: Handled) -> Self {
        match handled {
            Handled::Ack => MessageDisposition::Ack,
            Handled::Publish(msg) => MessageDisposition::Reply(msg),
        }
    }
}

/// A generic trait for handling messages (commands or events).
///
/// Handlers process an incoming message and can optionally return a new
/// message (e.g. a reply) via `Handled::Publish`, or acknowledge processing via `Handled::Ack`.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError>;

    /// Handle a batch one message at a time.
    ///
    /// A **retryable** or **connection** failure aborts the rest of the batch: those
    /// messages are nacked and redelivered together, so stopping early keeps the batch
    /// intact and preserves order.
    ///
    /// A **non-retryable** failure only poisons its own message. The remaining ones are
    /// still handled, because a dropped message is never redelivered — aborting here
    /// would silently discard healthy messages, and how many depends on `batch_size`.
    async fn handle_many(&self, msgs: Vec<CanonicalMessage>) -> Vec<Result<Handled, HandlerError>> {
        let mut results = Vec::with_capacity(msgs.len());
        let mut remaining = msgs.len();
        for msg in msgs {
            remaining -= 1;
            let result = self.handle(msg).await;
            type Abort = fn(anyhow::Error) -> HandlerError;
            let aborted: Option<(Abort, &'static str)> = match &result {
                Err(HandlerError::Retryable(_)) => Some((
                    HandlerError::Retryable,
                    "batch aborted after earlier retryable handler failure",
                )),
                Err(HandlerError::Connection(_)) => Some((
                    HandlerError::Connection,
                    "batch aborted after earlier handler connection failure",
                )),
                Err(HandlerError::NonRetryable(_)) | Ok(_) => None,
            };
            results.push(result);
            if let Some((make_error, reason)) = aborted {
                for _ in 0..remaining {
                    results.push(Err(make_error(anyhow!(reason))));
                }
                break;
            }
        }
        results
    }

    /// Tries to register a handler for a specific type.
    /// Returns `None` if this handler does not support registration (e.g. it's not a TypeHandler).
    fn register_handler(
        &self,
        _type_name: &str,
        _handler: Arc<dyn Handler>,
    ) -> Option<Arc<dyn Handler>> {
        None
    }
}

#[async_trait]
impl<T: Handler + ?Sized> Handler for Arc<T> {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        (**self).handle(msg).await
    }

    async fn handle_many(&self, msgs: Vec<CanonicalMessage>) -> Vec<Result<Handled, HandlerError>> {
        (**self).handle_many(msgs).await
    }

    fn register_handler(
        &self,
        type_name: &str,
        handler: Arc<dyn Handler>,
    ) -> Option<Arc<dyn Handler>> {
        (**self).register_handler(type_name, handler)
    }
}

/// A helper trait that allows implementing handlers using native `async fn` syntax
/// without the `#[async_trait]` macro.
///
/// Implementations of this trait can be adapted to `Handler` using `SimpleHandler`.
pub trait AsyncHandler: Send + Sync + 'static {
    fn handle<'a>(&'a self, msg: CanonicalMessage) -> BoxFuture<'a, Result<Handled, HandlerError>>;
}

/// A wrapper struct that adapts an `AsyncHandler` to the `Handler` trait.
pub struct SimpleHandler<T>(pub T);

#[async_trait]
impl<T: AsyncHandler> Handler for SimpleHandler<T> {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        self.0.handle(msg).await
    }
}

/// A closure that can be called to commit the message.
/// It returns a `BoxFuture` to allow for async commit operations.
pub type CommitFunc =
    Box<dyn FnOnce(MessageDisposition) -> BoxFuture<'static, anyhow::Result<()>> + Send + 'static>;

/// A closure for committing a batch of messages.
pub type BatchCommitFunc = Box<
    dyn FnOnce(Vec<MessageDisposition>) -> BoxFuture<'static, anyhow::Result<()>> + Send + 'static,
>;

/// Status information about an endpoint (Consumer or Publisher).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EndpointStatus {
    pub healthy: bool,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub details: serde_json::Value,
}
impl Default for EndpointStatus {
    fn default() -> Self {
        Self {
            healthy: true,
            target: String::new(),
            pending: None,
            capacity: None,
            error: None,
            details: serde_json::Value::Null,
        }
    }
}

/// How long a draining consumer blocks for a first message before yielding an
/// empty batch so `exit_on_empty`/`--drain` can fire. Only applied while draining;
/// the streaming path blocks indefinitely (event-driven, no added latency).
///
/// Defaults to 1s — a safety margin so a source with a momentary gap isn't declared
/// drained prematurely. Override once at process start via the
/// `MQ_BRIDGE_DRAIN_IDLE_TIMEOUT_MS` env var (e.g. `0` to yield immediately, which
/// benchmarks want so the drain tail doesn't skew wall-clock timing). Read once and
/// cached; changing the env var after first use has no effect.
pub(crate) fn drain_idle_timeout() -> std::time::Duration {
    static V: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MQ_BRIDGE_DRAIN_IDLE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_millis(1000))
    })
}

/// Awaits `fut`, but in drain mode gives up after [`drain_idle_timeout`], returning
/// `None` so a blocking consumer can surface an empty batch instead of hanging.
/// Outside drain mode it simply awaits, preserving the event-driven fast path.
///
/// `fut` **must be cancel safe**: on expiry the timeout drops it mid-poll. A future that
/// has already taken a message off the transport at that point loses it silently — and
/// drain reports completion, so nothing retries it. Channel `recv`/`recv_many`,
/// `Stream::next`, and the length-delimited IPC `recv_batch` all qualify; a hand-rolled
/// `read_exact` pair does not.
pub(crate) async fn drain_gated<F: std::future::Future>(
    exit_on_empty: bool,
    fut: F,
) -> Option<F::Output> {
    if exit_on_empty {
        tokio::time::timeout(drain_idle_timeout(), fut).await.ok()
    } else {
        Some(fut.await)
    }
}

#[async_trait]
pub trait MessageConsumer: Send + Sync {
    /// Returns an optional lifecycle hook that runs once after the consumer connection is created.
    ///
    /// The route awaits this hook before it reports itself as ready. Returning an error fails
    /// route startup and lets the outer route runner reconnect or surface the startup failure.
    ///
    /// Use this for per-connection setup that should be shared by all messages read through this
    /// consumer, such as warming a connection pool, creating SQLite tables or indexes, setting up
    /// a Kafka consumer group, or authenticating a RabbitMQ channel.
    ///
    /// ```ignore
    /// fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
    ///     Some(Box::pin(async move {
    ///         self.pool.get().await?;
    ///         self.db.execute("CREATE TABLE IF NOT EXISTS embeddings (...)").await?;
    ///         Ok(())
    ///     }))
    /// }
    /// ```
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }

    /// Returns an optional lifecycle hook that runs before the consumer is dropped.
    ///
    /// The route awaits this hook during shutdown or reconnect cleanup. Errors are logged as
    /// warnings and do not replace the route's original result.
    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }

    /// Receives a batch of messages.
    ///
    /// This method must be implemented by all consumers.
    /// If in doubt, implement `receive_batch` to return a single message as a vector.
    async fn receive_batch(&mut self, _max_messages: usize)
        -> Result<ReceivedBatch, ConsumerError>;

    /// Receives a single message.
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        // This default implementation ensures we get exactly one message,
        // looping if the underlying batch consumer returns an empty batch.
        loop {
            let mut batch = self.receive_batch(1).await?;
            if let Some(msg) = batch.messages.pop() {
                debug_assert!(batch.messages.is_empty());
                if !batch.messages.is_empty() {
                    tracing::error!(
                        "receive_batch(1) returned {} extra messages; dropping them (implementation bug)",
                        batch.messages.len()
                    );
                }
                return Ok(Received {
                    message: msg,
                    commit: into_commit_func(batch.commit),
                });
            }
            // Batch was success but empty, which is unexpected for receive(1). Loop.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }
    }

    async fn receive_batch_helper(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let received = self.receive().await?; // The `?` now correctly handles ConsumerError
        let batch_commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            // The default implementation only handles one message, so we take the first disposition.
            let single_disposition = dispositions
                .into_iter()
                .next()
                .unwrap_or(MessageDisposition::Ack);
            (received.commit)(single_disposition)
        }) as BatchCommitFunc;
        Ok(ReceivedBatch {
            messages: vec![received.message],
            commit: batch_commit,
        })
    }

    /// Informs the consumer whether the owning route will terminate on an empty
    /// batch (`exit_on_empty` / `--drain`). Called once by the route right after
    /// creation. Blocking transports use it to gate an idle timeout (via
    /// `drain_gated`) so a quiet source surfaces an empty batch instead of
    /// hanging the drain; the file tail reader also uses it to decide whether a
    /// final record with no trailing delimiter is complete (drain) or torn (live tail).
    fn set_exit_on_empty(&mut self, _exit_on_empty: bool) {}

    /// Whether this consumer's commits (acks) must be applied in the order the
    /// batches were received.
    ///
    /// Defaults to `true` (the safe choice). Cumulative-ack transports such as
    /// Kafka **must** keep this `true`: acking a later offset implicitly
    /// acks everything before it, so committing out of order would silently drop
    /// the messages in between on a crash. For these the route funnels commits
    /// through a single ordered sequencer.
    ///
    /// Transports that ack each message/batch individually (NATS JetStream,
    /// MQTT, MongoDB, in-memory) can override this to `false`. The route then runs
    /// their commits concurrently (bounded by `commit_concurrency_limit`) instead
    /// of serially, which removes the per-batch ack round trip as a throughput cap.
    fn commit_requires_order(&self) -> bool {
        true
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: true,
            ..Default::default()
        }
    }

    /// Releases this consumer's broker-side resources. The default awaits
    /// `on_disconnect_hook()` if one is present; dropping the consumer afterwards
    /// frees the underlying connection.
    ///
    /// Native Rust code rarely calls this directly — scope-based `Drop` already
    /// cleans up. It exists mainly so the language bindings can expose an explicit
    /// `close()`, since GC'd hosts (Python/Node) have no deterministic drop point.
    async fn close(&mut self) -> anyhow::Result<()> {
        if let Some(hook) = self.on_disconnect_hook() {
            hook.await?;
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
pub trait MessagePublisher: Send + Sync + 'static {
    /// Returns an optional lifecycle hook that runs once after the publisher connection is created.
    ///
    /// The route awaits this hook before it reports itself as ready. Returning an error fails
    /// route startup and lets the outer route runner reconnect or surface the startup failure.
    ///
    /// Use this for per-connection setup that should be shared by all messages published through
    /// this publisher, such as loading an embedding model, warming a connection pool, creating
    /// SQLite tables or indexes, setting up a Kafka producer transaction context, or
    /// authenticating a RabbitMQ channel.
    ///
    /// ```ignore
    /// struct SqliteEmbeddingPublisher {
    ///     model: Arc<tokio::sync::Mutex<Option<EmbeddingModel>>>,
    ///     db: sqlx::SqlitePool,
    /// }
    ///
    /// impl MessagePublisher for SqliteEmbeddingPublisher {
    ///     fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
    ///         Some(Box::pin(async move {
    ///             let mut model = self.model.lock().await;
    ///             if model.is_none() {
    ///                 *model = Some(EmbeddingModel::load("all-MiniLM-L6-v2").await?);
    ///             }
    ///             sqlx::query("CREATE INDEX IF NOT EXISTS idx_embeddings_id ON embeddings(id)")
    ///                 .execute(&self.db)
    ///                 .await?;
    ///             Ok(())
    ///         }))
    ///     }
    /// }
    /// ```
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }

    /// Returns an optional lifecycle hook that runs before the publisher is dropped.
    ///
    /// The route awaits this hook during shutdown or reconnect cleanup. Errors are logged as
    /// warnings and do not replace the route's original result.
    ///
    /// A publisher whose close is a *signal* to someone else can read
    /// [`disconnect_outcome`] from inside the returned future to tell a finished route
    /// from a stopped one.
    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }

    /// Sends a batch of messages.
    ///
    /// This method must be implemented by all publishers.
    /// If in doubt, implement `send_batch` to send messages one at a time.
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError>;

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let message_id = message.message_id;
        let expects_reply = message.metadata.contains_key("reply_to");
        match self.send_batch(vec![message]).await {
            Ok(SentBatch::Ack) => {
                if expects_reply {
                    warn!("Message {:032x} expected a reply (reply_to set), but publisher returned Ack. Response loop might be broken.", message_id);
                }
                Ok(Sent::Ack)
            }
            Ok(SentBatch::Partial {
                mut responses,
                mut failed,
            }) => {
                if let Some((_, err)) = failed.pop() {
                    Err(err)
                } else if let Some(res) = responses.as_mut().and_then(|r| r.pop()) {
                    Ok(Sent::Response(res))
                } else {
                    if expects_reply {
                        warn!("Message {:032x} expected a reply (reply_to set), but publisher returned Ack. Response loop might be broken.", message_id);
                    }
                    Ok(Sent::Ack)
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: true,
            ..Default::default()
        }
    }

    /// Whether batches must reach this publisher in the order the source produced
    /// them. The publisher-side counterpart to
    /// [`MessageConsumer::commit_requires_order`].
    ///
    /// Defaults to `false`: with `concurrency > 1` the route's workers call
    /// `send_batch` in parallel, so whole batches can land at the sink out of source
    /// order (rows keep their order *within* a batch). For an unkeyed SQL, Mongo or
    /// ClickHouse insert that is meaningless, and serialising sends would cost
    /// throughput for nothing.
    ///
    /// Override to `true` when the destination *is* an ordered log and shuffling it
    /// corrupts the result — `file` does. The route then admits one batch at a time
    /// into `send_batch`, in sequence; everything around that call still runs across
    /// the worker pool, so an ordered route measures within noise of an unordered one.
    ///
    /// Only override it where the *cost* is also low, which means a sink whose
    /// `send_batch` is already serialised internally (the file sink holds a write lock
    /// for the whole batch). Sequencing a sink that ends in a network round trip costs
    /// it the whole concurrency factor: one batch in flight instead of N.
    ///
    /// Note for keyed transports: Kafka preserves order per partition only if batches
    /// are *produced* in order, so a route with `partition_key` and `concurrency > 1`
    /// can publish two batches sharing a key in either order. Kafka leaves this `false`
    /// (global ordering would serialise an inherently parallel sink); set
    /// `concurrency: 1` if per-key order matters.
    fn requires_ordered_publish(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
impl<T: MessagePublisher + ?Sized> MessagePublisher for Arc<T> {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        (**self).on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        (**self).on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        (**self).send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        (**self).send_batch(messages).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        (**self).flush().await
    }

    async fn status(&self) -> EndpointStatus {
        (**self).status().await
    }

    fn requires_ordered_publish(&self) -> bool {
        (**self).requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }
}

#[async_trait]
impl<T: MessagePublisher + ?Sized> MessagePublisher for Box<T> {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        (**self).on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        (**self).on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        (**self).send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        (**self).send_batch(messages).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        (**self).flush().await
    }

    async fn status(&self) -> EndpointStatus {
        (**self).status().await
    }

    fn requires_ordered_publish(&self) -> bool {
        (**self).requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }
}

/// Factory for creating custom endpoints (consumers and publishers).
#[async_trait]
pub trait CustomEndpointFactory: Send + Sync + std::fmt::Debug {
    async fn create_consumer(
        &self,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Err(anyhow::anyhow!(
            "This custom endpoint does not support creating consumers"
        ))
    }
    async fn create_publisher(
        &self,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Err(anyhow::anyhow!(
            "This custom endpoint does not support creating publishers"
        ))
    }
}

/// Factory for creating custom middleware.
#[async_trait]
pub trait CustomMiddlewareFactory: Send + Sync + std::fmt::Debug {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(consumer)
    }

    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(publisher)
    }
}

/// Default number of per-message sends kept in flight concurrently by
/// [`send_batch_helper`]. Bounds in-flight work so a large batch cannot overwhelm
/// the underlying client's buffers (e.g. NATS JetStream PubAcks).
pub const SEND_BATCH_CONCURRENCY: usize = 128;

/// A helper function to send messages in bulk by calling `send` for each one.
/// This is useful for `MessagePublisher` implementations that don't have a native bulk sending mechanism.
/// Requires that "send" is implemented for the publisher. Otherwise causes an infinite loop,
/// as send is calling "send_batch" by default.
///
/// Sends are pipelined: up to [`SEND_BATCH_CONCURRENCY`] are kept in flight at once
/// via `buffer_unordered`, then responses and failures are restored to input order
/// before returning. This avoids head-of-line blocking when an early send is slow
/// while still overlapping per-message round trips (e.g. JetStream PubAcks).
pub async fn send_batch_helper<P: MessagePublisher + ?Sized>(
    publisher: &P,
    messages: Vec<CanonicalMessage>,
    callback: impl for<'a> Fn(&'a P, CanonicalMessage) -> BoxFuture<'a, Result<Sent, PublisherError>>
        + Send
        + Sync,
) -> Result<SentBatch, PublisherError> {
    use futures::stream::StreamExt;

    let mut responses = Vec::new();
    let mut failed_messages = Vec::new();

    // Pair each result with its message so failures can report the message back.
    // Poll unordered to keep slots full, then sort successful responses and
    // failures back into input order before exposing them to callers.
    let callback = &callback;
    let mut results = futures::stream::iter(messages.into_iter().enumerate().map(
        |(idx, msg)| async move {
            let result = callback(publisher, msg.clone()).await;
            (idx, msg, result)
        },
    ))
    .buffer_unordered(SEND_BATCH_CONCURRENCY);

    while let Some((idx, msg, result)) = results.next().await {
        match result {
            Ok(Sent::Response(resp)) => responses.push((idx, resp)),
            Ok(Sent::Ack) => {}
            // Each send is awaited independently, so report each message's actual
            // outcome. Transient (Retryable/Connection) failures are surfaced with
            // their error so the route can Nack them for redelivery; messages that
            // did succeed before/around the failure are not needlessly resent.
            Err(e) => failed_messages.push((idx, msg, e)),
        }
    }

    responses.sort_by_key(|(idx, _)| *idx);
    let responses: Vec<_> = responses.into_iter().map(|(_, resp)| resp).collect();
    failed_messages.sort_by_key(|(idx, _, _)| *idx);
    let failed_messages: Vec<_> = failed_messages
        .into_iter()
        .map(|(_, msg, err)| (msg, err))
        .collect();

    if failed_messages.is_empty() && responses.is_empty() {
        Ok(SentBatch::Ack)
    } else {
        Ok(SentBatch::Partial {
            responses: if responses.is_empty() {
                None
            } else {
                Some(responses)
            },
            failed: failed_messages,
        })
    }
}

/// Converts a `BatchCommitFunc` into a `CommitFunc` by wrapping it.
/// This allows a function that commits a batch of messages to be used where a
/// function that commits a single message is expected.
pub fn into_commit_func(batch_commit: BatchCommitFunc) -> CommitFunc {
    Box::new(move |disposition: MessageDisposition| {
        let batch_disposition = vec![disposition];
        batch_commit(batch_disposition)
    })
}

/// Converts a `CommitFunc` into a `BatchCommitFunc` by wrapping it.
/// This allows a function that commits a single message to be used where a
/// function that commits a batch of messages is expected. It does so by
/// extracting the first message from the response vector (if any) and passing
/// it to the underlying single-message commit function.
pub fn into_batch_commit_func(commit: CommitFunc) -> BatchCommitFunc {
    Box::new(move |mut dispositions: Vec<MessageDisposition>| {
        let single_disposition = if dispositions.len() > 1 {
            warn!(
                "into_batch_commit_func called with batch of {} messages; dropping all responses to avoid partial commit (incorrect usage)",
                dispositions.len()
            );
            // Default to Ack to avoid hanging if we can't process the batch correctly
            MessageDisposition::Ack
        } else {
            dispositions.pop().unwrap_or(MessageDisposition::Ack)
        };
        commit(single_disposition)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use anyhow::anyhow;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// Fails on the message whose payload is `poison`, with a configurable error kind.
    struct PoisonHandler {
        retryable: bool,
        handled: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Handler for PoisonHandler {
        async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
            self.handled.fetch_add(1, Ordering::SeqCst);
            if msg.get_payload_str() == "poison" {
                return Err(if self.retryable {
                    HandlerError::Retryable(anyhow!("boom"))
                } else {
                    HandlerError::NonRetryable(anyhow!("boom"))
                });
            }
            Ok(Handled::Publish(msg))
        }
    }

    /// A non-retryable failure is never redelivered, so aborting the rest of the batch
    /// would silently drop healthy messages — how many depending only on `batch_size`.
    #[tokio::test]
    async fn handle_many_keeps_going_after_a_non_retryable_failure() {
        let handled = Arc::new(AtomicUsize::new(0));
        let handler = PoisonHandler {
            retryable: false,
            handled: Arc::clone(&handled),
        };

        let results = handler
            .handle_many(vec!["one".into(), "poison".into(), "three".into()])
            .await;

        assert_eq!(
            handled.load(Ordering::SeqCst),
            3,
            "every message is handled"
        );
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0], Ok(Handled::Publish(_))));
        assert!(matches!(results[1], Err(HandlerError::NonRetryable(_))));
        assert!(
            matches!(results[2], Ok(Handled::Publish(_))),
            "a message behind a poison one must still be handled"
        );
    }

    /// Retryable failures still abort: the whole batch is nacked and redelivered
    /// together, so stopping early loses nothing and keeps the batch ordered.
    #[tokio::test]
    async fn handle_many_aborts_the_batch_after_a_retryable_failure() {
        let handled = Arc::new(AtomicUsize::new(0));
        let handler = PoisonHandler {
            retryable: true,
            handled: Arc::clone(&handled),
        };

        let results = handler
            .handle_many(vec!["one".into(), "poison".into(), "three".into()])
            .await;

        assert_eq!(handled.load(Ordering::SeqCst), 2, "aborts before the third");
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0], Ok(Handled::Publish(_))));
        assert!(matches!(results[1], Err(HandlerError::Retryable(_))));
        assert!(matches!(results[2], Err(HandlerError::Retryable(_))));
    }

    struct MockPublisher;
    #[async_trait]
    impl MessagePublisher for MockPublisher {
        async fn send_batch(
            &self,
            _msgs: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_send_batch_helper_partial_failure() {
        let publisher = MockPublisher;
        let msgs = vec![
            CanonicalMessage::from("1"),
            CanonicalMessage::from("2"),
            CanonicalMessage::from("3"),
        ];

        let result = send_batch_helper(&publisher, msgs.clone(), |_pub, msg| {
            Box::pin(async move {
                let payload = msg.get_payload_str();
                if payload == "1" {
                    Ok(Sent::Response(CanonicalMessage::from("resp1")))
                } else if payload == "2" {
                    Err(PublisherError::Retryable(anyhow!("fail")))
                } else {
                    Ok(Sent::Ack)
                }
            })
        })
        .await;

        match result {
            Ok(SentBatch::Partial { responses, failed }) => {
                assert!(responses.is_some());
                let resps = responses.unwrap();
                assert_eq!(resps.len(), 1);
                assert_eq!(resps[0].get_payload_str(), "resp1");

                // Verify failures. Sends are pipelined and awaited independently,
                // so only message 2 (the one that errored) is reported failed;
                // message 3 still succeeds (Ack) and is not needlessly resent.
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0.get_payload_str(), "2");
                assert!(matches!(failed[0].1, PublisherError::Retryable(_)));
            }
            _ => panic!("Expected Partial result"),
        }
    }

    #[tokio::test]
    async fn test_send_batch_helper_preserves_response_order() {
        // The first message resolves last (longest sleep). The helper polls sends
        // unordered internally, but responses must still come back in input order.
        let publisher = MockPublisher;
        let count = 16u64;
        let msgs: Vec<CanonicalMessage> = (0..count)
            .map(|i| CanonicalMessage::from(i.to_string()))
            .collect();

        let result = send_batch_helper(&publisher, msgs, |_pub, msg| {
            Box::pin(async move {
                let i: u64 = msg.get_payload_str().parse().unwrap();
                // Earlier messages sleep longer, so completion order is reversed.
                tokio::time::sleep(std::time::Duration::from_millis((count - i) * 2)).await;
                let mut resp = CanonicalMessage::from(msg.get_payload_str().to_string());
                resp.message_id = msg.message_id;
                Ok(Sent::Response(resp))
            })
        })
        .await
        .unwrap();

        match result {
            SentBatch::Partial { responses, failed } => {
                assert!(failed.is_empty());
                let responses = responses.expect("expected responses");
                let order: Vec<u64> = responses
                    .iter()
                    .map(|r| r.get_payload_str().parse().unwrap())
                    .collect();
                assert_eq!(
                    order,
                    (0..count).collect::<Vec<u64>>(),
                    "send_batch_helper must preserve input order",
                );
            }
            SentBatch::Ack => panic!("expected per-message responses"),
        }
    }

    #[tokio::test]
    async fn test_send_batch_helper_keeps_pipeline_full_when_early_send_is_slow() {
        let publisher = Arc::new(MockPublisher);
        let total = SEND_BATCH_CONCURRENCY + 1;
        let msgs: Vec<CanonicalMessage> = (0..total)
            .map(|i| CanonicalMessage::from(i.to_string()))
            .collect();
        let started = Arc::new(AtomicUsize::new(0));
        let all_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());

        let helper = tokio::spawn({
            let publisher = Arc::clone(&publisher);
            let started = Arc::clone(&started);
            let all_started = Arc::clone(&all_started);
            let release_first = Arc::clone(&release_first);
            async move {
                send_batch_helper(&publisher, msgs, |_pub, msg| {
                    let started = Arc::clone(&started);
                    let all_started = Arc::clone(&all_started);
                    let release_first = Arc::clone(&release_first);
                    Box::pin(async move {
                        let idx: usize = msg.get_payload_str().parse().unwrap();
                        if started.fetch_add(1, Ordering::SeqCst) + 1 == total {
                            all_started.notify_waiters();
                        }
                        if idx == 0 {
                            release_first.notified().await;
                        }
                        let mut resp = CanonicalMessage::from(idx.to_string());
                        resp.message_id = msg.message_id;
                        Ok(Sent::Response(resp))
                    })
                })
                .await
            }
        });

        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                // Register the waiter before checking so a notify_waiters() landing
                // between the check and the await is not lost (Notify doesn't buffer).
                let notified = all_started.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if started.load(Ordering::SeqCst) == total {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("a completed later send should free a slot even while the first send is blocked");

        release_first.notify_waiters();
        let result = helper.await.unwrap().unwrap();
        match result {
            SentBatch::Partial { responses, failed } => {
                assert!(failed.is_empty());
                let order: Vec<usize> = responses
                    .expect("expected responses")
                    .iter()
                    .map(|r| r.get_payload_str().parse().unwrap())
                    .collect();
                assert_eq!(order, (0..total).collect::<Vec<_>>());
            }
            SentBatch::Ack => panic!("expected per-message responses"),
        }
    }

    #[tokio::test]
    async fn test_send_propagates_single_error() {
        struct FailPublisher;
        #[async_trait]
        impl MessagePublisher for FailPublisher {
            async fn send_batch(
                &self,
                msgs: Vec<CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                // Simulate what send_batch_helper does on single failure
                Ok(SentBatch::Partial {
                    responses: None,
                    failed: vec![(
                        msgs[0].clone(),
                        PublisherError::NonRetryable(anyhow!("inner")),
                    )],
                })
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let publ = FailPublisher;
        let res = publ.send(CanonicalMessage::from("test")).await;

        assert!(res.is_err());
        match res.unwrap_err() {
            PublisherError::NonRetryable(e) => assert_eq!(e.to_string(), "inner"),
            _ => panic!("Expected NonRetryable error"),
        }
    }

    #[tokio::test]
    async fn test_simple_handler_wrapper() {
        struct MyLogic;
        impl AsyncHandler for MyLogic {
            fn handle<'a>(
                &'a self,
                _msg: CanonicalMessage,
            ) -> BoxFuture<'a, Result<Handled, HandlerError>> {
                Box::pin(async { Ok(Handled::Ack) })
            }
        }

        let handler = SimpleHandler(MyLogic);
        let res = handler.handle(CanonicalMessage::from("test")).await;
        assert!(matches!(res, Ok(Handled::Ack)));
    }
}
