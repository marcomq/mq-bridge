use crate::models::{FaultMode, RandomPanicMiddleware};
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received, ReceivedBatch,
    Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Global registry for triggered state, keyed by route name and middleware instance ID (if needed)
static TRIGGERED_STATE: Lazy<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Wrapper around a `MessageConsumer` that injects faults for testing.
pub struct RandomPanicConsumer {
    inner: Box<dyn MessageConsumer>,
    config: Arc<RandomPanicMiddleware>,
    triggered_flag: Arc<AtomicBool>, // Shared flag
}

impl RandomPanicConsumer {
    /// Helper to get or create a shared AtomicBool for a given route.
    fn get_triggered_flag(route_name: &str) -> Arc<AtomicBool> {
        let mut map = TRIGGERED_STATE.lock().unwrap();
        map.entry(route_name.to_string())
           .or_insert_with(|| Arc::new(AtomicBool::new(false)))
           .clone()
    }

    pub fn new(inner: Box<dyn MessageConsumer>, config: &RandomPanicMiddleware, route_name: &str) -> Self {
        Self {
            inner,
            config: Arc::new(config.clone()),
            triggered_flag: Self::get_triggered_flag(route_name),
        }
    }

    /// Check if we should trigger a fault based on the configuration.
    fn should_trigger_fault(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        // Only trigger if it hasn't been triggered yet globally for this route.
        if self.triggered_flag.load(Ordering::SeqCst) {
            return false;
        }

        // If no specific trigger count is set, trigger always
        if let Some(trigger_on) = self.config.trigger_on_message {
            let current_count = self.config.message_count.fetch_add(1, Ordering::SeqCst) + 1;
            current_count == trigger_on
        } else {
            let _ = self.config.message_count.fetch_add(1, Ordering::SeqCst);
            true // Trigger always if no specific trigger_on_message
        }
        .then(|| { self.triggered_flag.store(true, Ordering::SeqCst); true }) // Mark as triggered globally
        .unwrap_or(false)
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
    triggered_flag: Arc<AtomicBool>, // Shared flag
}

impl RandomPanicPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &RandomPanicMiddleware, route_name: &str) -> Self {
        Self {
            inner,
            config: Arc::new(config.clone()),
            triggered_flag: RandomPanicConsumer::get_triggered_flag(route_name), // Use the same global registry
        }
    }

    /// Check if we should trigger a fault based on the configuration.
    fn should_trigger_fault(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        // Only trigger if it hasn't been triggered yet globally for this route.
        if self.triggered_flag.load(Ordering::SeqCst) {
            return false;
        }

        // If no specific trigger count is set, trigger always
        if let Some(trigger_on) = self.config.trigger_on_message {
            let current_count = self.config.message_count.fetch_add(1, Ordering::SeqCst) + 1;
            current_count == trigger_on
        } else {
            let _ = self.config.message_count.fetch_add(1, Ordering::SeqCst);
            true // Trigger always if no specific trigger_on_message
        }
        .then(|| { self.triggered_flag.store(true, Ordering::SeqCst); true }) // Mark as triggered globally
        .unwrap_or(false)
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
            FaultMode::Disconnect => Err(PublisherError::NonRetryable(anyhow::anyhow!(
                "__CONNECTION_ERROR__: RandomPanicPublisher: Simulated connection loss"
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}
