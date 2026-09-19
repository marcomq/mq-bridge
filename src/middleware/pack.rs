//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Transport-level batching. `pack` folds a publish batch into one physical
//! message on the output side; `unpack` splits it back on the input side. The
//! envelope itself lives in [`crate::support::pack`].
//!
//! Compression is not built in: stack the `compression` middleware around this one
//! (`[compression, pack]` on the output, `[unpack, compression]` on the input) and
//! the physical message is compressed as a whole.
//!
//! # Acknowledgement
//!
//! A physical message is the unit the transport acks, so the N logical messages
//! inside one share its fate. `unpack` holds the inner batch's commit until every
//! logical message it produced has been dispositioned, then collapses each group:
//! any `Nack` nacks the whole physical message, so all N are redelivered. That is
//! the same at-least-once widening a `batch_size` larger than one already has —
//! nothing is dropped, some may be seen twice. Pair with `deduplication` (or an
//! idempotent sink) if duplicates matter.

use super::buffer::rebuild_error;
use crate::models::{PackMiddleware, UnpackMiddleware};
use crate::support::pack::{unpack, Packer, UnpackLimits};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use std::any::Any;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Metadata key on the physical message recording how many logical messages it holds.
/// Purely informational — `unpack` reads the count out of the envelope, not from here.
const PACK_COUNT_KEY: &str = "mqb.pack.count";

// --- publisher ---

pub struct PackPublisher {
    inner: Box<dyn MessagePublisher>,
    packer: Packer,
    max_messages: usize,
    max_bytes: usize,
}

impl PackPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &PackMiddleware) -> anyhow::Result<Self> {
        if config.max_messages == 0 {
            return Err(anyhow!("pack max_messages must be greater than zero"));
        }
        if config.max_bytes == 0 {
            return Err(anyhow!("pack max_bytes must be greater than zero"));
        }
        Ok(Self {
            inner,
            packer: Packer::new(config.format, !config.drop_message_id),
            max_messages: config.max_messages,
            max_bytes: config.max_bytes,
        })
    }

    /// Splits `messages` at `max_messages` / `max_bytes`, oldest first. A single
    /// message larger than `max_bytes` gets a chunk of its own rather than being
    /// dropped, and whatever is left over at the end is a chunk too.
    fn chunks(&self, messages: &[CanonicalMessage]) -> Vec<Range<usize>> {
        let mut chunks = Vec::new();
        let mut start = 0;
        let mut bytes = 0usize;
        // Conservative: a chunk that ends up without metadata is only over-counted.
        let with_metadata = self.packer.batch_has_metadata(messages);

        for (index, message) in messages.iter().enumerate() {
            let len = self.packer.record_len(message, with_metadata);
            let full = index - start >= self.max_messages;
            let over = index > start && bytes + len > self.max_bytes;
            if full || over {
                chunks.push(start..index);
                start = index;
                bytes = 0;
            }
            bytes += len;
        }
        if start < messages.len() {
            chunks.push(start..messages.len());
        }
        chunks
    }
}

#[async_trait]
impl MessagePublisher for PackPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        // Packing borrows: no payload or metadata map is cloned, and `messages`
        // stays intact so a failure can be reported with the originals.
        let ranges = self.chunks(&messages);
        let mut physical = Vec::with_capacity(ranges.len());
        for range in &ranges {
            let body = self.packer.pack(&messages[range.clone()]);
            let mut packed = CanonicalMessage::new_bytes(body, None);
            packed
                .metadata
                .insert(PACK_COUNT_KEY.to_string(), range.len().to_string());
            physical.push(packed);
        }
        let packed_ids: Vec<u128> = physical.iter().map(|m| m.message_id).collect();

        match self.inner.send_batch(physical).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial { responses, failed } => {
                if responses.is_some_and(|r| !r.is_empty()) {
                    warn_once(
                        &REPLY_WARNED,
                        "pack: the transport replied to a packed batch, but a reply cannot be split \
                         across the messages inside it. Do not combine `pack` with request/reply.",
                    );
                }
                if failed.is_empty() {
                    return Ok(SentBatch::Ack);
                }

                // Report the originals, not the envelope: an outer retry or dlq has to
                // see the messages the route handed us.
                let mut originals: Vec<Option<CanonicalMessage>> =
                    messages.into_iter().map(Some).collect();
                let mut out = Vec::new();
                for (packed, error) in failed {
                    let Some(index) = packed_ids.iter().position(|id| *id == packed.message_id)
                    else {
                        continue;
                    };
                    let text = error.to_string();
                    for slot in &mut originals[ranges[index].clone()] {
                        if let Some(message) = slot.take() {
                            out.push((message, rebuild_error(&error, &text)));
                        }
                    }
                }
                Ok(SentBatch::Partial {
                    responses: None,
                    failed: out,
                })
            }
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static REPLY_WARNED: AtomicBool = AtomicBool::new(false);
static UNPACK_REPLY_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once(flag: &AtomicBool, message: &'static str) {
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::warn!("{message}");
    }
}

// --- consumer ---

/// One physical message's share of an inner batch's commit.
struct Slot {
    total: usize,
    outstanding: usize,
    nacked: bool,
    reply: Option<CanonicalMessage>,
}

/// An inner batch held open until every message unpacked out of it has been
/// dispositioned. Dropping it without resolving simply never commits, which on
/// every transport means redelivery — the safe direction.
struct HeldBatch {
    commit: Option<BatchCommitFunc>,
    slots: Vec<Slot>,
    unresolved: usize,
}

impl HeldBatch {
    /// Folds one logical message's disposition into its physical message's slot.
    fn record(&mut self, slot: usize, disposition: MessageDisposition) {
        let entry = &mut self.slots[slot];
        if entry.outstanding == 0 {
            return;
        }
        entry.outstanding -= 1;
        match disposition {
            MessageDisposition::Nack => entry.nacked = true,
            MessageDisposition::Reply(reply) => {
                if entry.total == 1 {
                    entry.reply = Some(reply);
                } else {
                    warn_once(
                        &UNPACK_REPLY_WARNED,
                        "unpack: a message inside a packed batch produced a reply, but the batch \
                         has a single acknowledgement. The reply is dropped; do not combine \
                         `pack` with request/reply.",
                    );
                }
            }
            MessageDisposition::Ack => {}
        }
        if entry.outstanding == 0 {
            self.unresolved -= 1;
        }
    }

    /// The inner commit plus one disposition per physical message, once nothing is
    /// outstanding.
    fn take_if_complete(&mut self) -> Option<(BatchCommitFunc, Vec<MessageDisposition>)> {
        if self.unresolved > 0 {
            return None;
        }
        let commit = self.commit.take()?;
        let dispositions = self
            .slots
            .iter_mut()
            .map(|slot| {
                if slot.nacked {
                    MessageDisposition::Nack
                } else if let Some(reply) = slot.reply.take() {
                    MessageDisposition::Reply(reply)
                } else {
                    MessageDisposition::Ack
                }
            })
            .collect();
        Some((commit, dispositions))
    }
}

/// A logical message waiting to be handed to the route, and the slot it answers to.
struct Pending {
    message: CanonicalMessage,
    batch: Arc<Mutex<HeldBatch>>,
    slot: usize,
}

pub struct UnpackConsumer {
    inner: Box<dyn MessageConsumer>,
    config: UnpackMiddleware,
    limits: UnpackLimits,
    pending: VecDeque<Pending>,
}

impl UnpackConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &UnpackMiddleware) -> Self {
        Self {
            inner,
            config: config.clone(),
            limits: UnpackLimits {
                max_messages: config.max_messages,
            },
            pending: VecDeque::new(),
        }
    }

    /// Expands one inner batch onto the pending queue. Returns the number of logical
    /// messages produced, having already committed the batch if that is zero.
    async fn refill(&mut self, batch: ReceivedBatch) -> Result<usize, ConsumerError> {
        let mut slots = Vec::with_capacity(batch.messages.len());
        let mut unpacked = Vec::with_capacity(batch.messages.len());
        for message in &batch.messages {
            // Malformed bytes never become well-formed on a re-read, so this is a
            // permanent failure rather than a reconnectable one.
            let messages = unpack(self.config.format, &message.payload, &self.limits)
                .map_err(ConsumerError::Permanent)?;
            slots.push(Slot {
                total: messages.len(),
                outstanding: messages.len(),
                nacked: false,
                reply: None,
            });
            unpacked.push(messages);
        }

        let unresolved = slots.iter().filter(|slot| slot.outstanding > 0).count();
        let total: usize = slots.iter().map(|slot| slot.total).sum();
        if total == 0 {
            // Every envelope was empty. Acknowledge them so the source moves on, and
            // let the caller read again rather than reporting a drained input.
            (batch.commit)(vec![MessageDisposition::Ack; slots.len()])
                .await
                .map_err(ConsumerError::Permanent)?;
            return Ok(0);
        }

        let held = Arc::new(Mutex::new(HeldBatch {
            commit: Some(batch.commit),
            slots,
            unresolved,
        }));
        for (slot, messages) in unpacked.into_iter().enumerate() {
            for message in messages {
                self.pending.push_back(Pending {
                    message,
                    batch: held.clone(),
                    slot,
                });
            }
        }
        Ok(total)
    }

    /// Takes up to `max_messages` off the pending queue as a batch whose commit
    /// folds back into the inner batches those messages came from.
    fn drain(&mut self, max_messages: usize) -> ReceivedBatch {
        let count = max_messages.min(self.pending.len());
        let mut messages = Vec::with_capacity(count);
        let mut owners = Vec::with_capacity(count);
        for pending in self.pending.drain(..count) {
            messages.push(pending.message);
            owners.push((pending.batch, pending.slot));
        }

        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                let mut ready = Vec::new();
                let mut dispositions = dispositions.into_iter();
                for (batch, slot) in owners {
                    let disposition = dispositions.next().unwrap_or_default();
                    let mut held = batch.lock().expect("unpack commit state poisoned");
                    held.record(slot, disposition);
                    if let Some(complete) = held.take_if_complete() {
                        ready.push(complete);
                    }
                }
                for (commit, dispositions) in ready {
                    commit(dispositions).await?;
                }
                Ok(())
            })
        });

        ReceivedBatch { messages, commit }
    }
}

#[async_trait]
impl MessageConsumer for UnpackConsumer {
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

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch::empty());
        }
        while self.pending.is_empty() {
            let batch = self.inner.receive_batch(max_messages).await?;
            if batch.messages.is_empty() {
                // An idle or drained source: pass the signal straight through.
                return Ok(batch);
            }
            self.refill(batch).await?;
        }
        Ok(self.drain(max_messages))
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PackFormat;
    use std::sync::atomic::AtomicUsize;

    fn pack_config(max_messages: usize, max_bytes: usize) -> PackMiddleware {
        PackMiddleware {
            format: PackFormat::Mqb,
            max_messages,
            max_bytes,
            drop_message_id: false,
        }
    }

    fn rows(count: usize) -> Vec<CanonicalMessage> {
        (0..count)
            .map(|i| {
                let mut message = CanonicalMessage::from(format!("row-{i}").as_str());
                message.metadata.insert("seq".to_string(), i.to_string());
                message
            })
            .collect()
    }

    #[derive(Default, Clone)]
    struct RecordingPublisher {
        sent: Arc<Mutex<Vec<CanonicalMessage>>>,
        fail_nth: Option<usize>,
    }

    #[async_trait]
    impl MessagePublisher for RecordingPublisher {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut failed = Vec::new();
            let mut kept = Vec::new();
            for (index, message) in messages.into_iter().enumerate() {
                if self.fail_nth == Some(index) {
                    failed.push((message, PublisherError::Retryable(anyhow!("nope"))));
                } else {
                    kept.push(message);
                }
            }
            self.sent.lock().unwrap().extend(kept);
            if failed.is_empty() {
                Ok(SentBatch::Ack)
            } else {
                Ok(SentBatch::Partial {
                    responses: None,
                    failed,
                })
            }
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Replays canned batches, recording the dispositions each one is committed with.
    struct ScriptedConsumer {
        batches: VecDeque<Vec<CanonicalMessage>>,
        commits: Arc<Mutex<Vec<Vec<MessageDisposition>>>>,
    }

    #[async_trait]
    impl MessageConsumer for ScriptedConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            let Some(messages) = self.batches.pop_front() else {
                return Ok(ReceivedBatch::empty());
            };
            let commits = self.commits.clone();
            Ok(ReceivedBatch {
                messages,
                commit: Box::new(move |dispositions| {
                    Box::pin(async move {
                        commits.lock().unwrap().push(dispositions);
                        Ok(())
                    })
                }),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn unpacker(
        wire: Vec<CanonicalMessage>,
    ) -> (UnpackConsumer, Arc<Mutex<Vec<Vec<MessageDisposition>>>>) {
        let commits = Arc::new(Mutex::new(Vec::new()));
        let consumer = UnpackConsumer::new(
            Box::new(ScriptedConsumer {
                batches: VecDeque::from(vec![wire]),
                commits: commits.clone(),
            }),
            &UnpackMiddleware::default(),
        );
        (consumer, commits)
    }

    async fn publish(
        config: &PackMiddleware,
        messages: Vec<CanonicalMessage>,
    ) -> Vec<CanonicalMessage> {
        let recording = RecordingPublisher::default();
        let publisher = PackPublisher::new(Box::new(recording.clone()), config).unwrap();
        publisher.send_batch(messages).await.unwrap();
        let sent = recording.sent.lock().unwrap().clone();
        sent
    }

    #[tokio::test]
    async fn a_batch_becomes_one_physical_message_and_comes_back_whole() {
        let input = rows(1000);
        let wire = publish(&pack_config(1000, 1 << 20), input.clone()).await;
        assert_eq!(wire.len(), 1, "1000 rows must cost one transport operation");
        assert_eq!(wire[0].metadata.get(PACK_COUNT_KEY).unwrap(), "1000");

        let (mut consumer, _) = unpacker(wire);
        let batch = consumer.receive_batch(10_000).await.unwrap();
        assert_eq!(batch.messages.len(), 1000);
        for (got, want) in batch.messages.iter().zip(&input) {
            assert_eq!(got.payload, want.payload);
            assert_eq!(got.metadata, want.metadata);
            assert_eq!(got.message_id, want.message_id);
        }
    }

    #[tokio::test]
    async fn an_empty_publish_batch_sends_nothing() {
        assert!(publish(&pack_config(10, 1 << 20), Vec::new())
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn the_message_limit_splits_the_batch_and_keeps_the_remainder() {
        let wire = publish(&pack_config(4, 1 << 20), rows(10)).await;
        assert_eq!(wire.len(), 3);
        let counts: Vec<&str> = wire
            .iter()
            .map(|m| m.metadata[PACK_COUNT_KEY].as_str())
            .collect();
        assert_eq!(
            counts,
            ["4", "4", "2"],
            "the final partial batch is sent too"
        );
    }

    #[tokio::test]
    async fn the_byte_limit_splits_the_batch() {
        let input = rows(10);
        let config = pack_config(1000, 1);
        let wire = publish(&config, input.clone()).await;
        assert_eq!(
            wire.len(),
            10,
            "a message larger than max_bytes still gets a chunk rather than being dropped"
        );

        // Two rows per physical message: one record fits, the second tips it over.
        let packer = Packer::new(PackFormat::Mqb, true);
        let two = packer.record_len(&input[0], true) + packer.record_len(&input[1], true);
        let wire = publish(&pack_config(1000, two), input).await;
        assert_eq!(wire.len(), 5);
    }

    #[tokio::test]
    async fn a_failed_physical_message_reports_its_original_messages() {
        let input = rows(6);
        let publisher = PackPublisher::new(
            Box::new(RecordingPublisher {
                fail_nth: Some(1),
                ..Default::default()
            }),
            &pack_config(2, 1 << 20),
        )
        .unwrap();

        let SentBatch::Partial { failed, .. } = publisher.send_batch(input.clone()).await.unwrap()
        else {
            panic!("expected a partial result");
        };
        assert_eq!(
            failed.len(),
            2,
            "both messages in the failed chunk come back"
        );
        assert_eq!(failed[0].0.payload, input[2].payload);
        assert_eq!(failed[1].0.payload, input[3].payload);
        assert!(matches!(failed[0].1, PublisherError::Retryable(_)));
        assert!(
            !failed[0].0.payload.starts_with(b"MQB1"),
            "failures must be reported unpacked, or an outer retry would re-pack them"
        );
    }

    #[tokio::test]
    async fn unpack_honours_the_requested_batch_size() {
        let wire = publish(&pack_config(1000, 1 << 20), rows(250)).await;
        let (mut consumer, commits) = unpacker(wire);

        let mut seen = 0;
        for _ in 0..3 {
            let batch = consumer.receive_batch(100).await.unwrap();
            assert!(batch.messages.len() <= 100);
            seen += batch.messages.len();
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
        assert_eq!(seen, 250);
        assert_eq!(
            commits.lock().unwrap().len(),
            1,
            "the source is acknowledged once, after the last chunk"
        );
    }

    #[tokio::test]
    async fn the_source_is_not_acknowledged_until_every_chunk_is_committed() {
        let wire = publish(&pack_config(1000, 1 << 20), rows(10)).await;
        let (mut consumer, commits) = unpacker(wire);

        let first = consumer.receive_batch(4).await.unwrap();
        (first.commit)(vec![MessageDisposition::Ack; 4])
            .await
            .unwrap();
        assert!(
            commits.lock().unwrap().is_empty(),
            "6 messages are still outstanding"
        );

        let second = consumer.receive_batch(100).await.unwrap();
        assert_eq!(second.messages.len(), 6);
        (second.commit)(vec![MessageDisposition::Ack; 6])
            .await
            .unwrap();
        assert_eq!(commits.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_nack_nacks_the_whole_physical_message() {
        let wire = publish(&pack_config(5, 1 << 20), rows(10)).await;
        assert_eq!(wire.len(), 2);
        let (mut consumer, commits) = unpacker(wire);

        let batch = consumer.receive_batch(100).await.unwrap();
        assert_eq!(batch.messages.len(), 10);
        let mut dispositions = vec![MessageDisposition::Ack; 10];
        dispositions[7] = MessageDisposition::Nack;
        (batch.commit)(dispositions).await.unwrap();

        let recorded = commits.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(recorded[0][0], MessageDisposition::Ack));
        assert!(
            matches!(recorded[0][1], MessageDisposition::Nack),
            "the physical message holding the nacked row is redelivered whole"
        );
    }

    /// Dropping a batch before committing must leave the source un-acked, so the
    /// messages come back rather than vanishing.
    #[tokio::test]
    async fn an_abandoned_batch_never_acknowledges_the_source() {
        let wire = publish(&pack_config(1000, 1 << 20), rows(10)).await;
        let (mut consumer, commits) = unpacker(wire);
        drop(consumer.receive_batch(100).await.unwrap());
        assert!(commits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_idle_source_still_reports_empty() {
        let (mut consumer, _) = unpacker(Vec::new());
        assert!(consumer
            .receive_batch(100)
            .await
            .unwrap()
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn a_corrupt_envelope_is_a_permanent_failure() {
        let mut wire = publish(&pack_config(10, 1 << 20), rows(3)).await;
        wire[0].payload = bytes::Bytes::from_static(b"not a packed batch at all");
        let (mut consumer, _) = unpacker(wire);
        assert!(matches!(
            consumer.receive_batch(100).await,
            Err(ConsumerError::Permanent(_))
        ));
    }

    /// Compression is a separate middleware, so the pipeline the docs describe —
    /// `[compression, pack]` writing and `[unpack, compression]` reading — has to
    /// round-trip. The wrapping below is exactly what `apply_middlewares_to_*`
    /// builds from those two lists.
    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn pack_round_trips_when_stacked_with_the_compression_middleware() {
        use crate::middleware::compression::{CompressionConsumer, CompressionPublisher};
        use crate::models::{Compression, CompressionMiddleware};

        for algorithm in [Compression::Gzip, Compression::Lz4, Compression::Zstd] {
            let codec = CompressionMiddleware {
                algorithm,
                max_decompressed_bytes: None,
            };
            let input = rows(200);

            // Output `[compression, pack]`: pack outermost, so it frames first and the
            // codec then compresses the one physical message.
            let recording = RecordingPublisher::default();
            let publisher = PackPublisher::new(
                Box::new(CompressionPublisher::new(
                    Box::new(recording.clone()),
                    &codec,
                )),
                &pack_config(1000, 1 << 20),
            )
            .unwrap();
            publisher.send_batch(input.clone()).await.unwrap();

            let wire = recording.sent.lock().unwrap().clone();
            assert_eq!(wire.len(), 1, "{algorithm:?}");
            assert_ne!(&wire[0].payload[..4], b"MQB1", "the envelope is compressed");

            // Input `[unpack, compression]`: the codec is innermost, so it decompresses
            // what the transport produced before unpack sees it.
            let mut consumer = UnpackConsumer::new(
                Box::new(CompressionConsumer::new(
                    Box::new(ScriptedConsumer {
                        batches: VecDeque::from(vec![wire]),
                        commits: Arc::new(Mutex::new(Vec::new())),
                    }),
                    &codec,
                )),
                &UnpackMiddleware::default(),
            );
            let batch = consumer.receive_batch(1000).await.unwrap();
            assert_eq!(batch.messages.len(), 200, "{algorithm:?}");
            for (got, want) in batch.messages.iter().zip(&input) {
                assert_eq!(got.payload, want.payload, "{algorithm:?}");
                assert_eq!(got.metadata, want.metadata, "{algorithm:?}");
                assert_eq!(got.message_id, want.message_id, "{algorithm:?}");
            }
        }
    }

    /// The full at-rest stack from the reference: frame, then compress, then encrypt.
    /// Order is the whole point — compressing *after* encryption would gain nothing,
    /// and `pack` is what makes compressing worthwhile, since it hands the codec a
    /// whole batch instead of one payload.
    #[cfg(all(feature = "compression", feature = "encryption"))]
    #[tokio::test]
    async fn pack_compression_and_encryption_stack_in_the_documented_order() {
        use crate::middleware::compression::{CompressionConsumer, CompressionPublisher};
        use crate::middleware::encryption::{EncryptionConsumer, EncryptionPublisher};
        use crate::models::{Compression, CompressionMiddleware, EncryptionConfig};

        let codec = CompressionMiddleware {
            algorithm: Compression::Zstd,
            max_decompressed_bytes: None,
        };
        let cipher = EncryptionConfig {
            cipher: Default::default(),
            key_id: "default".to_string(),
            key: "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE=".to_string(),
            decrypt_keys: Default::default(),
            authenticate_metadata: Vec::new(),
        };
        let input = rows(200);

        // Output `[encryption, compression, pack]`: pack outermost frames first, the
        // codec compresses the batch, encryption seals what is left.
        let recording = RecordingPublisher::default();
        let publisher = PackPublisher::new(
            Box::new(CompressionPublisher::new(
                Box::new(EncryptionPublisher::new(Box::new(recording.clone()), &cipher).unwrap()),
                &codec,
            )),
            &pack_config(1000, 1 << 20),
        )
        .unwrap();
        publisher.send_batch(input.clone()).await.unwrap();

        let wire = recording.sent.lock().unwrap().clone();
        assert_eq!(wire.len(), 1, "200 rows leave as one physical message");
        assert_ne!(
            &wire[0].payload[..4],
            b"MQB1",
            "the envelope is not on the wire"
        );

        // Input `[unpack, compression, encryption]` — the mirror.
        let mut consumer = UnpackConsumer::new(
            Box::new(CompressionConsumer::new(
                Box::new(
                    EncryptionConsumer::new(
                        Box::new(ScriptedConsumer {
                            batches: VecDeque::from(vec![wire]),
                            commits: Arc::new(Mutex::new(Vec::new())),
                        }),
                        &cipher,
                    )
                    .unwrap(),
                ),
                &codec,
            )),
            &UnpackMiddleware::default(),
        );
        let batch = consumer.receive_batch(1000).await.unwrap();
        assert_eq!(batch.messages.len(), 200);
        for (got, want) in batch.messages.iter().zip(&input) {
            assert_eq!(got.payload, want.payload);
            assert_eq!(got.metadata, want.metadata);
        }
    }

    #[tokio::test]
    async fn concurrent_chunk_commits_resolve_the_source_once() {
        let wire = publish(&pack_config(1000, 1 << 20), rows(100)).await;
        let (mut consumer, commits) = unpacker(wire);

        let mut futures = Vec::new();
        for _ in 0..10 {
            let batch = consumer.receive_batch(10).await.unwrap();
            let len = batch.messages.len();
            futures.push((batch.commit)(vec![MessageDisposition::Ack; len]));
        }
        // Committed out of order, as a concurrent route would.
        futures.reverse();
        for future in futures {
            future.await.unwrap();
        }
        assert_eq!(commits.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_empty_envelope_is_acknowledged_and_read_past() {
        let empty = publish(&pack_config(10, 1 << 20), Vec::new()).await;
        assert!(empty.is_empty());

        // Hand the consumer a batch whose only envelope holds nothing.
        let packer = Packer::new(PackFormat::Mqb, true);
        let wire = vec![CanonicalMessage::new_bytes(packer.pack(&[]), None)];
        let (mut consumer, commits) = unpacker(wire);
        assert!(consumer
            .receive_batch(100)
            .await
            .unwrap()
            .messages
            .is_empty());
        assert_eq!(
            commits.lock().unwrap().len(),
            1,
            "the empty envelope is acknowledged rather than re-read forever"
        );
    }

    #[tokio::test]
    async fn benthos_binary_drops_metadata_but_keeps_payloads_and_order() {
        let input = rows(5);
        let config = PackMiddleware {
            format: PackFormat::BenthosBinary,
            ..pack_config(1000, 1 << 20)
        };
        let wire = publish(&config, input.clone()).await;
        assert_eq!(wire.len(), 1);

        let commits = Arc::new(Mutex::new(Vec::new()));
        let mut consumer = UnpackConsumer::new(
            Box::new(ScriptedConsumer {
                batches: VecDeque::from(vec![wire]),
                commits,
            }),
            &UnpackMiddleware {
                format: PackFormat::BenthosBinary,
                ..Default::default()
            },
        );
        let batch = consumer.receive_batch(100).await.unwrap();
        assert_eq!(batch.messages.len(), 5);
        for (got, want) in batch.messages.iter().zip(&input) {
            assert_eq!(got.payload, want.payload);
            assert!(got.metadata.is_empty());
        }
    }

    #[tokio::test]
    async fn max_messages_rejects_an_oversized_envelope() {
        let wire = publish(&pack_config(1000, 1 << 20), rows(50)).await;
        let mut consumer = UnpackConsumer::new(
            Box::new(ScriptedConsumer {
                batches: VecDeque::from(vec![wire]),
                commits: Arc::new(Mutex::new(Vec::new())),
            }),
            &UnpackMiddleware {
                max_messages: Some(10),
                ..Default::default()
            },
        );
        assert!(matches!(
            consumer.receive_batch(100).await,
            Err(ConsumerError::Permanent(_))
        ));
    }

    /// `send` goes through `send_batch`, so a single message is still framed and a
    /// reader with `unpack` configured can read it.
    #[tokio::test]
    async fn a_single_send_is_framed_too() {
        let recording = RecordingPublisher::default();
        let publisher =
            PackPublisher::new(Box::new(recording.clone()), &pack_config(10, 1 << 20)).unwrap();
        publisher
            .send(CanonicalMessage::from("lonely"))
            .await
            .unwrap();
        let wire = recording.sent.lock().unwrap().clone();
        assert_eq!(wire.len(), 1);
        let (mut consumer, _) = unpacker(wire);
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages[0].payload.as_ref(), b"lonely");
    }

    #[tokio::test]
    async fn zero_limits_are_rejected_at_construction() {
        for config in [pack_config(0, 1 << 20), pack_config(10, 0)] {
            assert!(
                PackPublisher::new(Box::<RecordingPublisher>::default(), &config).is_err(),
                "{config:?}"
            );
        }
    }

    /// The chunker must not lose or duplicate a message at any limit.
    #[tokio::test]
    async fn every_message_lands_in_exactly_one_chunk() {
        let counted = AtomicUsize::new(0);
        for count in [1usize, 2, 7, 64, 129] {
            for max_messages in [1usize, 3, 64, 1000] {
                let input = rows(count);
                let wire = publish(&pack_config(max_messages, 1 << 20), input.clone()).await;
                let (mut consumer, _) = unpacker(wire);
                let batch = consumer.receive_batch(10_000).await.unwrap();
                assert_eq!(batch.messages.len(), count, "{count}/{max_messages}");
                for (got, want) in batch.messages.iter().zip(&input) {
                    assert_eq!(got.payload, want.payload);
                }
                counted.fetch_add(1, Ordering::Relaxed);
            }
        }
        assert_eq!(counted.load(Ordering::Relaxed), 20);
    }
}
