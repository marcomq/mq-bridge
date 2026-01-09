use crate::models::DelayMiddleware;
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received, ReceivedBatch,
    Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::time::Duration;

pub struct DelayConsumer {
    inner: Box<dyn MessageConsumer>,
    delay: Duration,
}

impl DelayConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &DelayMiddleware) -> Self {
        Self {
            inner,
            delay: Duration::from_millis(config.delay_ms),
        }
    }
}

#[async_trait]
impl MessageConsumer for DelayConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        tokio::time::sleep(self.delay).await;
        self.inner.receive().await
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        tokio::time::sleep(self.delay).await;
        self.inner.receive_batch(max_messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct DelayPublisher {
    inner: Box<dyn MessagePublisher>,
    delay: Duration,
}

impl DelayPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &DelayMiddleware) -> Self {
        Self {
            inner,
            delay: Duration::from_millis(config.delay_ms),
        }
    }
}

#[async_trait]
impl MessagePublisher for DelayPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        tokio::time::sleep(self.delay).await;
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        tokio::time::sleep(self.delay).await;
        self.inner.send_batch(messages).await
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
    use std::time::Instant;

    #[tokio::test]
    async fn test_delay_consumer() {
        let config = DelayMiddleware { delay_ms: 50 };
        let mem_cfg = MemoryConfig {
            topic: "delay_test_in".to_string(),
            capacity: Some(10),
        };
        let mem_consumer = MemoryConsumer::new(&mem_cfg).unwrap();
        let channel = mem_consumer.channel();
        channel
            .send_message(CanonicalMessage::from("test"))
            .await
            .unwrap();

        let mut consumer = DelayConsumer::new(Box::new(mem_consumer), &config);

        let start = Instant::now();
        let _ = consumer.receive().await.unwrap();
        let elapsed = start.elapsed();

        assert!(elapsed >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_delay_publisher() {
        let config = DelayMiddleware { delay_ms: 50 };
        let mem_cfg = MemoryConfig {
            topic: "delay_test_out".to_string(),
            capacity: Some(10),
        };
        let mem_publisher = MemoryPublisher::new(&mem_cfg).unwrap();
        let publisher = DelayPublisher::new(Box::new(mem_publisher), &config);

        let start = Instant::now();
        publisher
            .send(CanonicalMessage::from("test"))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(elapsed >= Duration::from_millis(50));
    }
}
