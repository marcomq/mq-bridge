//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

pub use crate::errors::{ConsumerError, HandlerError, PublisherError};
pub use crate::outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use dashmap::DashMap;
pub use futures::future::BoxFuture;
use once_cell::sync::Lazy;
use std::any::Any;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::warn;

pub const REPLY_PATH_KEY: &str = "mq_bridge.reply_path";
pub const ORIGINAL_FORMAT_KEY: &str = "mq_bridge.original_format";
pub const READER_MAX_MESSAGES_KEY: &str = "mq_bridge.reader.max_messages";

/// A thread-safe registry to map message IDs to their return paths.
/// This removes the need for infrastructure fields inside CanonicalMessage.
pub struct ReplyRegistry;

static REGISTRY: Lazy<DashMap<u128, (ReplySender, Instant)>> = Lazy::new(DashMap::new);

impl ReplyRegistry {
    pub fn register(id: u128, sender: ReplySender) {
        REGISTRY.insert(id, (sender, Instant::now()));
    }

    pub fn get(id: u128) -> Option<ReplySender> {
        REGISTRY.get(&id).map(|r| r.value().0.clone())
    }

    pub fn unregister(id: u128) {
        REGISTRY.remove(&id);
    }

    /// Clears all entries from the registry.
    pub fn clear() {
        REGISTRY.clear();
    }

    /// Periodically removes entries that are no longer active or have timed out.
    pub fn cleanup_stale_entries(ttl: std::time::Duration) {
        let now = Instant::now();
        REGISTRY.retain(|_, (sender, timestamp)| {
            !sender.is_closed() && now.duration_since(*timestamp) < ttl
        });
    }
}

/// A callback that allows a publisher to send a reply directly back to the source consumer.
/// This enables true end-to-end streaming by bypassing the batching in the Route.
pub type MessageReplyCallback = ReplySender;

/// A thin wrapper around a channel sender for asynchronous replies.
#[derive(Clone)]
pub struct ReplySender(mpsc::Sender<MessageDisposition>);

impl ReplySender {
    pub fn new(tx: mpsc::Sender<MessageDisposition>) -> Self {
        Self(tx)
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Sends a disposition to the channel. Returns the number of messages enqueued.
    ///
    /// For `ReplyBatch`, it attempts to send each message individually as a `Reply`.
    /// This operation is best-effort; if the channel becomes full or is closed, it returns an
    /// error containing the count of successfully sent messages.
    pub async fn send(&self, disposition: MessageDisposition) -> anyhow::Result<usize> {
        match disposition {
            MessageDisposition::ReplyBatch(mut msgs) => {
                let mut count = 0;
                while !msgs.is_empty() {
                    let msg = msgs.remove(0);
                    // Use non-blocking try_send to fail early if the channel is full, allowing
                    // callers to recover the remaining messages.
                    match self.0.try_send(MessageDisposition::Reply(msg)) {
                        Ok(_) => count += 1,
                        Err(mpsc::error::TrySendError::Full(MessageDisposition::Reply(
                            failed_msg,
                        ))) => {
                            msgs.insert(0, failed_msg);
                            return Err(anyhow::anyhow!("Failed to send batch item: channel is full. {} messages sent, {} remaining.", count, msgs.len()));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err(anyhow::anyhow!(
                                "Failed to send batch item: channel is closed. {} messages sent.",
                                count
                            ));
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(count)
            }
            MessageDisposition::Reply(_) => {
                self.0
                    .send(disposition)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to send reply: {}", e))?;
                Ok(1)
            }
            other => {
                let is_ack_nack =
                    matches!(other, MessageDisposition::Ack | MessageDisposition::Nack);
                self.0
                    .send(other)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to send disposition: {}", e))?;
                // Return 1 for Ack/Nack as they represent a single logical signal delivered to the source.
                Ok(if is_ack_nack { 1 } else { 0 })
            }
        }
    }
}

impl std::fmt::Debug for ReplySender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReplySender")
    }
}

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
    /// Acknowledge processing and send multiple replies.
    ReplyBatch(Vec<CanonicalMessage>),
    /// Negative acknowledgement (failure).
    Nack,
}

impl MessageDisposition {
    /// Returns an iterator over all reply messages in this disposition.
    pub fn replies(&self) -> Box<dyn Iterator<Item = &CanonicalMessage> + '_> {
        match self {
            MessageDisposition::Reply(msg) => Box::new(std::iter::once(msg)),
            MessageDisposition::ReplyBatch(msgs) => Box::new(msgs.iter()),
            _ => Box::new(std::iter::empty()),
        }
    }
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

/// A callback mechanism for handlers to send multiple responses asynchronously.
///
/// Handlers that implement `StreamingHandler` receive a `Yielder` and can call
/// `yielder.send()` multiple times to produce a stream of responses.
#[derive(Clone)]
pub struct Yielder {
    sender: mpsc::Sender<MessageDisposition>,
    original_message_id: u128,
    original_correlation_id: Option<String>,
    reply_callback: Option<MessageReplyCallback>,
}

impl Yielder {
    pub fn new(
        sender: mpsc::Sender<MessageDisposition>,
        original_message_id: u128,
        original_correlation_id: Option<String>,
        reply_callback: Option<MessageReplyCallback>,
    ) -> Self {
        Self {
            sender,
            original_message_id,
            original_correlation_id,
            reply_callback,
        }
    }

    /// Sends a message as part of the stream of responses.
    pub async fn send(&self, mut msg: CanonicalMessage) -> anyhow::Result<()> {
        // Ensure yielded messages maintain the original message ID for correlation/reply routing
        msg.message_id = self.original_message_id;

        // Ensure yielded messages maintain correlation with the original request
        msg.metadata
            .entry("correlation_id".to_string())
            .or_insert_with(|| {
                self.original_correlation_id
                    .clone()
                    .unwrap_or_else(|| format!("{:032x}", self.original_message_id))
            });

        if self.reply_callback.is_some() {
            msg.metadata
                .insert(REPLY_PATH_KEY.to_string(), "true".to_string());
        }

        self.sender
            .send(MessageDisposition::Reply(msg))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send yielded message: {}", e))
    }
}

/// A generic trait for handling messages (commands or events).
///
/// Handlers process an incoming message and can optionally return a new
/// message (e.g. a reply) via `Handled::Publish`, or acknowledge processing via `Handled::Ack`.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError>;

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

/// A trait for handlers that can yield multiple responses for a single input message.
///
/// This trait is separate from `Handler` to avoid breaking changes to existing handlers.
#[async_trait]
pub trait StreamingHandler: Send + Sync + 'static {
    async fn handle_stream(
        &self,
        msg: CanonicalMessage,
        yielder: Yielder,
    ) -> Result<(), HandlerError>;

    /// Tries to register a streaming handler for a specific type.
    fn register_streaming_handler(
        &self,
        _type_name: &str,
        _handler: Arc<dyn StreamingHandler>,
    ) -> Option<Arc<dyn StreamingHandler>> {
        None
    }
}

#[async_trait]
impl<F, Fut> StreamingHandler for F
where
    F: Fn(CanonicalMessage, Yielder) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), HandlerError>> + Send,
{
    async fn handle_stream(
        &self,
        msg: CanonicalMessage,
        yielder: Yielder,
    ) -> Result<(), HandlerError> {
        self(msg, yielder).await
    }
}

#[async_trait]
impl<T: StreamingHandler + ?Sized> StreamingHandler for Arc<T> {
    async fn handle_stream(
        &self,
        msg: CanonicalMessage,
        yielder: Yielder,
    ) -> Result<(), HandlerError> {
        (**self).handle_stream(msg, yielder).await
    }
    fn register_streaming_handler(
        &self,
        type_name: &str,
        handler: Arc<dyn StreamingHandler>,
    ) -> Option<Arc<dyn StreamingHandler>> {
        (**self).register_streaming_handler(type_name, handler)
    }
}

/// A closure that can be called to commit the message.
/// It returns a `BoxFuture` to allow for async commit operations.
pub type CommitFunc = Box<
    dyn Fn(MessageDisposition) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync + 'static,
>;

/// A signal for committing a batch of messages.
#[derive(Debug, Clone)]
pub enum CommitDisposition {
    /// Apply the same disposition to all messages in the batch.
    All(MessageDisposition),
    /// Apply a specific disposition to each message in the batch.
    Individual(Vec<MessageDisposition>),
}

/// A closure for committing a batch of messages.
pub type BatchCommitFunc = Box<
    dyn Fn(CommitDisposition) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync + 'static,
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

#[async_trait]
pub trait MessageConsumer: Send + Sync {
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
            tokio::task::yield_now().await;
        }
    }

    async fn receive_batch_helper(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let received = self.receive().await?;
        let batch_commit = Box::new(move |disposition: CommitDisposition| {
            let single_disposition = match disposition {
                CommitDisposition::All(d) => d,
                CommitDisposition::Individual(mut v) => v.pop().unwrap_or(MessageDisposition::Ack),
            };
            (received.commit)(single_disposition)
        }) as BatchCommitFunc;
        Ok(ReceivedBatch {
            messages: vec![received.message],
            commit: batch_commit,
        })
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: true,
            ..Default::default()
        }
    }
    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
pub trait MessagePublisher: Send + Sync + 'static {
    /// Sends a batch of messages.
    ///
    /// This method must be implemented by all publishers.
    /// If in doubt, implement `send_batch` to send messages one at a time.
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError>;

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.send_batch(vec![message]).await {
            Ok(SentBatch::Ack) => Ok(Sent::Ack),
            Ok(SentBatch::Partial {
                mut responses,
                mut failed,
                commit,
            }) => {
                if let Some((_, err)) = failed.pop() {
                    Err(err)
                } else if let Some(res) = responses.as_mut().and_then(|r| r.pop()) {
                    if let Some(c) = commit {
                        Ok(Sent::Responses(vec![res], Some(c)))
                    } else {
                        Ok(Sent::Response(res))
                    }
                } else {
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
    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
impl<T: MessagePublisher + ?Sized> MessagePublisher for Arc<T> {
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

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }
}

#[async_trait]
impl<T: MessagePublisher + ?Sized> MessagePublisher for Box<T> {
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

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }
}

/// Factory for creating custom endpoints (consumers and publishers).
#[async_trait]
pub trait CustomEndpointFactory: Send + Sync {
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

/// A helper function to send messages in bulk by calling a callback (typically `send`) for each one.
///
/// `response_disposition` defines how any responses received during the batch process are acknowledged
/// back to their downstream source. For example, if sending a request results in multiple responses
/// that require commitment, this disposition is used to commit them. Usually, this is `MessageDisposition::Ack`.
pub async fn send_batch_helper<P: MessagePublisher + ?Sized>(
    publisher: &P,
    messages: Vec<CanonicalMessage>,
    response_disposition: MessageDisposition,
    callback: impl for<'a> Fn(&'a P, CanonicalMessage) -> BoxFuture<'a, Result<Sent, PublisherError>>
        + Send
        + Sync,
) -> Result<SentBatch, PublisherError> {
    let mut responses = Vec::new();
    let mut failed_messages = Vec::new();

    let mut iter = messages.into_iter();
    while let Some(msg) = iter.next() {
        match callback(publisher, msg.clone()).await {
            Ok(Sent::Response(resp)) => responses.push(resp),
            Ok(Sent::Responses(mut resps, commit)) => {
                let _count = resps.len();
                responses.append(&mut resps);
                if let Some(c) = commit {
                    // Use CommitDisposition::All to avoid expensive per-item cloning of the disposition.
                    c(CommitDisposition::All(response_disposition.clone()))
                        .await
                        .map_err(|e| {
                            let err = anyhow::anyhow!(
                                "Failed to commit responses in send_batch_helper: {}",
                                e
                            );
                            if matches!(
                                e.downcast_ref::<PublisherError>(),
                                Some(PublisherError::NonRetryable(_))
                            ) {
                                PublisherError::NonRetryable(err)
                            } else {
                                PublisherError::Retryable(err)
                            }
                        })?;
                }
            }
            Ok(Sent::Ack) => {}
            Err(PublisherError::Retryable(e)) => {
                // A retryable error likely affects the whole connection.
                // We must return what succeeded so far (responses) and mark the rest as failed.
                failed_messages.push((msg, PublisherError::Retryable(e)));
                for m in iter {
                    failed_messages.push((
                        m,
                        PublisherError::Retryable(anyhow::anyhow!(
                            "Batch aborted due to previous error"
                        )),
                    ));
                }
                break;
            }
            Err(PublisherError::NonRetryable(e)) => {
                // A non-retryable error is specific to this message.
                // Collect it and continue with the rest of the batch.
                failed_messages.push((msg, PublisherError::NonRetryable(e)));
            }
        }
    }

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
            commit: None,
        })
    }
}

/// Converts a `BatchCommitFunc` into a `CommitFunc` by wrapping it.
/// This allows a function that commits a batch of messages to be used where a
/// function that commits a single message is expected.
pub fn into_commit_func(batch_commit: BatchCommitFunc) -> CommitFunc {
    Box::new(move |disposition: MessageDisposition| {
        batch_commit(CommitDisposition::Individual(vec![disposition]))
    })
}

/// Converts a `CommitFunc` into a `BatchCommitFunc` by wrapping it.
/// This allows a function that commits a single message to be used where a
/// function that commits a batch of messages is expected. It does so by
/// extracting the first message from the response vector (if any) and passing
/// it to the underlying single-message commit function.
pub fn into_batch_commit_func(commit: CommitFunc) -> BatchCommitFunc {
    Box::new(move |disposition: CommitDisposition| {
        let single_disposition = match disposition {
            CommitDisposition::All(d) => d,
            CommitDisposition::Individual(mut v) => {
                if v.len() > 1 {
                    warn!("into_batch_commit_func called with batch of {} messages; dropping all but the last to avoid partial commit (incorrect usage)", v.len());
                }
                v.pop().unwrap_or(MessageDisposition::Ack)
            }
        };
        commit(single_disposition)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use anyhow::anyhow;

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

        let result = send_batch_helper(
            &publisher,
            msgs.clone(),
            MessageDisposition::Ack,
            |_pub, msg| {
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
            },
        )
        .await;

        match result {
            Ok(SentBatch::Partial {
                responses,
                failed,
                commit: _,
            }) => {
                // 1. Verify response from first message
                assert!(responses.is_some());
                let resps = responses.unwrap();
                assert_eq!(resps.len(), 1);
                assert_eq!(resps[0].get_payload_str(), "resp1");

                // 2. Verify failures
                // Message 2 failed explicitly
                // Message 3 failed implicitly because batch was aborted
                assert_eq!(failed.len(), 2);
                assert_eq!(failed[0].0.get_payload_str(), "2");
                assert!(matches!(failed[0].1, PublisherError::Retryable(_)));

                assert_eq!(failed[1].0.get_payload_str(), "3");
                assert!(matches!(failed[1].1, PublisherError::Retryable(_)));
            }
            _ => panic!("Expected Partial result"),
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
                    commit: None,
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
