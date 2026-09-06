//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Cloud object-store endpoint (S3 / GCS / Azure Blob / R2 / ...), built on the
//! `object_store` crate (the same backend as the checkpoint store).
//!
//! - **Sink** ([`ObjectStorePublisher`]): each flushed batch is encoded (reusing the
//!   file endpoint's [`FileFormat`] codecs) and written as one immutable object at
//!   `<prefix>/[YYYY/MM/DD/]<uuidv7>.<ext>`. Objects are write-once; nothing is appended
//!   or mutated. The date prefix is a readability / lifecycle-rule convenience only.
//!
//!   **Object order holds only at `concurrency: 1`.** Lexicographic key order is the only
//!   ordering a bucket offers. The keys are strictly increasing per publisher, so a
//!   sequential route writes them in source order — but the key is minted inside
//!   `send_batch`, which the worker pool runs concurrently, so above `concurrency: 1` mint
//!   order is worker arrival order. Replaying a CDC stream through such a bucket can then
//!   reorder UPDATE/DELETE events on the same key and yield a silently wrong final state.
//!   Set `idempotency: true` to get source-sequenced names
//!   (`part-<topic>-<partition>-<start>-<end>.<ext>`, zero-padded) for sources that stamp a
//!   position; those sort by source position regardless of write order, at any concurrency.
//! - **Source** ([`ObjectStoreConsumer`]): objects under `prefix` are listed in key order,
//!   fetched whole, split by `delimiter`, and emitted. Progress is a durable cursor holding
//!   the last fully-acked object key (via the external checkpoint store), so a restart
//!   resumes without re-emitting. Objects are never deleted or rewritten — resume is
//!   non-destructive, at-least-once at object granularity.

use crate::checkpoint::{self, CheckpointBackend, CheckpointStore};
use crate::endpoints::file::{encode_record, parse_delimiter, parse_message};
use crate::models::{Compression, FileFormat, NameBy, ObjectStoreConfig};
#[cfg(feature = "encryption")]
use crate::support::crypto::Crypto;
use crate::support::source_ranges::{
    finalized_name, parse_finalized_name, CoveredRanges, SourcePartition,
};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use fast_uuid_v7::SequentialGenerator;
use futures::StreamExt;
use object_store::{
    path::Path as ObjPath, Error as ObjectStoreError, ObjectStore, ObjectStoreExt, PutMode,
    PutOptions, PutPayload,
};
use std::any::Any;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, trace, warn};

/// Builds an `object_store` backend and its base prefix `Path` from a URL, reading
/// credentials from the environment. Shared with the object-store checkpoint backend so
/// both resolve creds identically — see [`checkpoint::object_store_backend::build_store`].
use crate::checkpoint::object_store_backend::build_store;

/// True if two object-store URLs share a scheme+host and one's path segments are a prefix
/// of the other's — i.e. a recursive `list` of one would surface objects of the other.
/// Used to reject a checkpoint location that overlaps the source prefix.
fn object_urls_overlap(a: &str, b: &str) -> bool {
    let (pa, pb) = match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return false,
    };
    if pa.scheme() != pb.scheme() || pa.host_str() != pb.host_str() {
        return false;
    }
    let seg = |u: &url::Url| -> Vec<String> {
        u.path_segments()
            .map(|s| s.filter(|p| !p.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let (sa, sb) = (seg(&pa), seg(&pb));
    // Segment-wise prefix match (covers equality); "data" and "database" do NOT overlap.
    sa.iter().zip(sb.iter()).all(|(x, y)| x == y)
}

/// Default object extension derived from the record format, compression and
/// encryption (e.g. `jsonl`, `jsonl.gz`, `jsonl.lz4`, `jsonl.gz.enc`). An
/// encrypted object is ciphertext, so it gets a trailing `.enc` rather than a
/// bare `.gz`/`.lz4` that tools would wrongly treat as directly decompressable.
fn extension_for(format: &FileFormat, compression: Compression, encrypted: bool) -> String {
    let base = match format {
        FileFormat::Normal | FileFormat::Json | FileFormat::Text => "jsonl",
        FileFormat::Csv => "csv",
        FileFormat::Raw => "bin",
    };
    let mut ext = match compression {
        Compression::None => base.to_string(),
        Compression::Gzip => format!("{base}.gz"),
        Compression::Lz4 => format!("{base}.lz4"),
        Compression::Zstd => format!("{base}.zst"),
    };
    if encrypted {
        ext.push_str(".enc");
    }
    ext
}

/// Rejects `compression`/`encryption` settings whose Cargo feature is missing.
fn validate_object_settings(_config: &ObjectStoreConfig) -> anyhow::Result<()> {
    #[cfg(not(feature = "compression"))]
    if _config.compression != Compression::None {
        return Err(anyhow!(
            "object_store 'compression' requires the `compression` feature"
        ));
    }
    #[cfg(not(feature = "encryption"))]
    if _config.encryption.is_some() {
        return Err(anyhow!(
            "object_store 'encryption' requires the `encryption` feature"
        ));
    }
    Ok(())
}

/// Rebuilds the covered source ranges from the object names already present under `base`.
///
/// Unlike the file sink this never deletes anything: a single PUT under the final name *is*
/// the commit here (object stores have no atomic rename), so this path never creates staging
/// objects and has no leftovers of its own to reap. Anything that is not a parseable part
/// name — including a `.stage-` object — belongs to someone else and is left untouched.
async fn recover_finalized_object_ranges(
    store: &dyn ObjectStore,
    base: &ObjPath,
    extension: &str,
) -> anyhow::Result<CoveredRanges> {
    // Parsed as the listing streams: a bucket holding a long history of parts would otherwise
    // materialise every key at once, and only the merged ranges are worth keeping.
    let mut covered = CoveredRanges::default();
    let mut stream = store.list(Some(base));
    while let Some(meta) = stream.next().await {
        let location = meta?.location;
        // The listing is recursive, but this sink writes its parts flat under `base`. A
        // parseable name below a nested prefix is another sink's part, and counting it here
        // would mark ranges covered that were never written to this prefix.
        let Some(mut parts) = location.prefix_match(base) else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        if let Some((source, start, end)) = parse_finalized_name(name.as_ref(), extension) {
            // `parse_finalized_name` already guarantees a valid range.
            covered.insert(source, start, end)?;
        }
    }
    Ok(covered)
}

/// Classifies a failed PUT. A denied credential or a store that does not implement the
/// requested mode does not become writable by trying again, so those stop the route with the
/// reason instead of retrying forever. `PutMode::Create` on a store with conditional put
/// disabled lands here as `NotImplemented`, which is how an idempotent sink reports that the
/// bucket cannot give it the write-once guarantee it is built on.
fn is_permanent_store_error(error: &ObjectStoreError) -> bool {
    matches!(
        error,
        ObjectStoreError::NotImplemented { .. }
            | ObjectStoreError::NotSupported { .. }
            | ObjectStoreError::PermissionDenied { .. }
            | ObjectStoreError::Unauthenticated { .. }
            | ObjectStoreError::InvalidPath { .. }
            | ObjectStoreError::UnknownConfigurationKey { .. }
    )
}

fn classify_put_error(error: ObjectStoreError, context: String) -> PublisherError {
    let permanent = is_permanent_store_error(&error);
    let error = anyhow!(error).context(context);
    if permanent {
        PublisherError::NonRetryable(error)
    } else {
        PublisherError::Retryable(error)
    }
}

/// Splits a fetched object into record slices on `delimiter`, dropping a trailing empty
/// remainder and a stray `\r` before a `\n` delimiter (mirrors the file reader).
fn split_records<'a>(data: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    let mut records = Vec::new();
    if delimiter.is_empty() {
        return records;
    }
    let newline = delimiter.len() == 1 && delimiter[0] == b'\n';
    let mut start = 0;
    let mut i = 0;
    while i + delimiter.len() <= data.len() {
        if &data[i..i + delimiter.len()] == delimiter {
            let mut end = i;
            if newline && end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            records.push(&data[start..end]);
            i += delimiter.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start < data.len() {
        // Trailing record with no closing delimiter.
        records.push(&data[start..]);
    }
    records
}

/// Splits and decodes an object's bytes into messages, threading CSV header state across
/// the object's lines (so the first CSV row establishes the schema).
fn split_and_parse(data: &[u8], delimiter: &[u8], format: &FileFormat) -> Vec<CanonicalMessage> {
    let mut out = Vec::new();
    let mut csv_header: Option<crate::endpoints::file::CsvHeader> = None;
    for record in split_records(data, delimiter) {
        if let Some(msg) = parse_message(record, format, &mut csv_header) {
            out.push(msg);
        }
    }
    out
}

fn empty_batch() -> ReceivedBatch {
    ReceivedBatch {
        messages: Vec::new(),
        commit: Box::new(|_| Box::pin(async { Ok(()) })),
    }
}

// --- Publisher (sink) ---

/// Writes each batch as one immutable object under the configured prefix.
#[derive(Clone)]
pub struct ObjectStorePublisher {
    store: Arc<dyn ObjectStore>,
    base: ObjPath,
    delimiter: Vec<u8>,
    format: FileFormat,
    #[cfg(feature = "compression")]
    compression: Compression,
    #[cfg(feature = "encryption")]
    crypto: Option<Arc<Crypto>>,
    date_partition: bool,
    extension: String,
    name_by: NameBy,
    /// Ranges already on the store, filled by one listing before the first idempotent write.
    /// A fresh prefix pays a single empty listing; `recovered` keeps it to one per publisher.
    covered_ranges: Arc<Mutex<CoveredRanges>>,
    recovered: Arc<tokio::sync::OnceCell<()>>,
    // Shared across clones on purpose: the ordering guarantee is per generator instance, so
    // two independent generators would hand out interleaved keys.
    keys: Arc<StdMutex<SequentialGenerator>>,
}

impl ObjectStorePublisher {
    /// Opens the sink with the naming scheme the config alone implies. Without a route there
    /// is no input to resolve `auto` against, so it falls back to `write_time`.
    pub async fn new(config: &ObjectStoreConfig) -> anyhow::Result<Self> {
        Self::new_with_name_by(config, config.resolved_name_by(false)).await
    }

    pub async fn new_with_name_by(
        config: &ObjectStoreConfig,
        name_by: NameBy,
    ) -> anyhow::Result<Self> {
        if matches!(config.format, FileFormat::Csv) {
            // Each object is independent, so CSV would need its own header row per object.
            // Not implemented for the sink; sources can still read CSV objects.
            return Err(anyhow!(
                "object_store sink does not support the 'csv' format (per-object CSV headers are unimplemented); use jsonl/json/text/raw"
            ));
        }
        validate_object_settings(config)?;
        let (store, base) = build_store(&config.url)?;
        let store: Arc<dyn ObjectStore> = Arc::from(store);
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let extension = config.extension.clone().unwrap_or_else(|| {
            extension_for(
                &config.format,
                config.compression,
                config.encryption.is_some(),
            )
        });
        if name_by == NameBy::SourcePosition && config.date_partition == Some(true) {
            warn!("object_store 'date_partition' does not apply to 'name_by: source_position'; part names carry the source range and are written flat under the prefix");
        }
        info!(url = %config.url, format = ?config.format, ?name_by, "Object-store sink opened");
        Ok(Self {
            store,
            base,
            delimiter,
            format: config.format.clone(),
            #[cfg(feature = "compression")]
            compression: config.compression,
            #[cfg(feature = "encryption")]
            crypto: config
                .encryption
                .as_ref()
                .map(Crypto::new_at_rest)
                .transpose()?
                .map(Arc::new),
            date_partition: config.date_partition_enabled(name_by),
            extension,
            name_by,
            covered_ranges: Arc::new(Mutex::new(CoveredRanges::default())),
            recovered: Arc::new(tokio::sync::OnceCell::new()),
            keys: Arc::new(StdMutex::new(SequentialGenerator::new())),
        })
    }

    /// Object key for the next write: `<prefix>/[YYYY/MM/DD/]<uuidv7>.<ext>`.
    ///
    /// The ids are strictly increasing per publisher, so key order is allocation order even
    /// for objects written within the same millisecond. The optional date prefix is derived
    /// from that same id's embedded millisecond timestamp (no wall-clock dependency), so
    /// the folder and the name can never disagree. A backwards clock jump therefore keeps
    /// writing under the pre-jump date until the clock catches up, rather than breaking the
    /// key ordering.
    fn next_key(&self) -> ObjPath {
        let id = self
            .keys
            .lock()
            .expect("Object-store key generator mutex poisoned")
            .next_id();
        let name = format!("{}.{}", fast_uuid_v7::format_uuid(id), self.extension);
        if self.date_partition {
            // Top 48 bits of a uuidv7 are the Unix-epoch millisecond timestamp.
            let (y, m, d) = civil_from_unix_ms((id >> 80) as u64);
            self.base
                .clone()
                .join(format!("{y:04}").as_str())
                .join(format!("{m:02}").as_str())
                .join(format!("{d:02}").as_str())
                .join(name.as_str())
        } else {
            self.base.clone().join(name.as_str())
        }
    }

    /// Compress-then-encrypt a fully built object body. The whole object is one member, so a
    /// compressed object stays a standard `.gz`/`.lz4` file and a sealed one is a single
    /// envelope — no framing, because objects are written and read whole. Both write paths go
    /// through here, so an idempotent part object is shaped exactly like an ordinary one and
    /// [`ObjectStoreConsumer::decode_object`] reads either without knowing which wrote it.
    #[allow(unused_mut)]
    fn encode_object_body(&self, mut body: Vec<u8>) -> Result<Vec<u8>, PublisherError> {
        #[cfg(feature = "compression")]
        if self.compression != Compression::None {
            body = crate::support::compression::compress_member(self.compression, &body)
                .map_err(|error| PublisherError::NonRetryable(anyhow!(error)))?;
        }
        #[cfg(feature = "encryption")]
        if let Some(crypto) = &self.crypto {
            body = crypto
                .seal(&body, b"")
                .map_err(PublisherError::NonRetryable)?;
        }
        Ok(body)
    }

    /// Lists the prefix once per publisher and folds what is already finalized into
    /// `covered_ranges`. This is what makes filtering per record rather than per batch:
    /// `PutMode::Create` only catches a repeat that lands on the same name, so without the
    /// listing a replay on different batch boundaries would write every record again under
    /// names that never collide.
    ///
    /// Runs before the first PUT. A fresh prefix costs one empty listing per publisher; a
    /// populated one pays the scan it needs to be correct.
    ///
    /// Infallible by design: a store that allows PUT but denies LIST would otherwise stall
    /// the route outright. A failure leaves the cell uninitialised, so the next batch retries
    /// it, and until it succeeds the sink degrades to same-name-only deduplication.
    async fn ensure_recovered(&self) {
        let outcome = self
            .recovered
            .get_or_try_init(|| async {
                let recovered = recover_finalized_object_ranges(
                    self.store.as_ref(),
                    &self.base,
                    &self.extension,
                )
                .await?;
                self.covered_ranges.lock().await.merge(recovered)
            })
            .await;
        if let Err(error) = outcome {
            let permanent = error
                .downcast_ref::<ObjectStoreError>()
                .is_some_and(is_permanent_store_error);
            warn!(
                %error,
                permanent,
                "object_store could not list the prefix; this run's writes are deduplicated only \
                 when a replay reproduces the same batch boundaries, so a replay that batches \
                 differently will write overlapping objects"
            );
            if permanent {
                // A denied or unimplemented listing does not become allowed by asking again.
                // Latching the cell trades the guarantee for one warning and one round trip
                // instead of one of each per batch, forever.
                let _ = self.recovered.set(());
            }
        }
    }

    async fn send_batch_by_source_position(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // The lock guards the shared range map, not the work done with it. Held across the
        // encode and the PUT it would serialise the whole worker pool onto one in-flight
        // object — a sink that publishes sequentially, which is what naming by source range
        // exists to make unnecessary. A snapshot is a handful of merged ranges per partition,
        // so cloning it costs far less than what it lets run concurrently.
        //
        // The route enqueues each message once, so batches in flight together hold disjoint
        // ranges and cannot race here at all. A re-sent batch repeats a range exactly, and an
        // identical range is an identical name, which `PutMode::Create` turns into a no-op.
        // Only a source that redelivers offsets still in flight — a Kafka rebalance mid-batch —
        // could produce two runs that overlap *partially*; those name two different objects and
        // the overlap would be written twice. Claiming the range under the lock before the PUT
        // would close that window, at the cost of unwinding the claim on every failure path.
        //
        // Recovery has to happen before the first PUT, not after one collides: a replay whose
        // batches fall on different boundaries names different objects, so nothing collides and
        // there would be nothing to trigger it.
        self.ensure_recovered().await;
        let covered = self.covered_ranges.lock().await.clone();
        let runs = covered
            .uncovered_runs(messages)
            .map_err(PublisherError::NonRetryable)?;

        let mut failed = Vec::new();
        for run in runs {
            // A record that cannot be encoded splits the run rather than failing the batch:
            // the offsets around it are still written, under names covering exactly what went
            // in, and the bad record goes to the DLQ like it would on the write-time path.
            // Encode failures are a property of the record, so a replay splits identically.
            // No format this sink accepts can actually fail today, which is why the split has
            // no test of its own; it guards the fallible signature, not a reachable case.
            let mut segment_start = run.start;
            let mut segment_end = None;
            let mut body = Vec::new();
            for (index, mut message) in run.messages.into_iter().enumerate() {
                let offset = run.start.saturating_add(index as u64);
                message.strip_source_metadata();
                match encode_record(&message, &self.format) {
                    Ok(bytes) => {
                        body.extend_from_slice(&bytes);
                        body.extend_from_slice(&self.delimiter);
                        segment_end = Some(offset);
                    }
                    Err(error) => {
                        if let Some(end) = segment_end.take() {
                            self.put_source_range(
                                &run.source,
                                segment_start,
                                end,
                                std::mem::take(&mut body),
                            )
                            .await?;
                        }
                        // `take` above emptied the body, and it can only be non-empty when
                        // `segment_end` was set, so there is nothing left to discard here.
                        segment_start = offset.saturating_add(1);
                        failed.push((message, PublisherError::NonRetryable(anyhow!(error))));
                    }
                }
            }
            if let Some(end) = segment_end {
                self.put_source_range(&run.source, segment_start, end, body)
                    .await?;
            }
        }
        if failed.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed,
            })
        }
    }

    /// Writes one contiguous source range as a single object and records it as covered.
    async fn put_source_range(
        &self,
        source: &SourcePartition,
        start: u64,
        end: u64,
        body: Vec<u8>,
    ) -> Result<(), PublisherError> {
        let name = finalized_name(source, start, end, &self.extension)
            .map_err(PublisherError::NonRetryable)?;
        let body = self.encode_object_body(body)?;
        let key = self.base.clone().join(name.as_str());
        match self
            .store
            .put_opts(
                &key,
                PutPayload::from(body),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {}
            // Same name, same range, same bytes: the object is already the one this PUT would
            // have written. Recovery ran before the first PUT, so reaching this arm means a
            // repeat within the run rather than an unrecovered earlier one.
            Err(ObjectStoreError::AlreadyExists { .. }) => {}
            Err(error) => {
                return Err(classify_put_error(
                    error,
                    format!("object-store put by source position '{key}'"),
                ));
            }
        }
        self.covered_ranges
            .lock()
            .await
            .insert(source.clone(), start, end)
            .map_err(PublisherError::NonRetryable)
    }
}

/// Converts Unix-epoch milliseconds (UTC) to a `(year, month, day)` civil date using
/// Howard Hinnant's `civil_from_days` algorithm — avoids a date-crate dependency.
fn civil_from_unix_ms(ms: u64) -> (i64, u32, u32) {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[async_trait]
impl MessagePublisher for ObjectStorePublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        if self.name_by == NameBy::SourcePosition {
            return self.send_batch_by_source_position(messages).await;
        }
        let mut body = Vec::new();
        let mut failed = Vec::new();
        for mut msg in messages {
            msg.strip_source_metadata();
            match encode_record(&msg, &self.format) {
                Ok(bytes) => {
                    body.extend_from_slice(&bytes);
                    body.extend_from_slice(&self.delimiter);
                }
                Err(e) => {
                    failed.push((msg, PublisherError::NonRetryable(anyhow!(e))));
                }
            }
        }
        if body.is_empty() {
            // Every message failed to encode; nothing to write.
            return Ok(SentBatch::Partial {
                responses: None,
                failed,
            });
        }
        let body = self.encode_object_body(body)?;
        let key = self.next_key();
        self.store
            .put(&key, PutPayload::from(body))
            .await
            .map_err(|e| classify_put_error(e, format!("object-store put '{key}'")))?;
        trace!(key = %key, "Wrote object to object store");
        if failed.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed,
            })
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }

    // Deliberately *not* declaring `requires_ordered_publish()`. Read order here is key order
    // and `next_key` now hands out strictly increasing keys, but it allocates them inside
    // `send_batch`, so at concurrency > 1 that order is worker arrival, not source order.
    // Ordered cloud export needs the key allocated while the batches are still sequenced.

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// --- Consumer (source) ---

/// Tracks the object currently being drained and how many of its records are still
/// un-acked, so the durable cursor advances only once an object is fully consumed.
struct ObjProgress {
    key: String,
    remaining: usize,
}

/// Reads objects under a prefix in key order, resuming from a durable cursor.
pub struct ObjectStoreConsumer {
    store: Arc<dyn ObjectStore>,
    base: ObjPath,
    delimiter: Vec<u8>,
    format: FileFormat,
    #[cfg(feature = "compression")]
    compression: Compression,
    #[cfg(feature = "encryption")]
    crypto: Option<Arc<Crypto>>,
    checkpoint: Option<Arc<dyn CheckpointStore>>,
    /// Last fully-acked object key (the resume cursor).
    last_key: Arc<Mutex<Option<String>>>,
    /// Undelivered records of the current object.
    buffer: Arc<Mutex<Vec<CanonicalMessage>>>,
    /// Set while an object is in flight (buffered or awaiting commits); cleared when fully acked.
    progress: Arc<Mutex<Option<ObjProgress>>>,
    idle_delay: Duration,
    /// Reject objects larger than this many bytes rather than buffering them whole; `None` = no limit.
    max_object_bytes: Option<u64>,
    /// Consecutive decode failures on `decode_failing_key` before it is quarantined.
    decode_failures: u32,
    decode_failing_key: Option<String>,
}

/// Consecutive decode failures on one object before it is quarantined (skipped) so a
/// single poison object cannot block the source forever.
const MAX_OBJECT_DECODE_FAILURES: u32 = 5;

impl ObjectStoreConsumer {
    pub async fn new(config: &ObjectStoreConfig) -> anyhow::Result<Self> {
        Self::new_with_no_resume(config, false).await
    }

    pub(crate) async fn new_with_no_resume(
        config: &ObjectStoreConfig,
        no_resume: bool,
    ) -> anyhow::Result<Self> {
        validate_object_settings(config)?;
        let (store, base) = build_store(&config.url)?;
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;

        // Durable resume needs an external checkpoint store: an object store has no cheap
        // per-key cursor row, so the source-datastore backend is rejected here.
        let checkpoint: Option<Arc<dyn CheckpointStore>> = if no_resume {
            None
        } else {
            match (&config.cursor_id, &config.checkpoint_store) {
                (Some(cid), Some(spec)) => match checkpoint::parse_checkpoint_store(spec)? {
                    CheckpointBackend::Source { .. } => {
                        // Same misconfiguration class as the overlap check below: permanent.
                        return Err(anyhow::Error::new(ConsumerError::Permanent(anyhow!(
                            "object_store source requires an external checkpoint_store (file://, s3://, postgres://, or mongodb://); a source-datastore checkpoint is not available."
                        ))));
                    }
                    external => {
                        // Guard against the cursor object landing under the source prefix, where
                        // it would be listed and re-emitted as data.
                        if let CheckpointBackend::ObjectStore { url: ck_url } = &external {
                            if object_urls_overlap(&config.url, ck_url) {
                                // Misconfiguration: rebuilding the consumer reads the same config,
                                // so this must stop the route rather than reconnect forever.
                                return Err(anyhow::Error::new(ConsumerError::Permanent(anyhow!(
                                "object_store checkpoint_store '{ck_url}' overlaps the source prefix '{}'; the cursor object would be listed and re-read as data. Point checkpoint_store at a different bucket or prefix.",
                                config.url
                            ))));
                            }
                        }
                        Some(checkpoint::build_external_store(external, &config.url, cid).await?)
                    }
                },
                (Some(_), None) => {
                    warn!(
                        url = %config.url,
                        "object_store source has cursor_id but no checkpoint_store; resume is disabled and every restart re-emits all objects. Set an external checkpoint_store (file://, s3://, postgres://, mongodb://)."
                    );
                    None
                }
                (None, _) => {
                    warn!(
                        url = %config.url,
                        "object_store source has no cursor_id; resume is disabled and every restart re-emits all objects."
                    );
                    None
                }
            }
        };

        let last_key = match &checkpoint {
            Some(cp) => cp.load().await?,
            None => None,
        };

        info!(
            url = %config.url,
            has_checkpoint = %last_key.is_some(),
            "Object-store source connected"
        );

        Ok(Self {
            store: Arc::from(store),
            base,
            delimiter,
            format: config.format.clone(),
            #[cfg(feature = "compression")]
            compression: config.compression,
            #[cfg(feature = "encryption")]
            crypto: config
                .encryption
                .as_ref()
                .map(Crypto::new_at_rest)
                .transpose()?
                .map(Arc::new),
            checkpoint,
            last_key: Arc::new(Mutex::new(last_key)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            progress: Arc::new(Mutex::new(None)),
            idle_delay: Duration::from_millis(config.polling_interval_ms.unwrap_or(1000)),
            max_object_bytes: config.max_object_bytes,
            decode_failures: 0,
            decode_failing_key: None,
        })
    }

    #[cfg(test)]
    fn from_store(
        store: Arc<dyn ObjectStore>,
        base: ObjPath,
        format: FileFormat,
        checkpoint: Option<Arc<dyn CheckpointStore>>,
        last_key: Option<String>,
    ) -> Self {
        Self {
            store,
            base,
            delimiter: vec![b'\n'],
            format,
            #[cfg(feature = "compression")]
            compression: Compression::None,
            #[cfg(feature = "encryption")]
            crypto: None,
            checkpoint,
            last_key: Arc::new(Mutex::new(last_key)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            progress: Arc::new(Mutex::new(None)),
            idle_delay: Duration::from_millis(10),
            max_object_bytes: None,
            decode_failures: 0,
            decode_failing_key: None,
        }
    }

    /// Fetches the next object strictly after `last` (in key order), skipping directory
    /// markers. Relies on the store listing keys in lexicographic order (S3/GCS/Azure/local
    /// /in-memory all do); `list_with_offset` also filters server-side when resuming.
    async fn next_object(&self, last: Option<&str>) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        let mut stream = match last {
            Some(k) => self
                .store
                .list_with_offset(Some(&self.base), &ObjPath::from(k)),
            None => self.store.list(Some(&self.base)),
        };
        while let Some(meta) = stream.next().await {
            let meta = meta?;
            let key = meta.location.to_string();
            // Skip pseudo-directory markers; `list_with_offset` may also surface the offset key.
            if key.ends_with('/') || last == Some(key.as_str()) {
                continue;
            }
            // Refuse to materialize an over-large object; the listing already carries its size.
            // The object will not shrink, so retrying re-lists it forever: fail permanently.
            if let Some(limit) = self.max_object_bytes {
                if meta.size > limit {
                    return Err(anyhow::Error::new(ConsumerError::Permanent(anyhow!(
                        "object '{key}' is {} bytes, exceeding max_object_bytes ({limit}); refusing to buffer it whole. Raise max_object_bytes or remove the object.",
                        meta.size
                    ))));
                }
            }
            let data = self
                .store
                .get(&meta.location)
                .await?
                .bytes()
                .await?
                .to_vec();
            // Decode (decrypt/decompress) is deferred to `receive_batch` so a poison
            // object is bounded-retried then quarantined rather than blocking forever.
            return Ok(Some((key, data)));
        }
        Ok(None)
    }

    /// Bound on decompressed output.
    ///
    /// `max_object_bytes` caps the *stored* size (see `next_object`), so reusing it here
    /// would reject any object that compressed better than 1:1 — i.e. most of them. Scale
    /// it instead; the result is still bounded, so a decompression bomb cannot run away.
    #[cfg(feature = "compression")]
    fn decompressed_limit(&self) -> Option<u64> {
        const MAX_DECOMPRESSED_EXPANSION: u64 = 20;
        self.max_object_bytes
            .map(|limit| limit.saturating_mul(MAX_DECOMPRESSED_EXPANSION))
    }

    /// Decrypt-then-decompress a fetched object whole (the write path compressed first).
    #[allow(unused_variables)]
    fn decode_object(&self, key: &str, data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        #[cfg(feature = "encryption")]
        let data = if let Some(crypto) = &self.crypto {
            crypto
                .open(&data, b"")
                .with_context(|| format!("decrypt object '{key}'"))?
        } else {
            data
        };
        #[cfg(feature = "compression")]
        let data = if self.compression != Compression::None {
            crate::support::compression::decompress_all(
                self.compression,
                &data,
                self.decompressed_limit(),
            )
            .with_context(|| format!("decompress object '{key}'"))?
        } else {
            data
        };
        Ok(data)
    }

    /// Persists the resume cursor durably, then advances the in-memory cursor. The durable
    /// save happens first and its error is propagated: `last_key` only moves once progress is
    /// safely checkpointed, so a failed save re-lists the object rather than silently skipping it.
    async fn save_cursor(&self, key: &str) -> anyhow::Result<()> {
        if let Some(cp) = &self.checkpoint {
            cp.save(key)
                .await
                .with_context(|| format!("persist object-store cursor '{key}'"))?;
        }
        *self.last_key.lock().await = Some(key.to_string());
        Ok(())
    }
}

#[async_trait]
impl MessageConsumer for ObjectStoreConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(empty_batch());
        }

        // Refill only when the current object is fully accounted for (buffer empty AND no
        // in-flight commits). This keeps objects from interleaving so the cursor advances
        // strictly in key order.
        {
            let buffer_empty = self.buffer.lock().await.is_empty();
            let in_flight = self.progress.lock().await.is_some();
            if buffer_empty && !in_flight {
                let last = self.last_key.lock().await.clone();
                // `from` (not `Connection`) so a permanent listing failure — e.g. an object
                // over `max_object_bytes` — is not retried as a transport blip.
                match self
                    .next_object(last.as_deref())
                    .await
                    .map_err(ConsumerError::from)?
                {
                    None => {
                        tokio::time::sleep(self.idle_delay).await;
                        return Ok(empty_batch());
                    }
                    Some((key, raw)) => {
                        // Decode here so a poison object is bounded-retried then
                        // quarantined (cursor advanced past it) rather than looping forever.
                        let data = match self.decode_object(&key, raw) {
                            Ok(data) => {
                                self.decode_failing_key = None;
                                self.decode_failures = 0;
                                data
                            }
                            Err(e) => {
                                if self.decode_failing_key.as_deref() == Some(key.as_str()) {
                                    self.decode_failures += 1;
                                } else {
                                    self.decode_failing_key = Some(key.clone());
                                    self.decode_failures = 1;
                                }
                                if self.decode_failures >= MAX_OBJECT_DECODE_FAILURES {
                                    warn!(
                                        key = %key,
                                        error = %e,
                                        "object failed to decode {MAX_OBJECT_DECODE_FAILURES} times; quarantining (advancing cursor past it)"
                                    );
                                    self.save_cursor(&key)
                                        .await
                                        .map_err(ConsumerError::Connection)?;
                                    self.decode_failing_key = None;
                                    self.decode_failures = 0;
                                    tokio::time::sleep(self.idle_delay).await;
                                    return Ok(empty_batch());
                                }
                                // Under the limit: retry the same object next poll, but do
                                // NOT surface a Connection error — that would rebuild the
                                // consumer (resetting decode_failures/decode_failing_key) and
                                // let a poison object loop forever without ever quarantining.
                                // Idle instead so the in-instance counter keeps climbing to
                                // MAX_OBJECT_DECODE_FAILURES.
                                warn!(
                                    key = %key,
                                    error = %e,
                                    attempt = self.decode_failures,
                                    max = MAX_OBJECT_DECODE_FAILURES,
                                    "object failed to decode; will retry before quarantining"
                                );
                                tokio::time::sleep(self.idle_delay).await;
                                return Ok(empty_batch());
                            }
                        };
                        let records = split_and_parse(&data, &self.delimiter, &self.format);
                        if records.is_empty() {
                            // No data records (e.g. a lone CSV header): advance past it so we
                            // don't re-list it forever, then idle.
                            self.save_cursor(&key)
                                .await
                                .map_err(ConsumerError::Connection)?;
                            tokio::time::sleep(self.idle_delay).await;
                            return Ok(empty_batch());
                        }
                        let n = records.len();
                        *self.buffer.lock().await = records;
                        *self.progress.lock().await = Some(ObjProgress { key, remaining: n });
                    }
                }
            } else if buffer_empty {
                // Buffer drained but commits for the current object are still pending; the
                // commit will clear `progress`. Idle rather than fetch the next object.
                tokio::time::sleep(self.idle_delay).await;
                return Ok(empty_batch());
            }
        }

        let batch: Vec<CanonicalMessage> = {
            let mut buffer = self.buffer.lock().await;
            let count = buffer.len().min(max_messages);
            buffer.drain(0..count).collect()
        };

        let buffer_arc = self.buffer.clone();
        let progress_arc = self.progress.clone();
        let last_key_arc = self.last_key.clone();
        let checkpoint = self.checkpoint.clone();
        let batch_for_commit = batch.clone();

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Count the leading run of acks; the first nack and everything after it is
                // requeued to the front of the buffer for at-least-once re-delivery.
                let mut leading_acks = 0usize;
                let mut requeue = Vec::new();
                let mut hit_nack = false;
                for (i, d) in dispositions.iter().enumerate() {
                    if hit_nack {
                        if let Some(m) = batch_for_commit.get(i) {
                            requeue.push(m.clone());
                        }
                        continue;
                    }
                    match d {
                        MessageDisposition::Ack | MessageDisposition::Reply(_) => leading_acks += 1,
                        MessageDisposition::Nack => {
                            hit_nack = true;
                            if let Some(m) = batch_for_commit.get(i) {
                                requeue.push(m.clone());
                            }
                        }
                    }
                }
                // Defensive: requeue any tail the dispositions didn't cover.
                if dispositions.len() < batch_for_commit.len() {
                    for m in &batch_for_commit[dispositions.len()..] {
                        requeue.push(m.clone());
                    }
                }

                if !requeue.is_empty() {
                    let mut buf = buffer_arc.lock().await;
                    let old = std::mem::take(&mut *buf);
                    let mut new = requeue;
                    new.extend(old);
                    *buf = new;
                }

                if leading_acks > 0 {
                    let mut prog = progress_arc.lock().await;
                    if let Some(p) = prog.as_mut() {
                        p.remaining = p.remaining.saturating_sub(leading_acks);
                        if p.remaining == 0 {
                            // Object fully acked. Persist the resume cursor durably BEFORE
                            // advancing: if the save fails, drop the in-flight object without
                            // advancing so it is re-listed and re-emitted (at-least-once), and
                            // surface the error rather than acking progress that isn't durable.
                            let key = p.key.clone();
                            if let Some(cp) = &checkpoint {
                                if let Err(e) = cp.save(&key).await {
                                    *prog = None;
                                    return Err(anyhow!(e).context("persist object-store cursor"));
                                }
                            }
                            *prog = None;
                            drop(prog);
                            *last_key_arc.lock().await = Some(key);
                        }
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch {
            messages: batch,
            commit,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::source_ranges::SourcePartition;
    use crate::traits::{MessageConsumer, MessagePublisher};
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Key an idempotent write lands on, derived rather than spelled out: the part-name
    /// format is pinned by the `source_ranges` tests, not by these.
    fn part_key(start: u64, end: u64, extension: &str) -> ObjPath {
        let source = SourcePartition {
            topic: "orders".to_string(),
            partition: 0,
        };
        ObjPath::from(format!(
            "data/{}",
            finalized_name(&source, start, end, extension).unwrap()
        ))
    }

    #[test]
    fn checkpoint_overlap_detection() {
        // Same bucket, checkpoint nested under the source prefix -> overlap.
        assert!(object_urls_overlap(
            "s3://bucket/data",
            "s3://bucket/data/cursors"
        ));
        // Identical location -> overlap.
        assert!(object_urls_overlap("s3://bucket/data", "s3://bucket/data"));
        // Sibling prefixes in the same bucket -> safe.
        assert!(!object_urls_overlap(
            "s3://bucket/data",
            "s3://bucket/cursors"
        ));
        // String-prefix but distinct segment -> safe.
        assert!(!object_urls_overlap(
            "s3://bucket/data",
            "s3://bucket/database"
        ));
        // Different bucket -> safe.
        assert!(!object_urls_overlap("s3://bucket/data", "s3://other/data"));
    }

    #[test]
    fn civil_date_from_unix_ms() {
        // 2026-07-17T00:00:00Z = 1_784_246_400_000 ms.
        assert_eq!(civil_from_unix_ms(1_784_246_400_000), (2026, 7, 17));
        // Epoch.
        assert_eq!(civil_from_unix_ms(0), (1970, 1, 1));
        // A leap day: 2024-02-29T12:00:00Z.
        assert_eq!(civil_from_unix_ms(1_709_208_000_000), (2024, 2, 29));
    }

    #[test]
    fn extension_reflects_compression_and_encryption() {
        assert_eq!(
            extension_for(&FileFormat::Normal, Compression::None, false),
            "jsonl"
        );
        assert_eq!(
            extension_for(&FileFormat::Normal, Compression::Gzip, false),
            "jsonl.gz"
        );
        assert_eq!(
            extension_for(&FileFormat::Raw, Compression::Lz4, false),
            "bin.lz4"
        );
        assert_eq!(
            extension_for(&FileFormat::Normal, Compression::Zstd, false),
            "jsonl.zst"
        );
        // Encrypted objects are ciphertext -> trailing `.enc`, never a bare `.gz`.
        assert_eq!(
            extension_for(&FileFormat::Normal, Compression::Gzip, true),
            "jsonl.gz.enc"
        );
        assert_eq!(
            extension_for(&FileFormat::Raw, Compression::None, true),
            "bin.enc"
        );
    }

    fn json_msg(v: serde_json::Value) -> CanonicalMessage {
        CanonicalMessage::new(serde_json::to_vec(&v).unwrap(), None)
    }

    fn test_publisher(store: Arc<dyn ObjectStore>) -> ObjectStorePublisher {
        ObjectStorePublisher {
            store,
            base: ObjPath::from("data"),
            delimiter: vec![b'\n'],
            format: FileFormat::Normal,
            #[cfg(feature = "compression")]
            compression: Compression::None,
            #[cfg(feature = "encryption")]
            crypto: None,
            date_partition: false,
            extension: "jsonl".to_string(),
            name_by: NameBy::WriteTime,
            covered_ranges: Arc::new(Mutex::new(CoveredRanges::default())),
            recovered: Arc::new(tokio::sync::OnceCell::new()),
            keys: Arc::new(StdMutex::new(SequentialGenerator::new())),
        }
    }

    #[test]
    fn object_keys_sort_in_allocation_order_across_clones() {
        let publisher = test_publisher(Arc::new(InMemory::new()));
        let clone = publisher.clone();
        let keys: Vec<String> = (0..1000)
            .map(|i| {
                if i % 2 == 0 { &publisher } else { &clone }
                    .next_key()
                    .to_string()
            })
            .collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    fn kafka_message(offset: i64) -> CanonicalMessage {
        let mut message = json_msg(serde_json::json!({ "offset": offset }));
        message
            .metadata
            .insert("mqb.src.kafka_topic".into(), "orders".into());
        message
            .metadata
            .insert("mqb.src.kafka_partition".into(), "0".into());
        message
            .metadata
            .insert("mqb.src.kafka_offset".into(), offset.to_string());
        message
    }

    /// A store whose `put_opts` parks on a barrier, so a put can only complete once as many
    /// puts are in flight as the barrier expects. A publisher that serialises its writes
    /// never gets there and hangs, which is what turns "slower" into a failing test.
    #[derive(Debug)]
    struct BarrierStore {
        inner: InMemory,
        puts: tokio::sync::Barrier,
    }

    impl BarrierStore {
        fn new(concurrent_puts: usize) -> Self {
            Self {
                inner: InMemory::new(),
                puts: tokio::sync::Barrier::new(concurrent_puts),
            }
        }
    }

    impl std::fmt::Display for BarrierStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "BarrierStore")
        }
    }

    #[async_trait]
    impl ObjectStore for BarrierStore {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.puts.wait().await;
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjPath>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjPath,
            to: &ObjPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// Counts `list` calls, so a test can pin how often recovery scans the prefix.
    #[derive(Debug)]
    struct CountingStore {
        inner: InMemory,
        lists: Arc<AtomicUsize>,
    }

    impl std::fmt::Display for CountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CountingStore")
        }
    }

    #[async_trait]
    impl ObjectStore for CountingStore {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjPath>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjPath,
            to: &ObjPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// `list` fails, `put` works. `permanent` picks whether the failure is one retrying can
    /// ever clear.
    #[derive(Debug)]
    struct UnlistableStore {
        inner: InMemory,
        permanent: bool,
        lists: Arc<AtomicUsize>,
    }

    impl std::fmt::Display for UnlistableStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "UnlistableStore")
        }
    }

    #[async_trait]
    impl ObjectStore for UnlistableStore {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            _prefix: Option<&ObjPath>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.lists.fetch_add(1, Ordering::SeqCst);
            let permanent = self.permanent;
            futures::stream::once(async move {
                Err(if permanent {
                    ObjectStoreError::PermissionDenied {
                        path: "data".to_string(),
                        source: "listing denied".into(),
                    }
                } else {
                    ObjectStoreError::Generic {
                        store: "UnlistableStore",
                        source: "listing unavailable".into(),
                    }
                })
            })
            .boxed()
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjPath,
            to: &ObjPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// Recovery is an optimisation and runs only once a PUT reported the object already on the
    /// store, so a bucket that denies `ListObjects` must cost throughput, not the batch. The
    /// old code propagated the listing error and failed a replay whose data was already safe.
    #[tokio::test]
    async fn a_replay_survives_a_prefix_it_may_not_list() {
        let store: Arc<dyn ObjectStore> = Arc::new(UnlistableStore {
            inner: InMemory::new(),
            permanent: false,
            lists: Arc::new(AtomicUsize::new(0)),
        });
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;

        let batch = || vec![kafka_message(0), kafka_message(1)];
        assert!(matches!(
            publisher.send_batch(batch()).await.unwrap(),
            SentBatch::Ack
        ));

        // Same offsets again, on a fresh publisher so nothing is remembered in-process. The
        // listing it wants before writing is the one this store refuses.
        let mut replay = test_publisher(store.clone());
        replay.name_by = NameBy::SourcePosition;
        assert!(
            matches!(replay.send_batch(batch()).await.unwrap(), SentBatch::Ack),
            "a failed prefix listing must not fail a batch that is already on the store"
        );
        assert!(
            !replay.recovered.initialized(),
            "a failed recovery must stay retryable"
        );
    }

    /// The idempotent path must not trade the worker pool for its ordering guarantee. Naming
    /// objects by source range is what makes concurrent writes safe, so the range bookkeeping
    /// may only be locked around the map itself — never around the encode and the PUT, which
    /// would leave an ordered sink publishing one object at a time.
    #[tokio::test]
    async fn idempotent_sends_are_not_serialised_by_the_covered_range_lock() {
        let store = Arc::new(BarrierStore::new(2));
        let mut publisher = test_publisher(store);
        publisher.name_by = NameBy::SourcePosition;
        let other = publisher.clone();

        // Disjoint offset runs, so both batches genuinely write and neither is skipped.
        let first = tokio::spawn(async move {
            publisher
                .send_batch_by_source_position(vec![kafka_message(0)])
                .await
        });
        let second = tokio::spawn(async move {
            other
                .send_batch_by_source_position(vec![kafka_message(1)])
                .await
        });

        let sends = async {
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
        };
        tokio::time::timeout(Duration::from_secs(10), sends)
            .await
            .expect("both puts must be in flight at once; the covered-range lock is being held across the put");
    }

    /// The guarantee the idempotent sink exists to make, and the one the whole ordering
    /// thread turns on: object order is *source* order even when batches are written
    /// concurrently and out of order. Dispatches the batches in reverse and lets them race,
    /// which is what the worker pool does to source order above `concurrency: 1`.
    #[tokio::test]
    async fn idempotent_objects_replay_in_source_order_when_written_concurrently() {
        const BATCHES: i64 = 10;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        // Raw writes the payload verbatim, so a record read back is the source row itself.
        publisher.format = FileFormat::Raw;

        let mut writes = tokio::task::JoinSet::new();
        for batch in (0..BATCHES).rev() {
            let publisher = publisher.clone();
            writes.spawn(async move {
                let base = batch * 2;
                publisher
                    .send_batch(vec![kafka_message(base), kafka_message(base + 1)])
                    .await
            });
        }
        while let Some(joined) = writes.join_next().await {
            joined.unwrap().unwrap();
        }

        // Lexicographic listing is the only ordering a bucket offers, so that is what a
        // reader gets and what this has to reproduce.
        let mut stream = store.list(Some(&ObjPath::from("data")));
        let mut names = Vec::new();
        while let Some(item) = stream.next().await {
            names.push(item.unwrap().location);
        }
        names.sort();
        assert_eq!(names.len(), BATCHES as usize, "one part file per batch");

        let mut replayed = Vec::new();
        for name in &names {
            let bytes = store.get(name).await.unwrap().bytes().await.unwrap();
            for record in split_records(&bytes, b"\n") {
                let value: serde_json::Value = serde_json::from_slice(record).unwrap();
                replayed.push(value["offset"].as_i64().unwrap());
            }
        }
        assert_eq!(replayed, (0..BATCHES * 2).collect::<Vec<_>>());
    }

    /// Recovery is an optimisation, not a correctness requirement, so it must not cost a
    /// prefix-wide LIST on the common path: writing into a fresh prefix never triggers it.
    /// Recovery has to run before the first write, so it can no longer be free on a fresh
    /// prefix — but it must stay *one* listing per publisher rather than one per batch.
    #[tokio::test]
    async fn a_prefix_is_listed_once_per_publisher() {
        let lists = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
            inner: InMemory::new(),
            lists: lists.clone(),
        });
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        for offset in 0..8 {
            publisher
                .send_batch(vec![kafka_message(offset)])
                .await
                .unwrap();
        }
        assert_eq!(
            lists.load(Ordering::SeqCst),
            1,
            "eight batches must share one listing, not pay for one each"
        );

        let mut written = store.list(Some(&ObjPath::from("data")));
        let mut count = 0;
        while written.next().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 8, "an empty prefix must not filter anything out");
    }

    /// Recovery now runs on every batch until it succeeds, so a bucket that will never allow
    /// `ListObjects` must not pay a failed round trip and a warning per batch for the life of
    /// the route.
    #[tokio::test]
    async fn a_permanently_denied_listing_is_only_attempted_once() {
        let lists = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn ObjectStore> = Arc::new(UnlistableStore {
            inner: InMemory::new(),
            permanent: true,
            lists: lists.clone(),
        });
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        for offset in 0..5 {
            publisher
                .send_batch(vec![kafka_message(offset)])
                .await
                .unwrap();
        }
        assert_eq!(
            lists.load(Ordering::SeqCst),
            1,
            "a listing that is denied outright must not be retried per batch"
        );
        for offset in 0..5u64 {
            assert!(
                store.get(&part_key(offset, offset, "jsonl")).await.is_ok(),
                "the sink must keep writing without the listing"
            );
        }
    }

    /// The replay that the collision-triggered recovery got wrong: a restart whose batches
    /// fall on different boundaries names different objects, so not one PUT collides and
    /// nothing would ever ask for the listing. Every record would then be written a second
    /// time under a name that overlaps the first run's without matching it.
    #[tokio::test]
    async fn a_replay_on_shifted_batch_boundaries_writes_nothing_twice() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut first = test_publisher(store.clone());
        first.name_by = NameBy::SourcePosition;
        for chunk in [[0, 1], [2, 3], [4, 5]] {
            first
                .send_batch(chunk.map(kafka_message).to_vec())
                .await
                .unwrap();
        }

        // The same six offsets, re-read in threes. No name in common with the run above.
        let mut replay = test_publisher(store.clone());
        replay.name_by = NameBy::SourcePosition;
        for chunk in [[0, 1, 2], [3, 4, 5]] {
            replay
                .send_batch(chunk.map(kafka_message).to_vec())
                .await
                .unwrap();
        }

        assert_eq!(
            keys(store.as_ref()).await,
            vec![
                "data/part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl",
                "data/part-orders-0000000000-00000000000000000002-00000000000000000003.jsonl",
                "data/part-orders-0000000000-00000000000000000004-00000000000000000005.jsonl",
            ],
            "the replay must add no object; before the fix it added three that overlap these"
        );
    }

    /// Sorted keys under the test prefix.
    async fn keys(store: &dyn ObjectStore) -> Vec<String> {
        let mut stream = store.list(Some(&ObjPath::from("data")));
        let mut names = Vec::new();
        while let Some(item) = stream.next().await {
            names.push(item.unwrap().location.to_string());
        }
        names.sort();
        names
    }

    /// A restart into a prefix that already holds parts recovers exactly once, before its
    /// first write, and then skips the rest of the replay without rewriting anything.
    #[tokio::test]
    async fn a_replay_recovers_once_and_then_skips() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut first = test_publisher(store.clone());
        first.name_by = NameBy::SourcePosition;
        first
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();

        // A restarted publisher starts with no knowledge of the prefix at all.
        let mut restarted = test_publisher(store.clone());
        restarted.name_by = NameBy::SourcePosition;
        restarted
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();
        assert!(
            restarted.recovered.initialized(),
            "a populated prefix must be recovered before it is written to"
        );

        // The replayed offsets are now known covered, so the next batch writes only the new one.
        restarted
            .send_batch(vec![kafka_message(0), kafka_message(1), kafka_message(2)])
            .await
            .unwrap();
        let mut stream = store.list(Some(&ObjPath::from("data")));
        let mut names = Vec::new();
        while let Some(item) = stream.next().await {
            names.push(item.unwrap().location.to_string());
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                "data/part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl"
                    .to_string(),
                "data/part-orders-0000000000-00000000000000000002-00000000000000000002.jsonl"
                    .to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn idempotent_sink_recovers_finalized_objects_and_leaves_foreign_keys_alone() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        publisher
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();
        publisher
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();

        store
            .put(
                &ObjPath::from("data/.stage-crash"),
                PutPayload::from(Vec::new()),
            )
            .await
            .unwrap();
        let mut restarted = test_publisher(store.clone());
        restarted.name_by = NameBy::SourcePosition;
        restarted.covered_ranges = Arc::new(Mutex::new(
            recover_finalized_object_ranges(store.as_ref(), &restarted.base, "jsonl")
                .await
                .unwrap(),
        ));
        restarted
            .send_batch(vec![kafka_message(0), kafka_message(1), kafka_message(2)])
            .await
            .unwrap();

        let mut stream = store.list(Some(&ObjPath::from("data")));
        let mut names = Vec::new();
        while let Some(item) = stream.next().await {
            names.push(item.unwrap().location.to_string());
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                // Not a parseable part name, and not ours to delete.
                "data/.stage-crash".to_string(),
                part_key(0, 1, "jsonl").to_string(),
                part_key(2, 2, "jsonl").to_string(),
            ]
        );
    }

    /// `list` is recursive, so a sink whose base is a *sub*prefix of ours shows up in our
    /// listing with a perfectly parseable part name. Counting it would mark that range
    /// covered here and silently drop the records that were never written under our prefix.
    #[tokio::test]
    async fn recovery_ignores_parseable_parts_under_a_nested_prefix() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let nested = ObjPath::from(format!(
            "data/other/{}",
            part_key(0, 1, "jsonl").filename().unwrap()
        ));
        store
            .put(&nested, PutPayload::from("someone else's part"))
            .await
            .unwrap();

        let base = ObjPath::from("data");
        let covered = recover_finalized_object_ranges(store.as_ref(), &base, "jsonl")
            .await
            .unwrap();

        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        publisher.covered_ranges = Arc::new(Mutex::new(covered));
        publisher
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();

        let written = store
            .get(&part_key(0, 1, "jsonl"))
            .await
            .expect("the nested part must not count as coverage")
            .bytes()
            .await
            .unwrap();
        assert!(!written.is_empty());
    }

    #[tokio::test]
    async fn idempotent_sink_does_not_overwrite_an_existing_range_object() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = part_key(0, 1, "jsonl");
        store
            .put(&key, PutPayload::from("already committed"))
            .await
            .unwrap();

        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        publisher
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .expect("an existing deterministic object is a successful retry");

        let stored = store.get(&key).await.unwrap().bytes().await.unwrap();
        assert_eq!(stored.as_ref(), b"already committed");
    }

    #[tokio::test]
    async fn source_position_overrides_date_partition_rather_than_rejecting_it() {
        // `auto` resolves to source_position on most ETL inputs, so erroring here would
        // reject configs that never asked for the two to be combined.
        let publisher = ObjectStorePublisher::new(&ObjectStoreConfig {
            url: "memory:///data".to_string(),
            name_by: NameBy::SourcePosition,
            date_partition: Some(true),
            ..Default::default()
        })
        .await
        .expect("date_partition is overridden, not rejected");
        assert!(!publisher.date_partition);
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn idempotent_parts_are_compressed_and_named_for_it() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut publisher = test_publisher(store.clone());
        publisher.name_by = NameBy::SourcePosition;
        publisher.compression = Compression::Gzip;
        publisher.extension = extension_for(&FileFormat::Json, Compression::Gzip, false);
        publisher
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();

        // The part name advertises the codec, and the bytes really are gzip.
        let key = part_key(0, 1, "jsonl.gz");
        let stored = store.get(&key).await.unwrap().bytes().await.unwrap();
        let plain =
            crate::support::compression::decompress_all(Compression::Gzip, &stored, None).unwrap();
        assert_eq!(split_records(&plain, b"\n").len(), 2);

        // A restart parses the longer extension back out, so covered offsets are not rewritten.
        let recovered =
            recover_finalized_object_ranges(store.as_ref(), &publisher.base, "jsonl.gz")
                .await
                .unwrap();
        let mut restarted = test_publisher(store.clone());
        restarted.name_by = NameBy::SourcePosition;
        restarted.compression = Compression::Gzip;
        restarted.extension = publisher.extension.clone();
        restarted.covered_ranges = Arc::new(Mutex::new(recovered));
        restarted
            .send_batch(vec![kafka_message(0), kafka_message(1)])
            .await
            .unwrap();

        let mut stream = store.list(Some(&ObjPath::from("data")));
        let mut names = Vec::new();
        while let Some(item) = stream.next().await {
            names.push(item.unwrap().location.to_string());
        }
        assert_eq!(names, vec![part_key(0, 1, "jsonl.gz").to_string()]);
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn compressed_object_round_trips() {
        for compression in [Compression::Gzip, Compression::Lz4, Compression::Zstd] {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

            let mut publisher = test_publisher(store.clone());
            publisher.format = FileFormat::Raw;
            publisher.compression = compression;
            publisher.extension = extension_for(&FileFormat::Raw, compression, false);
            publisher
                .send_batch(vec![
                    json_msg(serde_json::json!({"n": 1})),
                    json_msg(serde_json::json!({"n": 2})),
                ])
                .await
                .unwrap();

            // The stored object is really compressed: it decodes to the two JSONL rows.
            let listed = store
                .list(Some(&ObjPath::from("data")))
                .next()
                .await
                .unwrap()
                .unwrap();
            let suffix = match compression {
                Compression::Gzip => ".bin.gz",
                Compression::Zstd => ".bin.zst",
                _ => ".bin.lz4",
            };
            assert!(listed.location.to_string().ends_with(suffix));
            let bytes = store
                .get(&listed.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let decoded =
                crate::support::compression::decompress_all(compression, &bytes, None).unwrap();
            assert_eq!(String::from_utf8(decoded).unwrap().lines().count(), 2);

            let mut consumer = ObjectStoreConsumer::from_store(
                store,
                ObjPath::from("data"),
                FileFormat::Raw,
                None,
                None,
            );
            consumer.compression = compression;
            let batch = consumer.receive_batch(10).await.unwrap();
            assert_eq!(batch.messages.len(), 2);
            assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
            assert_eq!(batch.messages[1].payload.as_ref(), br#"{"n":2}"#);
            (batch.commit)(vec![MessageDisposition::Ack; 2])
                .await
                .unwrap();
        }
    }

    #[cfg(all(feature = "compression", feature = "encryption"))]
    #[tokio::test]
    async fn encrypted_object_round_trips() {
        use base64::Engine as _;

        let crypto_cfg = crate::models::EncryptionConfig {
            key: base64::engine::general_purpose::STANDARD.encode([42u8; 32]),
            ..Default::default()
        };
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut publisher = test_publisher(store.clone());
        publisher.format = FileFormat::Raw;
        publisher.compression = Compression::Gzip;
        publisher.crypto = Some(Arc::new(Crypto::new(&crypto_cfg).unwrap()));
        publisher
            .send_batch(vec![json_msg(serde_json::json!({"who": "alice"}))])
            .await
            .unwrap();

        // The stored object is ciphertext: not gzip, and no plaintext inside.
        let listed = store
            .list(Some(&ObjPath::from("data")))
            .next()
            .await
            .unwrap()
            .unwrap();
        let bytes = store
            .get(&listed.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(
            crate::support::compression::decompress_all(Compression::Gzip, &bytes, None).is_err()
        );
        assert!(!bytes.windows(5).any(|w| w == b"alice"));

        let mut consumer = ObjectStoreConsumer::from_store(
            store.clone(),
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        consumer.compression = Compression::Gzip;
        consumer.crypto = Some(Arc::new(Crypto::new(&crypto_cfg).unwrap()));
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"who":"alice"}"#);

        // A consumer with the wrong key fails cleanly.
        let mut wrong = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        wrong.compression = Compression::Gzip;
        let wrong_cfg = crate::models::EncryptionConfig {
            key: base64::engine::general_purpose::STANDARD.encode([1u8; 32]),
            ..Default::default()
        };
        wrong.crypto = Some(Arc::new(Crypto::new(&wrong_cfg).unwrap()));
        // A wrong-key object is bounded-retried then quarantined: it never delivers
        // garbage and never surfaces a Connection error (which would rebuild the consumer,
        // reset the failure counter, and loop the poison object forever).
        for _ in 0..=MAX_OBJECT_DECODE_FAILURES {
            let batch = wrong
                .receive_batch(10)
                .await
                .expect("wrong-key object must not error out the source");
            assert!(batch.messages.is_empty());
        }
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn gzip_source_quarantines_non_gzip_object() {
        // A gzip source must handle a non-gzip object cleanly (not panic): the object is
        // bounded-retried then quarantined, never delivered as data and never surfaced as a
        // Connection error that would rebuild the consumer and loop the poison object.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjPath::from("data/not-gzip.jsonl.gz"),
                PutPayload::from(br#"{"n":1}"#.to_vec()),
            )
            .await
            .unwrap();

        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        consumer.compression = Compression::Gzip;
        for _ in 0..=MAX_OBJECT_DECODE_FAILURES {
            let batch = consumer
                .receive_batch(10)
                .await
                .expect("non-gzip object must not error out the source");
            assert!(batch.messages.is_empty());
        }
    }

    #[tokio::test]
    async fn sink_writes_object_and_source_round_trips() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let publisher = test_publisher(store.clone());
        publisher
            .send_batch(vec![
                json_msg(serde_json::json!({"n": 1})),
                json_msg(serde_json::json!({"n": 2})),
            ])
            .await
            .unwrap();

        // One object exists under the prefix.
        let mut listed = store.list(Some(&ObjPath::from("data")));
        let first = listed.next().await.unwrap().unwrap();
        assert!(first.location.to_string().starts_with("data/"));

        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Normal,
            None,
            None,
        );

        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
        assert_eq!(batch.messages[1].payload.as_ref(), br#"{"n":2}"#);
        (batch.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .unwrap();

        // Cursor advanced past the object; a further read is idle (empty).
        let drained = consumer.receive_batch(10).await.unwrap();
        assert!(drained.messages.is_empty());
    }

    #[tokio::test]
    async fn local_filesystem_sink_and_source_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}", dir.path().display()),
            format: FileFormat::Normal,
            date_partition: Some(false),
            polling_interval_ms: Some(1),
            ..Default::default()
        };

        let publisher = ObjectStorePublisher::new(&config).await.unwrap();
        publisher
            .send_batch(vec![json_msg(serde_json::json!({"source": "local"}))])
            .await
            .unwrap();

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"source":"local"}"#);
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        let drained = consumer.receive_batch(10).await.unwrap();
        assert!(drained.messages.is_empty());
    }

    #[tokio::test]
    async fn local_filesystem_source_reads_csv_drop_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("2026-09-03-orders.csv"),
            b"id,name\n1,Ada\n2,Grace\n",
        )
        .unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}", dir.path().display()),
            format: FileFormat::Csv,
            polling_interval_ms: Some(1),
            ..Default::default()
        };

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(
            batch.messages[0].payload.as_ref(),
            br#"{"id":"1","name":"Ada"}"#
        );
        assert_eq!(
            batch.messages[1].payload.as_ref(),
            br#"{"id":"2","name":"Grace"}"#
        );
        (batch.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn nacked_records_are_redelivered() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let publisher = test_publisher(store.clone());
        publisher
            .send_batch(vec![
                json_msg(serde_json::json!({"n": 1})),
                json_msg(serde_json::json!({"n": 2})),
            ])
            .await
            .unwrap();

        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Normal,
            None,
            None,
        );

        // Ack the first, nack the second -> the second is requeued.
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        (batch.commit)(vec![MessageDisposition::Ack, MessageDisposition::Nack])
            .await
            .unwrap();

        // The nacked record is redelivered (object not yet fully acked).
        let retry = consumer.receive_batch(10).await.unwrap();
        assert_eq!(retry.messages.len(), 1);
        assert_eq!(retry.messages[0].payload.as_ref(), br#"{"n":2}"#);
        (retry.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        let drained = consumer.receive_batch(10).await.unwrap();
        assert!(drained.messages.is_empty());
    }
}
