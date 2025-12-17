//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use crate::CanonicalMessage;
use async_trait::async_trait;
pub use futures::future::BoxFuture;
use std::any::Any;

/// A closure that can be called to commit the message.
/// It returns a `BoxFuture` to allow for async commit operations.
pub type CommitFunc =
    Box<dyn FnOnce(Option<CanonicalMessage>) -> BoxFuture<'static, ()> + Send + 'static>;

/// A closure for committing a batch of messages.
pub type BatchCommitFunc =
    Box<dyn FnOnce(Option<Vec<CanonicalMessage>>) -> BoxFuture<'static, ()> + Send + 'static>;

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
pub fn into_batch_commit_func(commit: CommitFunc) -> BatchCommitFunc {
    Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
        if let Some(resp_vec) = &responses {
            debug_assert!(resp_vec.len() == 1);
        };
        let single_response = responses.and_then(|resp_vec| resp_vec.into_iter().next());
        commit(single_response)
    })
}

#[async_trait]
pub trait MessageConsumer: Send + Sync {
    /// Receives a batch of messages. Needs to be implemented.
    /// In doubt, just implement a receive_batch that returns 1 message as vec
    /// Receives a batch of messages.
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)>;

    /// Receives a single message.
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let (msg_vec, batch_commit) = self.receive_batch(1).await?;
        debug_assert!(msg_vec.len() == 1);
        if let Some(msg) = msg_vec.into_iter().next() {
            Ok((msg, into_commit_func(batch_commit)))
        } else {
            Err(anyhow::anyhow!(
                "Nothing received, receiver probably closed."
            ))
        }
    }

    async fn receive_batch_helper(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (msg, single_commit) = self.receive().await?;
        let batch_commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            // The default implementation only handles one message, so we take the first response.
            let single_response = responses.and_then(|v| v.into_iter().next());
            single_commit(single_response)
        }) as BatchCommitFunc;
        Ok((vec![msg], batch_commit))
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
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)>;

    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let (result_vec, failed_msgs) = self.send_batch(vec![message]).await?;
        if !failed_msgs.is_empty() {
            Err(anyhow::anyhow!("Failed to send message"))
        } else if let Some(result) = result_vec {
            Ok(result.into_iter().next())
        } else {
            Ok(None)
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn Any;
}

/// A helper function to send messages in bulk by calling `send` for each one.
/// This is useful for `MessagePublisher` implementations that don't have a native bulk sending mechanism.
pub async fn send_batch_helper<P: MessagePublisher + ?Sized>(
    publisher: &P,
    messages: Vec<CanonicalMessage>,
    callback: impl for<'a> Fn(
            &'a P,
            CanonicalMessage,
        ) -> BoxFuture<'a, anyhow::Result<Option<CanonicalMessage>>>
        + Send
        + Sync,
) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
    let mut responses = Vec::new();
    let mut failed_messages = Vec::new();

    for msg in messages {
        match callback(publisher, msg.clone()).await {
            Ok(Some(resp)) => responses.push(resp),
            Ok(None) => {}
            Err(_) => {
                failed_messages.push(msg);
            }
        }
    }

    let responses = if responses.is_empty() {
        None
    } else {
        Some(responses)
    };

    Ok((responses, failed_messages))
}
