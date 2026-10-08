use crate::models::{FaultMode, RandomPanicMiddleware};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Wrapper around a `MessageConsumer` that injects faults for testing.
pub struct RandomPanicConsumer {
    inner: Box<dyn MessageConsumer>,
    config: Arc<RandomPanicMiddleware>,
}

impl RandomPanicConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &RandomPanicMiddleware) -> Self {
        Self {
            inner,
            config: Arc::new(config.clone()),
        }
    }

    /// Check if we should trigger a fault based on the configuration.
    fn should_trigger_fault(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        // If no specific trigger count is set, trigger always
        if let Some(trigger_on) = self.config.trigger_on_message {
            let current_count = self.config.message_count.fetch_add(1, Ordering::SeqCst) + 1;
            current_count == trigger_on
        } else {
            let _ = self.config.message_count.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    /// Execute the fault injection based on the configured mode.
    fn inject_fault(&self) -> Result<Received, ConsumerError> {
        match self.config.mode {
            FaultMode::Panic => {
                panic!(
                    "RandomPanicConsumer: Panic fault triggered! (mode: {})",
                    self.config.mode
                );
            }
            FaultMode::Disconnect => Err(ConsumerError::Connection(anyhow::anyhow!(
                "RandomPanicConsumer: Simulated connection loss"
            ))),
            FaultMode::Timeout => Err(ConsumerError::Connection(anyhow::anyhow!(
                "RandomPanicConsumer: Simulated timeout"
            ))),
            FaultMode::JsonFormatError => {
                // Return a malformed message that might cause JSON parsing errors downstream
                Ok(Received {
                    message: CanonicalMessage::new("{invalid json}".as_bytes().to_vec(), None),
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                })
            }
            FaultMode::Nack => Err(ConsumerError::Connection(anyhow::anyhow!(
                "RandomPanicConsumer: Message nacked"
            ))),
        }
    }
}

#[async_trait]
impl MessageConsumer for RandomPanicConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        if self.should_trigger_fault() {
            self.inject_fault()
        } else {
            self.inner.receive().await
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if self.should_trigger_fault() {
            match self.inject_fault() {
                Ok(received) => {
                    // For JsonFormatError, we get a single message. We need to convert it to a batch.
                    let commit = crate::traits::into_batch_commit_func(received.commit);
                    Ok(ReceivedBatch {
                        messages: vec![received.message],
                        commit,
                    })
                }
                Err(e) => Err(e), // For other faults, it's an error.
            }
        } else {
            self.inner.receive_batch(max_messages).await
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Wrapper around a `MessagePublisher` that injects faults for testing.
pub struct RandomPanicPublisher {
    inner: Box<dyn MessagePublisher>,
    config: Arc<RandomPanicMiddleware>,
}

impl RandomPanicPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &RandomPanicMiddleware) -> Self {
        Self {
            inner,
            config: Arc::new(config.clone()),
        }
    }

    /// Check if we should trigger a fault based on the configuration.
    fn should_trigger_fault(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        // If no specific trigger count is set, trigger always
        if let Some(trigger_on) = self.config.trigger_on_message {
            let current_count = self.config.message_count.fetch_add(1, Ordering::SeqCst) + 1;
            current_count == trigger_on
        } else {
            let _ = self.config.message_count.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    /// Execute the fault injection based on the configured mode.
    fn inject_fault(&self) -> Result<Sent, PublisherError> {
        match self.config.mode {
            FaultMode::Panic => {
                panic!(
                    "RandomPanicPublisher: Panic fault triggered! (mode: {})",
                    self.config.mode
                );
            }
            FaultMode::Disconnect => Err(PublisherError::Connection(anyhow::anyhow!(
                "RandomPanicPublisher: Simulated connection loss"
            ))),
            FaultMode::Timeout => Err(PublisherError::Retryable(anyhow::anyhow!(
                "RandomPanicPublisher: Simulated timeout"
            ))),
            FaultMode::JsonFormatError => Err(PublisherError::NonRetryable(anyhow::anyhow!(
                "RandomPanicPublisher: JSON format error in message"
            ))),
            FaultMode::Nack => Err(PublisherError::Retryable(anyhow::anyhow!(
                "RandomPanicPublisher: Message nacked"
            ))),
        }
    }
}

#[async_trait]
impl MessagePublisher for RandomPanicPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if self.should_trigger_fault() {
            self.inject_fault()
        } else {
            self.inner.send(message).await
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if self.should_trigger_fault() {
            match self.inject_fault() {
                Ok(_) => {
                    // The fault was triggered but didn't result in an error.
                    // This path is unexpected for current fault modes but we handle it defensively.
                    // We'll consider the batch "handled" by the fault injection.
                    Ok(SentBatch::Ack)
                }
                Err(e) => Err(e),
            }
        } else {
            self.inner.send_batch(messages).await
        }
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};

    fn fault(mode: FaultMode) -> RandomPanicMiddleware {
        RandomPanicMiddleware::default()
            .with_enabled(true)
            .with_mode(mode)
    }

    async fn consumer_with(topic: &str, config: &RandomPanicMiddleware) -> RandomPanicConsumer {
        let inner = MemoryConsumer::new_local(topic, 10);
        let channel = inner.channel();
        for body in ["one", "two"] {
            channel
                .send_message(CanonicalMessage::from(body))
                .await
                .unwrap();
        }
        RandomPanicConsumer::new(Box::new(inner), config)
    }

    #[tokio::test]
    async fn a_disabled_fault_passes_messages_through() {
        let config = fault(FaultMode::Disconnect).with_enabled(false);
        let mut consumer = consumer_with("fault_disabled_in", &config).await;
        assert_eq!(
            &consumer.receive().await.unwrap().message.payload[..],
            b"one"
        );

        let publisher = RandomPanicPublisher::new(
            Box::new(MemoryPublisher::new_local("fault_off", 10)),
            &config,
        );
        publisher.send(CanonicalMessage::from("x")).await.unwrap();
        let sent = publisher
            .send_batch(vec![CanonicalMessage::from("y")])
            .await;
        assert!(matches!(sent, Ok(SentBatch::Ack)));
    }

    #[tokio::test]
    async fn the_consumer_fault_fires_on_the_configured_message_only() {
        let config = fault(FaultMode::Disconnect).with_trigger_on_message(2);
        let mut consumer = consumer_with("fault_second_in", &config).await;

        assert_eq!(
            &consumer.receive().await.unwrap().message.payload[..],
            b"one"
        );
        let error = consumer.receive_batch(10).await.err().unwrap();
        assert!(matches!(error, ConsumerError::Connection(_)), "{error}");
        assert!(error.to_string().contains("connection loss"), "{error}");
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(&batch.messages[0].payload[..], b"two");
    }

    #[tokio::test]
    async fn consumer_timeout_and_nack_surface_as_connection_errors() {
        for (mode, text) in [(FaultMode::Timeout, "timeout"), (FaultMode::Nack, "nacked")] {
            let mut consumer = consumer_with("fault_modes_in", &fault(mode)).await;
            let error = consumer.receive().await.err().unwrap();
            assert!(matches!(error, ConsumerError::Connection(_)), "{error}");
            assert!(error.to_string().contains(text), "{error}");
        }
    }

    #[tokio::test]
    async fn a_json_format_fault_hands_out_a_broken_payload_instead_of_the_message() {
        let config = fault(FaultMode::JsonFormatError);
        let mut consumer = consumer_with("fault_json_in", &config).await;

        let single = consumer.receive().await.unwrap();
        assert_eq!(&single.message.payload[..], b"{invalid json}");
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(&batch.messages[0].payload[..], b"{invalid json}");
    }

    #[tokio::test]
    async fn each_publisher_fault_maps_to_its_error_class() {
        for mode in [
            FaultMode::Disconnect,
            FaultMode::Timeout,
            FaultMode::Nack,
            FaultMode::JsonFormatError,
        ] {
            let name = mode.to_string();
            let inner = Box::new(MemoryPublisher::new_local("fault_out", 10));
            let publisher = RandomPanicPublisher::new(inner, &fault(mode));

            let single = publisher.send(CanonicalMessage::from("x")).await;
            let batch = publisher
                .send_batch(vec![CanonicalMessage::from("y")])
                .await;
            for error in [single.err().unwrap(), batch.err().unwrap()] {
                let matches_class = match name.as_str() {
                    "disconnect" => matches!(error, PublisherError::Connection(_)),
                    "json_format_error" => matches!(error, PublisherError::NonRetryable(_)),
                    _ => matches!(error, PublisherError::Retryable(_)),
                };
                assert!(matches_class, "{name}: {error}");
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "Panic fault triggered! (mode: panic)")]
    async fn the_panic_mode_panics() {
        let inner = Box::new(MemoryPublisher::new_local("fault_panic_out", 10));
        let publisher = RandomPanicPublisher::new(inner, &fault(FaultMode::Panic));
        let _ = publisher.send(CanonicalMessage::from("x")).await;
    }
}
