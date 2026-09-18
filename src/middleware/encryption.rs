//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Two-way payload encryption middleware: the publisher side seals each
//! message payload into an AEAD envelope, the consumer side opens it.
//! Metadata and routing keys stay in the clear. Request-reply response
//! payloads pass through untouched.
//!
//! Metadata keys listed in `authenticate_metadata` are not encrypted but are
//! bound into the AEAD tag, so altering one in transit fails decryption.

use crate::models::EncryptionConfig;
use crate::support::crypto::Crypto;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

pub struct EncryptionPublisher {
    inner: Box<dyn MessagePublisher>,
    crypto: Arc<Crypto>,
}

impl EncryptionPublisher {
    pub fn new(
        inner: Box<dyn MessagePublisher>,
        config: &EncryptionConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            crypto: Arc::new(Crypto::new(config)?),
        })
    }

    fn seal_message(&self, message: &mut CanonicalMessage) -> Result<(), PublisherError> {
        let aad = self.crypto.metadata_aad(&message.metadata);
        let sealed = self
            .crypto
            .seal(&message.payload, &aad)
            .map_err(PublisherError::NonRetryable)?;
        message.payload = sealed.into();
        Ok(())
    }
}

#[async_trait]
impl MessagePublisher for EncryptionPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.seal_message(&mut message)?;
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Keep the plaintext payloads: messages surfaced as failed must go back
        // upstream (retry/dlq) in their original form, or an outer retry would
        // double-seal them. A Vec of refcount-bumped `Bytes` costs one allocation
        // for the batch.
        let mut originals: Vec<(u128, bytes::Bytes)> = Vec::with_capacity(messages.len());
        for message in &mut messages {
            originals.push((message.message_id, message.payload.clone()));
            self.seal_message(message)?;
        }
        match self.inner.send_batch(messages).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial {
                responses,
                mut failed,
            } => {
                // Indexed only here: a whole batch can fail, and scanning the
                // originals per message made that quadratic.
                let by_id: HashMap<u128, bytes::Bytes> = originals.into_iter().collect();
                for (msg, _) in &mut failed {
                    if let Some(original) = by_id.get(&msg.message_id) {
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

pub struct EncryptionConsumer {
    inner: Box<dyn MessageConsumer>,
    crypto: Arc<Crypto>,
}

impl EncryptionConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &EncryptionConfig) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            crypto: Arc::new(Crypto::new(config)?),
        })
    }

    fn open_message(&self, message: &mut CanonicalMessage) -> Result<(), ConsumerError> {
        // A decrypt/authentication failure is permanent: the ciphertext will never
        // open, so it must be surfaced as non-retryable rather than triggering an
        // endless reconnect-and-re-read of the same poison message.
        let aad = self.crypto.metadata_aad(&message.metadata);
        let opened = self
            .crypto
            .open(&message.payload, &aad)
            .map_err(ConsumerError::Permanent)?;
        message.payload = opened.into();
        Ok(())
    }
}

#[async_trait]
impl MessageConsumer for EncryptionConsumer {
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
        self.open_message(&mut received.message)?;
        Ok(received)
    }

    /// A message that will not open is poison: returning the whole batch as an error drops
    /// it uncommitted, so it is redelivered and fails again forever, taking every valid
    /// message beside it down each time. Keep the valid ones and ack the rejected slots
    /// instead, mirroring `TransformConsumer::receive_batch`.
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let ReceivedBatch { messages, commit } = self.inner.receive_batch(max_messages).await?;
            let original_len = messages.len();
            let mut kept = Vec::with_capacity(original_len);
            let mut kept_indices: Vec<usize> = Vec::with_capacity(original_len);

            for (index, mut message) in messages.into_iter().enumerate() {
                match self.open_message(&mut message) {
                    Ok(()) => {
                        kept_indices.push(index);
                        kept.push(message);
                    }
                    Err(error) => tracing::error!(
                        message_id = format_args!("{:032x}", message.message_id),
                        "Rejecting message that failed to decrypt: {error}"
                    ),
                }
            }

            if kept.len() == original_len {
                return Ok(ReceivedBatch {
                    messages: kept,
                    commit,
                });
            }

            if kept.is_empty() {
                // Every message was poison. Ack them so they are not redelivered forever,
                // then fetch the next batch rather than surfacing an empty (idle-looking) one.
                if original_len > 0 {
                    commit(vec![MessageDisposition::Ack; original_len])
                        .await
                        .map_err(ConsumerError::Connection)?;
                }
                continue;
            }

            // Rejected slots are acked; the caller's dispositions go back at the indices
            // they came from, keeping at-least-once intact for the surviving messages.
            let remapped = Box::new(move |dispositions: Vec<MessageDisposition>| {
                let mut full = vec![MessageDisposition::Ack; original_len];
                for (slot, disposition) in kept_indices.into_iter().zip(dispositions) {
                    full[slot] = disposition;
                }
                commit(full)
            });

            return Ok(ReceivedBatch {
                messages: kept,
                commit: remapped,
            });
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::sync::Mutex;

    fn config() -> EncryptionConfig {
        EncryptionConfig {
            key: base64::engine::general_purpose::STANDARD.encode([5u8; 32]),
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

    /// Serves one batch then reports end-of-stream, and records the dispositions each
    /// batch was committed with so the poison-message handling can be asserted.
    struct MockConsumer {
        messages: Option<Vec<CanonicalMessage>>,
        committed: Arc<Mutex<Vec<Vec<MessageDisposition>>>>,
    }

    impl MockConsumer {
        fn new(messages: Vec<CanonicalMessage>) -> Self {
            Self {
                messages: Some(messages),
                committed: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl MessageConsumer for MockConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            let Some(messages) = self.messages.take() else {
                return Err(ConsumerError::EndOfStream);
            };
            let committed = self.committed.clone();
            Ok(ReceivedBatch {
                messages,
                commit: Box::new(move |dispositions| {
                    committed.lock().unwrap().push(dispositions);
                    Box::pin(async { Ok(()) })
                }),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn publisher_encrypts_and_consumer_decrypts() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = EncryptionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(),
        )
        .unwrap();

        let mut msg = CanonicalMessage::from("top secret");
        msg.metadata.insert("kind".to_string(), "note".to_string());
        publisher.send_batch(vec![msg]).await.unwrap();

        // The wire payload is ciphertext; metadata stays clear.
        let wire = sent.lock().unwrap().clone();
        assert_ne!(wire[0].payload.as_ref(), b"top secret");
        assert_eq!(
            wire[0].metadata.get("kind").map(|s| s.as_str()),
            Some("note")
        );

        let mut consumer =
            EncryptionConsumer::new(Box::new(MockConsumer::new(wire)), &config()).unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages[0].payload.as_ref(), b"top secret");
        assert_eq!(
            batch.messages[0].metadata.get("kind").map(|s| s.as_str()),
            Some("note")
        );
    }

    #[tokio::test]
    async fn tampered_payload_is_dropped_not_redelivered() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = EncryptionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(),
        )
        .unwrap();
        publisher
            .send_batch(vec![
                CanonicalMessage::from("payload"),
                CanonicalMessage::from("intact"),
            ])
            .await
            .unwrap();

        let mut wire = sent.lock().unwrap().clone();
        let mut tampered = wire[0].payload.to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        wire[0].payload = tampered.into();

        let inner = MockConsumer::new(wire);
        let committed = inner.committed.clone();
        let mut consumer = EncryptionConsumer::new(Box::new(inner), &config()).unwrap();

        // The poison message is dropped rather than failing the whole batch, so the
        // intact message beside it is still delivered.
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), b"intact");

        // Committing the surviving message acks the poison slot too, so it is not
        // re-read indefinitely, and the caller's disposition lands at its own index.
        (batch.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();
        let recorded = committed.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(
            recorded[0].as_slice(),
            [MessageDisposition::Ack, MessageDisposition::Nack]
        ));
    }

    /// Metadata is transport-visible, so a listed key is bound into the tag: changing
    /// it in transit must fail exactly like a tampered payload.
    #[tokio::test]
    async fn tampered_authenticated_metadata_is_rejected() {
        let mut cfg = config();
        cfg.authenticate_metadata = vec!["tenant".to_string()];

        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher =
            EncryptionPublisher::new(Box::new(RecordingPublisher { sent: sent.clone() }), &cfg)
                .unwrap();
        let mut msg = CanonicalMessage::from("payload");
        msg.metadata
            .insert("tenant".to_string(), "acme".to_string());
        msg.metadata.insert("trace".to_string(), "t1".to_string());
        publisher.send_batch(vec![msg]).await.unwrap();

        let wire = sent.lock().unwrap().clone();

        // An unlisted key stays free to change.
        let mut untouched = wire.clone();
        untouched[0]
            .metadata
            .insert("trace".to_string(), "t2".to_string());
        let mut consumer =
            EncryptionConsumer::new(Box::new(MockConsumer::new(untouched)), &cfg).unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages[0].payload.as_ref(), b"payload");

        // The listed one does not: the message is dropped and acked, not redelivered.
        let mut swapped = wire;
        swapped[0]
            .metadata
            .insert("tenant".to_string(), "evil".to_string());
        let inner = MockConsumer::new(swapped);
        let committed = inner.committed.clone();
        let mut consumer = EncryptionConsumer::new(Box::new(inner), &cfg).unwrap();
        assert!(matches!(
            consumer.receive_batch(10).await,
            Err(ConsumerError::EndOfStream)
        ));
        assert!(matches!(
            committed.lock().unwrap()[0].as_slice(),
            [MessageDisposition::Ack]
        ));
    }

    #[tokio::test]
    async fn an_all_poison_batch_is_acked_and_skipped() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = EncryptionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(),
        )
        .unwrap();
        publisher
            .send_batch(vec![CanonicalMessage::from("payload")])
            .await
            .unwrap();

        let mut wire = sent.lock().unwrap().clone();
        let mut tampered = wire[0].payload.to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        wire[0].payload = tampered.into();

        let inner = MockConsumer::new(wire);
        let committed = inner.committed.clone();
        let mut consumer = EncryptionConsumer::new(Box::new(inner), &config()).unwrap();

        // Nothing survives, so the batch is acked and the next one is fetched — here
        // the mock is exhausted, which surfaces as end-of-stream rather than a hang.
        assert!(matches!(
            consumer.receive_batch(10).await,
            Err(ConsumerError::EndOfStream)
        ));
        let recorded = committed.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(recorded[0].as_slice(), [MessageDisposition::Ack]));
    }
}
