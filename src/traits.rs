//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

pub use crate::errors::{ConsumerError, HandlerError, PublisherError};
pub use crate::outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
pub use futures::future::BoxFuture;
use std::any::Any;
use std::sync::Arc;
use tracing::warn;

/// A generic trait for handling messages (commands or events).
///
/// Handlers process an incoming message and can optionally return a new
/// message (e.g. a reply) via `Handled::Publish`, or acknowledge processing via `Handled::Ack`.
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError>;
}

#[async_trait]
impl<T: Handler + ?Sized> Handler for Arc<T> {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        (**self).handle(msg).await
    }
}

/// A closure that can be called to commit the message.
/// It returns a `BoxFuture` to allow for async commit operations.
pub type CommitFunc =
    Box<dyn FnOnce(Option<CanonicalMessage>) -> BoxFuture<'static, ()> + Send + 'static>;

/// A closure for committing a batch of messages.
pub type BatchCommitFunc =
    Box<dyn FnOnce(Option<Vec<CanonicalMessage>>) -> BoxFuture<'static, ()> + Send + 'static>;

#[async_trait]
pub trait MessageConsumer: Send + Sync {
    /// Receives a batch of messages. Needs to be implemented.
    /// In doubt, just implement a receive_batch that returns 1 message as vec
    /// Receives a batch of messages.
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
                    warn!(
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
        }
    }

    async fn receive_batch_helper(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let received = self.receive().await?; // The `?` now correctly handles ConsumerError
        let batch_commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            // The default implementation only handles one message, so we take the first response.
            let single_response = responses.and_then(|v| v.into_iter().next());
            (received.commit)(single_response)
        }) as BatchCommitFunc;
        Ok(ReceivedBatch {
            messages: vec![received.message],
            commit: batch_commit,
        })
    }
    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
pub trait MessagePublisher: Send + Sync + 'static {
    /// Sends a batch of messages. Endpoints needs to override this.
    /// In doubt, just implement a send_batch that returns 1 message as vec
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError>;

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.send_batch(vec![message]).await {
            Ok(SentBatch::Ack) => Ok(Sent::Ack),
            Ok(SentBatch::Partial {
                mut responses,
                failed,
            }) => {
                if !failed.is_empty() {
                    Err(PublisherError::NonRetryable(anyhow::anyhow!(
                        "Failed to send single message"
                    )))
                } else if let Some(res) = responses.as_mut().and_then(|r| r.pop()) {
                    Ok(Sent::Response(res))
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

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }
}

/// A helper function to send messages in bulk by calling `send` for each one.
/// This is useful for `MessagePublisher` implementations that don't have a native bulk sending mechanism.
pub async fn send_batch_helper<P: MessagePublisher + ?Sized>(
    publisher: &P,
    messages: Vec<CanonicalMessage>,
    callback: impl for<'a> Fn(&'a P, CanonicalMessage) -> BoxFuture<'a, Result<Sent, PublisherError>>
        + Send
        + Sync,
) -> Result<SentBatch, PublisherError> {
    let mut responses = Vec::new();
    let mut failed_messages = Vec::new();

    for msg in messages {
        match callback(publisher, msg.clone()).await {
            Ok(Sent::Response(resp)) => responses.push(resp),
            Ok(Sent::Ack) => {}
            Err(PublisherError::Retryable(e)) => {
                // A retryable error likely affects the whole connection.
                // Abort the batch and propagate the error to trigger a reconnect.
                return Err(PublisherError::Retryable(e));
            }
            Err(PublisherError::NonRetryable(_)) => {
                // A non-retryable error is specific to this message.
                // Collect it and continue with the rest of the batch.
                failed_messages.push(msg);
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
        })
    }
}

/// Converts a `BatchCommitFunc` into a `CommitFunc` by wrapping it.
/// This allows a function that commits a batch of messages to be used where a
/// function that commits a single message is expected.
pub fn into_commit_func(batch_commit: BatchCommitFunc) -> CommitFunc {
    Box::new(move |response: Option<CanonicalMessage>| {
        let single_response_vec = response.map(|resp| vec![resp]);
        batch_commit(single_response_vec)
    })
}

/// Converts a `CommitFunc` into a `BatchCommitFunc` by wrapping it.
/// This allows a function that commits a single message to be used where a
/// function that commits a batch of messages is expected. It does so by
/// extracting the first message from the response vector (if any) and passing
/// it to the underlying single-message commit function.
pub fn into_batch_commit_func(commit: CommitFunc) -> BatchCommitFunc {
    Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
        let single_response = match responses {
            Some(resp_vec) if resp_vec.len() > 1 => {
                warn!(
                    "into_batch_commit_func called with batch of {} messages; dropping all responses to avoid partial commit (incorrect usage)",
                    resp_vec.len()
                );
                None
            }
            Some(mut resp_vec) => resp_vec.pop(),
            None => None,
        };
        commit(single_response)
    })
}
