//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use crate::CanonicalMessage;
use async_trait::async_trait;
pub use futures::future::BoxFuture;
use std::any::Any;
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ConsumerError {
    #[error("BufferedConsumer channel has been closed.")]
    ChannelClosed,
}

/// A closure that can be called to commit the message.
/// It returns a `BoxFuture` to allow for async commit operations.
pub type CommitFunc =
    Box<dyn FnOnce(Option<CanonicalMessage>) -> BoxFuture<'static, ()> + Send + 'static>;

/// A closure for committing a batch of messages.
pub type BulkCommitFunc =
    Box<dyn FnOnce(Option<Vec<CanonicalMessage>>) -> BoxFuture<'static, ()> + Send + 'static>;

#[async_trait]
pub trait MessageConsumer: Send + Sync {
    /// Receives a single message.
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)>;

    /// Receives a batch of messages. The default implementation calls `receive` once.
    /// Endpoints should override this for better performance.
    async fn receive_bulk(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BulkCommitFunc)> {
        let (msg, single_commit) = self.receive().await?;
        let bulk_commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            // The default implementation only handles one message, so we take the first response.
            let single_response = responses.and_then(|v| v.into_iter().next());
            single_commit(single_response)
        }) as BulkCommitFunc;
        Ok((vec![msg], bulk_commit))
    }

    fn as_any(&self) -> &dyn Any;
}

#[async_trait]
pub trait MessagePublisher: Send + Sync + 'static {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>>;

    /// Sends a batch of messages. The default implementation calls `send` for each.
    /// Endpoints should override this for better performance.
    async fn send_bulk(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Option<Vec<CanonicalMessage>>> {
        let mut responses = Vec::new();
        for msg in messages {
            if let Some(resp) = self.send(msg).await? {
                responses.push(resp);
            }
        }
        Ok(if responses.is_empty() {
            None
        } else {
            Some(responses)
        })
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn Any;
}
