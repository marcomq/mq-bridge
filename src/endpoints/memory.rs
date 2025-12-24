//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::models::MemoryConfig;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::info;

/// A map to hold memory channels for the duration of the bridge setup.
/// This allows a consumer and publisher in different routes to connect to the same in-memory topic.
static RUNTIME_MEMORY_CHANNELS: Lazy<Mutex<HashMap<String, MemoryChannel>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// A shareable, thread-safe, in-memory channel for testing.
///
/// This struct holds the sender and receiver for an in-memory queue.
/// It can be cloned and shared between your test code and the bridge's endpoints. It transports batches of messages.
#[derive(Debug, Clone)]
pub struct MemoryChannel {
    pub sender: Sender<Vec<CanonicalMessage>>,
    pub receiver: Receiver<Vec<CanonicalMessage>>,
}

impl MemoryChannel {
    /// Creates a new batch channel with a specified capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self { sender, receiver }
    }

    /// Helper function for tests to easily send a message to the channel.
    pub async fn send_message(&self, message: CanonicalMessage) -> anyhow::Result<()> {
        self.sender.send(vec![message]).await?;
        tracing::debug!("Message sent to memory {} channel", self.sender.len());
        Ok(())
    }

    /// Helper function for tests to easily fill in messages.
    pub async fn fill_messages(&self, messages: Vec<CanonicalMessage>) -> anyhow::Result<()> {
        // Send the entire vector as a single batch.
        self.sender
            .send(messages)
            .await
            .map_err(|e| anyhow!("Memory channel was closed while filling messages: {}", e))?;
        Ok(())
    }

    /// Closes the sender part of the channel.
    pub fn close(&self) {
        self.sender.close();
    }

    /// Helper function for tests to drain all messages from the channel.
    pub fn drain_messages(&self) -> Vec<CanonicalMessage> {
        let mut messages = Vec::new();
        // Drain all batches from the channel and flatten them into a single Vec.
        while let Ok(batch) = self.receiver.try_recv() {
            messages.extend(batch);
        }
        messages
    }

    /// Returns the number of bulk messages in the channel.
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// Returns the number of messages currently in the channel.
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

/// Gets a shared `MemoryChannel` for a given topic, creating it if it doesn't exist.
pub fn get_or_create_channel(config: &MemoryConfig) -> MemoryChannel {
    let mut channels = RUNTIME_MEMORY_CHANNELS.lock().unwrap();
    channels
        .entry(config.topic.clone()) // Use the HashMap's entry API
        .or_insert_with(|| {
            info!(topic = %config.topic, "Creating new runtime memory channel");
            MemoryChannel::new(config.capacity.unwrap_or(100))
        })
        .clone()
}

/// A sink that sends messages to an in-memory channel.
#[derive(Clone)]
pub struct MemoryPublisher {
    topic: String,
    sender: Sender<Vec<CanonicalMessage>>,
}

impl MemoryPublisher {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let channel = get_or_create_channel(config);
        Ok(Self {
            topic: config.topic.clone(),
            sender: channel.sender.clone(),
        })
    }

    pub fn new_local(topic: &str, capacity: usize) -> Self {
        Self::new(&MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(capacity),
        })
        .expect("Failed to create local memory publisher")
    }

    /// Note: This helper is primarily for tests expecting a Queue.    
    /// If used on a broadcast publisher, it will create a separate Queue channel.
    pub fn channel(&self) -> MemoryChannel {
        get_or_create_channel(&MemoryConfig {
            topic: self.topic.clone(),
            capacity: None,
        })
    }
}

#[async_trait]
impl MessagePublisher for MemoryPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        self.sender
            .send(messages)
            .await
            .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;

        tracing::trace!(
            "Batch sent to publisher memory channel. Current batch count: {}",
            self.sender.len()
        );
        // Memory channel sends are atomic; if it succeeds, all messages were sent.
        // Return no responses and an empty vec of failed messages.
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A source that reads messages from an in-memory channel.
pub struct MemoryConsumer {
    topic: String,
    receiver: Receiver<Vec<CanonicalMessage>>,
    // Internal buffer to hold messages from a received batch.
    buffer: Vec<CanonicalMessage>,
}

impl MemoryConsumer {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let channel = get_or_create_channel(config);
        Ok(Self {
            topic: config.topic.clone(),
            receiver: channel.receiver.clone(),
            buffer: Vec::new(),
        })
    }

    pub fn new_local(topic: &str, capacity: usize) -> Self {
        Self::new(&MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(capacity),
        })
        .expect("Failed to create local memory consumer")
    }

    pub fn channel(&self) -> MemoryChannel {
        get_or_create_channel(&MemoryConfig {
            topic: self.topic.clone(),
            capacity: None,
        })
    }
}

#[async_trait]
impl MessageConsumer for MemoryConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // If the internal buffer has messages, return them first.
        if self.buffer.is_empty() {
            // Buffer is empty. Wait for a new batch from the channel.
            self.buffer = self
                .receiver
                .recv()
                .await
                .map_err(|_| anyhow!("Memory channel closed."))?;
            // Reverse the buffer so we can efficiently pop from the end.
            self.buffer.reverse();
        }

        // Determine the number of messages to take from the buffer.
        let num_to_take = self.buffer.len().min(max_messages);
        let split_at = self.buffer.len() - num_to_take;

        // `split_off` is highly efficient. It splits the Vec in two at the given
        // index and returns the part after the index, leaving the first part.
        let mut messages = self.buffer.split_off(split_at);
        messages.reverse(); // Reverse back to original order.

        if messages.is_empty() {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>),
            });
        }

        let commit = Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>);
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MemorySubscriber {
    consumer: MemoryConsumer,
}

impl MemorySubscriber {
    pub fn new(config: &MemoryConfig, id: &str) -> anyhow::Result<Self> {
        let mut sub_config = config.clone();
        sub_config.topic = format!("{}-{}", config.topic, id);
        let consumer = MemoryConsumer::new(&sub_config)?;
        Ok(Self { consumer })
    }
}

#[async_trait]
impl MessageConsumer for MemorySubscriber {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.consumer.receive_batch(max_messages).await
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.consumer.receive().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use serde_json::json;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_memory_channel_integration() {
        let cfg = MemoryConfig {
            topic: String::from("test-mem1"),
            capacity: Some(10),
        };

        let mut consumer = MemoryConsumer::new(&cfg).unwrap();
        let publisher = MemoryPublisher::new(&cfg).unwrap();

        let msg = CanonicalMessage::from_json(json!({"hello": "memory"})).unwrap();

        // Send a message via the publisher
        publisher.send(msg.clone()).await.unwrap();

        sleep(std::time::Duration::from_millis(10)).await;
        // Receive it with the consumer
        let received = consumer.receive().await.unwrap();
        (received.commit)(None).await;
        assert_eq!(received.message.payload, msg.payload);
        assert_eq!(consumer.channel().len(), 0);
    }

    #[tokio::test]
    async fn test_memory_publisher_and_consumer_integration() {
        let cfg = MemoryConfig {
            topic: String::from("test-mem2"),
            capacity: Some(10),
        };
        let mut consumer = MemoryConsumer::new(&cfg).unwrap();
        let publisher = MemoryPublisher::new(&cfg).unwrap();

        let msg1 = CanonicalMessage::from_json(json!({"message": "one"})).unwrap();
        let msg2 = CanonicalMessage::from_json(json!({"message": "two"})).unwrap();
        let msg3 = CanonicalMessage::from_json(json!({"message": "three"})).unwrap();

        // 3. Send messages via the publisher
        publisher
            .send_batch(vec![msg1.clone(), msg2.clone()])
            .await
            .unwrap();
        publisher.send(msg3.clone()).await.unwrap();

        // 4. Verify the channel has the messages
        assert_eq!(publisher.channel().len(), 2);

        // 5. Receive the messages and verify them
        let received1 = consumer.receive().await.unwrap();
        (received1.commit)(None).await;
        assert_eq!(received1.message.payload, msg1.payload);

        let batch2 = consumer.receive_batch(1).await.unwrap();
        let (received_msg2, commit2) = (batch2.messages, batch2.commit);
        commit2(None).await;
        assert_eq!(received_msg2.len(), 1);
        assert_eq!(received_msg2.get(0).unwrap().payload, msg2.payload);
        let batch3 = consumer.receive_batch(2).await.unwrap();
        let (received_msg3, commit3) = (batch3.messages, batch3.commit);
        commit3(None).await;
        assert_eq!(received_msg3.get(0).unwrap().payload, msg3.payload);

        // 6. Verify that the channel is now empty
        assert_eq!(publisher.channel().len(), 0);

        // 7. Verify that reading again results in an error because the channel is empty and we are not closing it
        // In a real scenario with a closed channel, this would error out. Here we can just check it's empty.
        // A `receive` call would just hang, waiting for a message.
    }

    #[tokio::test]
    async fn test_memory_subscriber_structure() {
        let cfg = MemoryConfig {
            topic: "base_topic".to_string(),
            capacity: Some(10),
        };
        let subscriber_id = "sub1";
        let mut subscriber = MemorySubscriber::new(&cfg, subscriber_id).unwrap();

        // The subscriber should be listening on "base_topic-sub1"
        // We can verify this by creating a publisher for that specific topic.
        let pub_cfg = MemoryConfig {
            topic: format!("base_topic-{}", subscriber_id),
            capacity: Some(10),
        };
        let publisher = MemoryPublisher::new(&pub_cfg).unwrap();

        let msg = CanonicalMessage::from_str("hello subscriber");
        publisher.send(msg).await.unwrap();

        let received = subscriber.receive().await.unwrap();
        assert_eq!(received.message.get_payload_str(), "hello subscriber");
    }
}
