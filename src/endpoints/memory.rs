//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue
use crate::models::MemoryConfig;
use crate::traits::{BoxFuture, BulkCommitFunc, CommitFunc, MessageConsumer, MessagePublisher};
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
        tracing::info!("Message sent to memory {} channel", self.sender.len());
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

    /// Returns the number of messages currently in the channel.
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
            .send(vec![message])
            .await
            .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;

        tracing::trace!(
            "Message sent to publisher memory channel {}",
            self.sender.len()
        );
        Ok(None)
    }

    async fn send_bulk(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Option<Vec<CanonicalMessage>>> {
        self.sender
            .send(messages)
            .await
            .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;
        tracing::trace!(
            "Batch sent to publisher memory channel {}. Current batch count: {}",
            self.sender.len(),
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
        // If the buffer is empty, await a new batch from the channel.
        if self.buffer.is_empty() {
            let batch = self
                .receiver
                .recv()
                .await
                .map_err(|_| anyhow!("Memory channel closed."))?;
            self.buffer = batch;
        }

        // Pop a message from the buffer. This will panic if empty, but the logic above prevents that.
        let message = self.buffer.remove(0);
        let commit = Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>);
        Ok((message, commit))
    }

    async fn receive_bulk(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BulkCommitFunc)> {
        // If the internal buffer has messages, return them first.
        if !self.buffer.is_empty() {
            let mut messages_to_return = std::mem::take(&mut self.buffer);
            if messages_to_return.len() > max_messages {
                self.buffer = messages_to_return.split_off(max_messages);
            }
            let commit = Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>);
            return Ok((messages_to_return, commit));
        }

        // Buffer is empty, so wait for a new batch from the channel.
        let messages = self
            .receiver
            .recv()
            .await
            .map_err(|_| anyhow!("Memory channel closed."))?;
        let commit = Box::new(|_| Box::pin(async move {}) as BoxFuture<'static, ()>);
        Ok((messages, commit))
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
