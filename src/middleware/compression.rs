//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Two-way payload compression middleware: the publisher side compresses each
//! message payload into a single self-contained member, the consumer side
//! decompresses it. Metadata and routing keys stay untouched. `Compression::None`
//! is a passthrough.

use crate::models::{Compression, CompressionMiddleware};
use crate::support::compression::{compress_member, decompress_all};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;

pub struct CompressionPublisher {
    inner: Box<dyn MessagePublisher>,
    algo: Compression,
}

impl CompressionPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &CompressionMiddleware) -> Self {
        Self {
            inner,
            algo: config.algorithm,
        }
    }

    fn compress_message(&self, message: &mut CanonicalMessage) -> Result<(), PublisherError> {
        if self.algo == Compression::None {
            return Ok(());
        }
        let out = compress_member(self.algo, &message.payload)
            .map_err(|e| PublisherError::NonRetryable(e.into()))?;
        message.payload = out.into();
        Ok(())
    }
}

#[async_trait]
impl MessagePublisher for CompressionPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.compress_message(&mut message)?;
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Keep the uncompressed payloads: messages surfaced as failed must go back
        // upstream (retry/dlq) in their original form, or an outer retry would
        // double-compress them.
        let originals: std::collections::HashMap<u128, bytes::Bytes> = messages
            .iter()
            .map(|m| (m.message_id, m.payload.clone()))
            .collect();
        for message in &mut messages {
            self.compress_message(message)?;
        }
        match self.inner.send_batch(messages).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial {
                responses,
                mut failed,
            } => {
                for (msg, _) in &mut failed {
                    if let Some(original) = originals.get(&msg.message_id) {
                        msg.payload = original.clone();
                    }
                }
                Ok(SentBatch::Partial { responses, failed })
            }
        }
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct CompressionConsumer {
    inner: Box<dyn MessageConsumer>,
    algo: Compression,
    max_bytes: Option<u64>,
}

impl CompressionConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &CompressionMiddleware) -> Self {
        Self {
            inner,
            algo: config.algorithm,
            max_bytes: config.max_decompressed_bytes,
        }
    }

    fn decompress_message(&self, message: &mut CanonicalMessage) -> Result<(), ConsumerError> {
        if self.algo == Compression::None {
            return Ok(());
        }
        // A malformed/truncated frame will never decode, so it is a permanent failure
        // rather than a reconnectable one — otherwise the poison message would be
        // re-read forever.
        let out = decompress_all(self.algo, &message.payload, self.max_bytes)
            .map_err(|e| ConsumerError::Permanent(e.into()))?;
        message.payload = out.into();
        Ok(())
    }
}

#[async_trait]
impl MessageConsumer for CompressionConsumer {
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
        let mut received = self.inner.receive().await?;
        self.decompress_message(&mut received.message)?;
        Ok(received)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        for message in &mut batch.messages {
            self.decompress_message(message)?;
        }
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn config(algo: Compression) -> CompressionMiddleware {
        CompressionMiddleware {
            algorithm: algo,
            ..Default::default()
        }
    }

    #[derive(Clone)]
    struct RecordingPublisher {
        sent: Arc<Mutex<Vec<CanonicalMessage>>>,
    }

    #[async_trait]
    impl MessagePublisher for RecordingPublisher {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.sent.lock().unwrap().extend(messages);
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MockConsumer {
        messages: Option<Vec<CanonicalMessage>>,
    }

    #[async_trait]
    impl MessageConsumer for MockConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            Ok(ReceivedBatch {
                messages: self.messages.take().expect("batch already consumed"),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn publisher_compresses_and_consumer_decompresses() {
        for algo in [Compression::Gzip, Compression::Lz4, Compression::Zstd] {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let publisher = CompressionPublisher::new(
                Box::new(RecordingPublisher { sent: sent.clone() }),
                &config(algo),
            );

            let plaintext = "compress me ".repeat(64);
            let mut msg = CanonicalMessage::from(plaintext.as_str());
            msg.metadata.insert("kind".to_string(), "note".to_string());
            publisher.send_batch(vec![msg]).await.unwrap();

            // The wire payload is compressed (smaller) and metadata stays clear.
            let wire = sent.lock().unwrap().clone();
            assert_ne!(
                wire[0].payload.as_ref(),
                plaintext.as_bytes(),
                "algo {algo:?}"
            );
            assert!(wire[0].payload.len() < plaintext.len(), "algo {algo:?}");
            assert_eq!(
                wire[0].metadata.get("kind").map(|s| s.as_str()),
                Some("note")
            );

            let mut consumer = CompressionConsumer::new(
                Box::new(MockConsumer {
                    messages: Some(wire),
                }),
                &config(algo),
            );
            let batch = consumer.receive_batch(10).await.unwrap();
            assert_eq!(
                batch.messages[0].payload.as_ref(),
                plaintext.as_bytes(),
                "algo {algo:?}"
            );
        }
    }

    #[tokio::test]
    async fn corrupt_frame_fails_consume() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = CompressionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(Compression::Gzip),
        );
        publisher
            .send_batch(vec![CanonicalMessage::from("payload payload payload")])
            .await
            .unwrap();

        let mut wire = sent.lock().unwrap().clone();
        let mut corrupt = wire[0].payload.to_vec();
        *corrupt.last_mut().unwrap() ^= 0xff;
        wire[0].payload = corrupt.into();

        let mut consumer = CompressionConsumer::new(
            Box::new(MockConsumer {
                messages: Some(wire),
            }),
            &config(Compression::Gzip),
        );
        assert!(matches!(
            consumer.receive_batch(10).await,
            Err(ConsumerError::Permanent(_))
        ));
    }

    #[tokio::test]
    async fn decompression_bomb_guard_rejects_oversized() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = CompressionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(Compression::Zstd),
        );
        let big = "x".repeat(64 * 1024);
        publisher
            .send_batch(vec![CanonicalMessage::from(big.as_str())])
            .await
            .unwrap();
        let wire = sent.lock().unwrap().clone();

        let cfg = CompressionMiddleware {
            algorithm: Compression::Zstd,
            max_decompressed_bytes: Some(1024),
        };
        let mut consumer = CompressionConsumer::new(
            Box::new(MockConsumer {
                messages: Some(wire),
            }),
            &cfg,
        );
        assert!(matches!(
            consumer.receive_batch(10).await,
            Err(ConsumerError::Permanent(_))
        ));
    }
}
