//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::transport::TransportChannel;
use crate::CanonicalMessage;
use anyhow::{anyhow, Result};
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;

/// In-process memory transport using async_channel
#[derive(Debug, Clone)]
pub struct MemoryTransport {
    sender: Sender<Vec<CanonicalMessage>>,
    receiver: Receiver<Vec<CanonicalMessage>>,
}

impl MemoryTransport {
    /// Create a new memory transport with the specified capacity
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self { sender, receiver }
    }

    /// Get the sender for this transport
    pub fn sender(&self) -> &Sender<Vec<CanonicalMessage>> {
        &self.sender
    }

    /// Get the receiver for this transport
    pub fn receiver(&self) -> &Receiver<Vec<CanonicalMessage>> {
        &self.receiver
    }
}

#[async_trait]
impl TransportChannel for MemoryTransport {
    async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> Result<()> {
        self.sender
            .send(messages)
            .await
            .map_err(|e| anyhow!("Failed to send batch to memory transport: {}", e))
    }

    async fn recv_batch(&self) -> Result<Vec<CanonicalMessage>> {
        self.receiver
            .recv()
            .await
            .map_err(|_| anyhow!("Memory transport channel closed"))
    }

    fn try_recv_batch(&self) -> Result<Option<Vec<CanonicalMessage>>> {
        match self.receiver.try_recv() {
            Ok(batch) => Ok(Some(batch)),
            Err(async_channel::TryRecvError::Empty) => Ok(None),
            Err(async_channel::TryRecvError::Closed) => {
                Err(anyhow!("Memory transport channel closed"))
            }
        }
    }

    fn len(&self) -> usize {
        self.receiver.len()
    }

    fn capacity(&self) -> Option<usize> {
        self.receiver.capacity()
    }

    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    fn close(&self) {
        self.sender.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;

    #[tokio::test]
    async fn test_memory_transport_send_recv() {
        let transport = MemoryTransport::new(10);

        let msg = CanonicalMessage::from_vec(b"test");
        transport.send_batch(vec![msg.clone()]).await.unwrap();

        let received = transport.recv_batch().await.unwrap();
        assert_eq!(received.len(), 1);
    }

    #[tokio::test]
    async fn test_memory_transport_try_recv() {
        let transport = MemoryTransport::new(10);

        // Empty channel
        let result = transport.try_recv_batch().unwrap();
        assert!(result.is_none());

        // Send and try receive
        let msg = CanonicalMessage::from_vec(b"test");
        transport.send_batch(vec![msg.clone()]).await.unwrap();

        let received = transport.try_recv_batch().unwrap();
        assert!(received.is_some());
        assert_eq!(received.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_memory_transport_close() {
        let transport = MemoryTransport::new(10);

        assert!(!transport.is_closed());
        transport.close();
        assert!(transport.is_closed());

        let msg = CanonicalMessage::from_vec(b"test");
        let result = transport.send_batch(vec![msg]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_memory_transport_capacity() {
        let transport = MemoryTransport::new(5);

        assert_eq!(transport.capacity(), Some(5));
        assert_eq!(transport.len(), 0);
        assert!(transport.is_empty());

        let msg = CanonicalMessage::from_vec(b"test");
        transport.send_batch(vec![msg]).await.unwrap();

        assert_eq!(transport.len(), 1);
        assert!(!transport.is_empty());
    }
}
