use crate::models::ComputeHandler;
use crate::traits::{BatchCommitFunc, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;

pub struct ComputeConsumer {
    inner: Box<dyn MessageConsumer>,
    handler: ComputeHandler,
}

impl ComputeConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, handler: ComputeHandler) -> Self {
        Self { inner, handler }
    }
}

#[async_trait]
impl MessageConsumer for ComputeConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let (msg, commit) = self.inner.receive().await?;
        let processed = self.handler.0.run(msg).await?;
        Ok((processed, commit))
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (msgs, commit) = self.inner.receive_batch(max_messages).await?;
        let mut processed_msgs = Vec::with_capacity(msgs.len());
        for msg in msgs {
            processed_msgs.push(self.handler.0.run(msg).await?);
        }
        Ok((processed_msgs, commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct ComputePublisher {
    inner: Box<dyn MessagePublisher>,
    handler: ComputeHandler,
}

impl ComputePublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, handler: ComputeHandler) -> Self {
        Self { inner, handler }
    }
}

#[async_trait]
impl MessagePublisher for ComputePublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let processed = self.handler.0.run(message).await?;
        self.inner.send(processed).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        let mut processed_msgs = Vec::with_capacity(messages.len());
        for msg in messages {
            processed_msgs.push(self.handler.0.run(msg).await?);
        }
        self.inner.send_batch(processed_msgs).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
    use crate::models::MemoryConfig;
    use crate::CanonicalMessage;

    #[tokio::test]
    async fn test_compute_consumer_lambda() {
        let config = MemoryConfig {
            topic: "test_compute_in".to_string(),
            capacity: Some(10),
        };
        let memory_consumer = MemoryConsumer::new(&config).unwrap();
        let channel = memory_consumer.channel();

        // Push a message to the source
        let msg = CanonicalMessage::new(b"original".to_vec(), None);
        channel.send_message(msg).await.unwrap();

        // Create the compute handler using a lambda
        let handler = ComputeHandler::new(|mut msg: CanonicalMessage| async move {
            let mut new_payload = b"computed_".to_vec();
            new_payload.extend_from_slice(&msg.payload);
            msg.payload = new_payload.into();
            Ok(msg)
        });

        let mut consumer = ComputeConsumer::new(Box::new(memory_consumer), handler);

        let (received, _) = consumer.receive().await.unwrap();
        assert_eq!(received.payload, "computed_original".as_bytes());
    }

    #[tokio::test]
    async fn test_compute_publisher_lambda() {
        let config = MemoryConfig {
            topic: "test_compute_out".to_string(),
            capacity: Some(10),
        };
        let memory_publisher = MemoryPublisher::new(&config).unwrap();
        let channel = memory_publisher.channel();

        // Create the compute handler using a lambda
        let handler = ComputeHandler::new(|mut msg: CanonicalMessage| async move {
            let mut new_payload = b"computed_".to_vec();
            new_payload.extend_from_slice(&msg.payload);
            msg.payload = new_payload.into();
            Ok(msg)
        });

        let publisher = ComputePublisher::new(Box::new(memory_publisher), handler);

        let msg = CanonicalMessage::new(b"original".to_vec(), None);
        publisher.send(msg).await.unwrap();

        let received = channel.drain_messages();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].payload, "computed_original".as_bytes());
    }
}
