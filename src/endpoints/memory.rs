//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use crate::models::MemoryConfig;
use crate::traits::{BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
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
/// It can be cloned and shared between your test code and the bridge's endpoints.
#[derive(Debug, Clone)]
pub struct MemoryChannel {
    pub sender: Sender<CanonicalMessage>,
    pub receiver: Receiver<CanonicalMessage>,
}

impl MemoryChannel {
    /// Creates a new channel with a specified capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self { sender, receiver }
    }

    /// Helper function for tests to easily send a message to the channel.
    pub async fn send_message(&self, message: CanonicalMessage) -> anyhow::Result<()> {
        self.sender.send(message).await?;
        tracing::info!("Message sent to memory {} channel", self.sender.len());
        Ok(())
    }

    /// Helper function for tests to easily fill in messages.
    pub async fn fill_messages(&self, messages: Vec<CanonicalMessage>) -> anyhow::Result<()> {
        for message in messages {
            // The `send` method is async. It will wait gracefully if the channel is full
            // without blocking the thread, allowing other tasks (like the consumer) to run.
            self.sender.send(message).await.map_err(|e| {
                anyhow!("Memory channel was closed while filling messages: {}", e)
            })?;
        }
        Ok(())
    }

    /// Closes the sender part of the channel.
    pub fn close(&self) {
        self.sender.close();
    }

    /// Helper function for tests to drain all messages from the channel.
    pub fn drain_messages(&self) -> Vec<CanonicalMessage> {
        let mut messages = Vec::new();
        while let Ok(msg) = self.receiver.try_recv() {
            messages.push(msg);
        }
        messages
    }

    /// Returns the number of messages currently in the channel.
    pub fn len(&self) -> usize {
        self.receiver.len()
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
    sender: Sender<CanonicalMessage>,
}

impl MemoryPublisher {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let channel = get_or_create_channel(&config);
        Ok(Self {
            topic: config.topic.clone(),
            sender: channel.sender.clone(),
        })
    }
    pub fn channel(&self) -> MemoryChannel {
        get_or_create_channel(&MemoryConfig {
            topic: self.topic.clone(),
            capacity: None,
        })
    }
}

#[async_trait]
impl MessagePublisher for MemoryPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        self.sender
            .send(message)
            .await
            .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;

        tracing::trace!(
            "Message sent to publisher memory channel {}",
            self.sender.len()
        );
        Ok(None)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A source that reads messages from an in-memory channel.
pub struct MemoryConsumer {
    topic: String,
    receiver: Receiver<CanonicalMessage>,
}

impl MemoryConsumer {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let channel = get_or_create_channel(&config);
        Ok(Self {
            topic: config.topic.clone(),
            receiver: channel.receiver.clone(),
        })
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
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let message = self
            .receiver
            .recv()
            .await
            .map_err(|_| anyhow!("Memory channel closed."))?;

        tracing::trace!("Message receveid. Queued: {}", self.receiver.len());
        let commit = Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>);
        Ok((message, commit))
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
        // Send a message via the publisher
        publisher.send(msg.clone()).await.unwrap();

        sleep(std::time::Duration::from_millis(10)).await;
        // Receive it with the consumer
        let (received_msg, commit) = consumer.receive().await.unwrap();
        commit(None).await;

        assert_eq!(received_msg.payload, msg.payload);
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

        // 3. Send messages via the publisher
        publisher.send(msg1.clone()).await.unwrap();
        publisher.send(msg2.clone()).await.unwrap();

        // 4. Verify the channel has the messages
        assert_eq!(publisher.channel().len(), 2);

        // 5. Receive the messages and verify them
        let (received_msg1, commit1) = consumer.receive().await.unwrap();
        commit1(None).await;
        assert_eq!(received_msg1.payload, msg1.payload);

        let (received_msg2, commit2) = consumer.receive().await.unwrap();
        commit2(None).await;
        assert_eq!(received_msg2.payload, msg2.payload);

        // 6. Verify that the channel is now empty
        assert_eq!(publisher.channel().len(), 0);

        // 7. Verify that reading again results in an error because the channel is empty and we are not closing it
        // In a real scenario with a closed channel, this would error out. Here we can just check it's empty.
        // A `receive` call would just hang, waiting for a message.
    }
}
