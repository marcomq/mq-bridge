//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Two-way payload compression middleware: the publisher side compresses each
//! message payload into a single self-contained member, the consumer side
//! decompresses it. Metadata and routing keys stay untouched. `Compression::None`
//! is a passthrough.

use crate::models::{Compression, CompressionMiddleware, InputErrorPolicy};
use crate::support::compression::{compress_member, decompress_all};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
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
        let originals: Vec<bytes::Bytes> = messages.iter().map(|m| m.payload.clone()).collect();
        if self.algo != Compression::None {
            let algo = self.algo;
            messages = crate::support::parallel::map_messages(messages, move |mut message| {
                compress_member(algo, &message.payload).map(|out| {
                    message.payload = out.into();
                    message
                })
            })
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(|e| PublisherError::NonRetryable(e.into()))?;
        }
        // `map_messages` keeps the batch order, so the two line up by position.
        let originals = messages
            .iter()
            .zip(originals)
            .map(|(m, original)| (m.message_id, original, m.payload.clone()))
            .collect();
        match self.inner.send_batch(messages).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial {
                responses,
                mut failed,
            } => {
                super::restore_payloads(originals, &mut failed);
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
    on_error: InputErrorPolicy,
}

/// Keeps the messages that decoded. The rejected slots are acked with the batch, and the
/// caller's dispositions go back at the slots they came from.
fn keep_decoded(decoded: Vec<Option<CanonicalMessage>>, commit: BatchCommitFunc) -> ReceivedBatch {
    let original_len = decoded.len();
    let mut kept = Vec::with_capacity(original_len);
    let mut kept_indices = Vec::with_capacity(original_len);
    for (index, message) in decoded.into_iter().enumerate() {
        if let Some(message) = message {
            kept_indices.push(index);
            kept.push(message);
        }
    }
    let remapped = Box::new(move |dispositions: Vec<MessageDisposition>| {
        let mut full = vec![MessageDisposition::Ack; original_len];
        for (slot, disposition) in kept_indices.into_iter().zip(dispositions) {
            full[slot] = disposition;
        }
        commit(full)
    });
    ReceivedBatch {
        messages: kept,
        commit: remapped,
    }
}

fn note_dropped(message_id: u128, error: &std::io::Error) {
    super::note_rejected_input_message();
    tracing::error!(
        message_id = format_args!("{message_id:032x}"),
        "Dropping message that failed to decompress: {error}"
    );
}

impl CompressionConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &CompressionMiddleware) -> Self {
        if config.algorithm != Compression::None && config.max_decompressed_bytes.is_none() {
            tracing::info!(
                "compression: no max_decompressed_bytes set, a payload may decompress to any size"
            );
        }
        Self {
            inner,
            algo: config.algorithm,
            max_bytes: config.max_decompressed_bytes,
            on_error: config.on_error,
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
        loop {
            let mut received = self.inner.receive().await?;
            match self.decompress_message(&mut received.message) {
                Ok(()) => return Ok(received),
                Err(ConsumerError::Permanent(error)) if self.on_error == InputErrorPolicy::Drop => {
                    super::note_rejected_input_message();
                    tracing::error!(
                        message_id = format_args!("{:032x}", received.message.message_id),
                        "Dropping message that failed to decompress: {error}"
                    );
                    (received.commit)(MessageDisposition::Ack)
                        .await
                        .map_err(ConsumerError::Connection)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let mut batch = self.inner.receive_batch(max_messages).await?;
            if self.algo == Compression::None || batch.messages.is_empty() {
                return Ok(batch);
            }
            let (algo, max_bytes) = (self.algo, self.max_bytes);
            let messages = std::mem::take(&mut batch.messages);
            let decoded = crate::support::parallel::map_messages(messages, move |mut message| {
                match decompress_all(algo, &message.payload, max_bytes) {
                    Ok(out) => {
                        message.payload = out.into();
                        Ok(message)
                    }
                    Err(error) => Err((message.message_id, error)),
                }
            })
            .await;
            if self.on_error == InputErrorPolicy::Fail {
                batch.messages = decoded
                    .into_iter()
                    .collect::<Result<_, _>>()
                    .map_err(|(_, e)| ConsumerError::Permanent(e.into()))?;
                return Ok(batch);
            }
            let decoded = decoded
                .into_iter()
                .map(|result| result.map_err(|(id, e)| note_dropped(id, &e)).ok())
                .collect();
            let batch = keep_decoded(decoded, batch.commit);
            if !batch.messages.is_empty() {
                return Ok(batch);
            }
            // Every message was dropped: ack them and read on, as an empty batch means idle.
            (batch.commit)(Vec::new())
                .await
                .map_err(ConsumerError::Connection)?;
        }
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
            ..Default::default()
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

    #[tokio::test]
    async fn on_error_drop_skips_an_undecodable_payload() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = CompressionPublisher::new(
            Box::new(RecordingPublisher { sent: sent.clone() }),
            &config(Compression::Zstd),
        );
        publisher
            .send_batch(vec![CanonicalMessage::from("intact")])
            .await
            .unwrap();
        let mut wire = vec![CanonicalMessage::from("not compressed")];
        wire.extend(sent.lock().unwrap().clone());

        let cfg = CompressionMiddleware {
            algorithm: Compression::Zstd,
            on_error: InputErrorPolicy::Drop,
            ..Default::default()
        };
        let mut consumer = CompressionConsumer::new(
            Box::new(MockConsumer {
                messages: Some(wire),
            }),
            &cfg,
        );
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), b"intact");
    }

    #[tokio::test]
    async fn dropped_slots_are_acked_beside_the_callers_dispositions() {
        let committed = Arc::new(Mutex::new(Vec::new()));
        let recorder = committed.clone();
        let batch = keep_decoded(
            vec![None, Some(CanonicalMessage::from("kept")), None],
            Box::new(move |dispositions| {
                recorder.lock().unwrap().push(dispositions);
                Box::pin(async { Ok(()) })
            }),
        );
        assert_eq!(batch.messages.len(), 1);
        (batch.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();
        assert!(matches!(
            committed.lock().unwrap()[0].as_slice(),
            [
                MessageDisposition::Ack,
                MessageDisposition::Nack,
                MessageDisposition::Ack
            ]
        ));
    }

    #[test]
    fn failed_messages_sharing_an_id_get_their_own_payload_back() {
        use bytes::Bytes;
        let originals = vec![
            (7, Bytes::from_static(b"first"), Bytes::from_static(b"c1")),
            (7, Bytes::from_static(b"second"), Bytes::from_static(b"c2")),
            (8, Bytes::from_static(b"third"), Bytes::from_static(b"c3")),
        ];
        let fail = |wire: &[u8], id| {
            (
                CanonicalMessage::new(wire.to_vec(), Some(id)),
                PublisherError::Retryable(anyhow::anyhow!("down")),
            )
        };
        let mut failed = vec![fail(b"c2", 7), fail(b"c3", 8)];
        crate::middleware::restore_payloads(originals, &mut failed);
        assert_eq!(&failed[0].0.payload[..], b"second");
        assert_eq!(&failed[1].0.payload[..], b"third");
    }
}
