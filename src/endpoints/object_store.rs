//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
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

use crate::CanonicalMessage;
use crate::checkpoint::{self, CheckpointBackend, CheckpointStore, VersionedCheckpoint};
use crate::codec::CsvDialect;
use crate::codec::csv::{CsvHeader, csv_append_record, parse_delimiter};
use crate::codec::format::{decode_records, encode_record};
use crate::codec::framing::split_records;
use crate::models::{
    Compression, DatePartitionStyle, DecodeErrorPolicy, FileFormat, NameBy, ObjectStoreConfig,
    ObjectStoreFsync,
};
#[cfg(feature = "encryption")]
use crate::support::crypto::Crypto;
use crate::support::source_ranges::{
    CoveredRanges, SourcePartition, SourceRun, finalized_name, parse_finalized_name,
};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, ReceivedBatch, SentBatch,
};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use fast_uuid_v7::SequentialGenerator;
use futures::{StreamExt, TryStreamExt};
use object_store::local::LocalFileSystem;
use object_store::{
    Error as ObjectStoreError, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
    path::Path as ObjPath,
};
use std::any::Any;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, trace, warn};

/// Builds an `object_store` backend and its base prefix `Path` from a URL, reading
/// credentials from the environment. Shared with the object-store checkpoint backend so
/// both resolve creds identically — see [`checkpoint::object_store_backend::build_store`].
use crate::checkpoint::object_store_backend::{build_store, build_store_with_fsync};
use crate::support::local_staging::{
    LocalStaging, STAGING_REAP_AGE, StagedName, remove_suffixed_leftovers,
};

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
        // The codec lives inside the Parquet file, so there is no outer compression suffix.
        FileFormat::Parquet => "parquet",
    };
    let mut ext = match compression {
        _ if *format == FileFormat::Parquet => base.to_string(),
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

/// Rejects `format`/`compression`/`encryption` settings whose Cargo feature is missing.
/// Whether `name` is an object this sink writes: a source-range part or a uuid name.
fn is_own_object_name(name: &str, extension: &str) -> bool {
    parse_finalized_name(name, extension).is_some()
        || name
            .strip_suffix(extension)
            .and_then(|stem| stem.strip_suffix('.'))
            .is_some_and(|id| {
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            })
}

fn validate_object_settings(_config: &ObjectStoreConfig) -> anyhow::Result<()> {
    #[cfg(not(feature = "parquet"))]
    if _config.format == FileFormat::Parquet {
        return Err(anyhow!(
            "object_store 'format: parquet' requires the `parquet` feature"
        ));
    }
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

/// Splits and decodes an object's bytes into messages; the first CSV row establishes the
/// schema, and a large object is decoded across cores like a file batch.
fn split_and_parse(
    data: bytes::Bytes,
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
) -> Vec<CanonicalMessage> {
    let csv = matches!(format, FileFormat::Csv).then_some(csv);
    let spans = split_records(&data, delimiter, csv);
    let mut csv_header = csv.map(|csv| Arc::new(CsvHeader::unread(csv.clone())));
    decode_records(&mut Vec::from(data), &spans, format, &mut csv_header, false)
}

/// One object's worth of encoded records: delimited bytes, or the rows of a Parquet body.
#[derive(Default)]
struct ObjectBody {
    bytes: Vec<u8>,
    #[cfg(feature = "parquet")]
    rows: crate::support::parquet::RowBuffer,
    /// The CSV columns, settled by this object's first row; every object has its own header.
    csv_columns: Option<Vec<String>>,
    csv_row: Vec<u8>,
}

impl ObjectBody {
    fn push(
        &mut self,
        msg: &CanonicalMessage,
        format: &FileFormat,
        delimiter: &[u8],
        csv: &CsvDialect,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "parquet")]
        if *format == FileFormat::Parquet {
            return self.rows.push(&msg.payload);
        }
        if *format == FileFormat::Csv {
            csv_append_record(
                &mut self.bytes,
                msg,
                &mut self.csv_columns,
                &mut self.csv_row,
                delimiter,
                csv,
                true,
            )?;
            return Ok(());
        }
        self.bytes
            .extend_from_slice(&encode_record(msg, format, delimiter)?);
        self.bytes.extend_from_slice(delimiter);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "parquet")]
        if !self.rows.is_empty() {
            return false;
        }
        self.bytes.is_empty()
    }
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
    csv: CsvDialect,
    #[cfg(feature = "compression")]
    compression: Compression,
    #[cfg(feature = "encryption")]
    crypto: Option<Arc<Crypto>>,
    date_partition: bool,
    hive_partition: bool,
    extension: String,
    name_by: NameBy,
    /// Ranges already on the store, filled by one listing before the first idempotent write.
    /// A fresh prefix pays a single empty listing; `recovered` keeps it to one per publisher.
    covered_ranges: Arc<Mutex<CoveredRanges>>,
    recovered: Arc<tokio::sync::OnceCell<()>>,
    // Shared across clones on purpose: the ordering guarantee is per generator instance, so
    // two independent generators would hand out interleaved keys.
    keys: Arc<StdMutex<SequentialGenerator>>,
    /// Set for a local store that syncs objects in groups; objects then go through it.
    staging: Option<Arc<LocalStaging>>,
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
        validate_object_settings(config)?;
        if config.fsync == Some(ObjectStoreFsync::Off) && name_by == NameBy::SourcePosition {
            warn!(url = %config.url, "object_store 'fsync: off' with 'name_by: source_position': after a power loss an empty part can count as written and its range is not replayed");
        }
        let local = config.url.starts_with("file:");
        let staged = local
            && !matches!(
                config.fsync,
                Some(ObjectStoreFsync::Off | ObjectStoreFsync::Batch)
            );
        let (store, base) =
            build_store_with_fsync(&config.url, config.fsync == Some(ObjectStoreFsync::Batch))?;
        let store: Arc<dyn ObjectStore> = Arc::from(store);
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let csv = CsvDialect::for_format(&config.format, &config.csv, &delimiter)?;
        csv.check_sink()?;
        let extension = config.extension.clone().unwrap_or_else(|| {
            extension_for(
                &config.format,
                config.compression,
                config.encryption.is_some(),
            )
        });
        if name_by == NameBy::SourcePosition && config.date_partition == Some(true) {
            warn!(
                "object_store 'date_partition' does not apply to 'name_by: source_position'; part names carry the source range and are written flat under the prefix"
            );
        }
        let staging = if staged {
            let extension = extension.clone();
            let root = LocalFileSystem::new().path_to_filesystem(&base)?;
            let scan = root.clone();
            let removed = tokio::task::spawn_blocking(move || {
                remove_suffixed_leftovers(&scan, STAGING_REAP_AGE, |name| {
                    is_own_object_name(name, &extension)
                })
            })
            .await??;
            if removed > 0 {
                info!(dir = %root.display(), removed, "Removed objects a previous run staged but never finalized");
            }
            let interval = config
                .fsync_interval_ms
                .map_or(FINALIZE_INTERVAL, |ms| Duration::from_millis(ms.max(1)));
            Some(LocalStaging::new(root, StagedName::Suffixed, interval))
        } else {
            None
        };
        info!(url = %config.url, format = ?config.format, ?name_by, "Object-store sink opened");
        Ok(Self {
            store,
            base,
            delimiter,
            format: config.format.clone(),
            csv,
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
            hive_partition: config.date_partition_style == DatePartitionStyle::Hive,
            extension,
            name_by,
            covered_ranges: Arc::new(Mutex::new(CoveredRanges::default())),
            recovered: Arc::new(tokio::sync::OnceCell::new()),
            keys: Arc::new(StdMutex::new(SequentialGenerator::new())),
            staging,
        })
    }

    /// Writes one object, staged when the local store syncs in groups. With `create_only` an
    /// existing object is kept and the write counts as done.
    async fn write_object(
        &self,
        key: &ObjPath,
        body: Vec<u8>,
        create_only: bool,
    ) -> Result<(), PublisherError> {
        if let Some(staging) = &self.staging {
            let target = LocalFileSystem::new()
                .path_to_filesystem(key)
                .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
            let staged = if create_only {
                staging.write_new(target, body).await
            } else {
                staging.write(target, body).await
            };
            return staged.map_err(|e| {
                PublisherError::Retryable(anyhow!(e).context(format!("staging object '{key}'")))
            });
        }
        let options = PutOptions {
            mode: if create_only {
                PutMode::Create
            } else {
                PutMode::Overwrite
            },
            ..Default::default()
        };
        match self
            .store
            .put_opts(key, PutPayload::from(body), options)
            .await
        {
            Ok(_) => Ok(()),
            // Same name, same range, same bytes: the object is already the one this PUT would
            // have written. Recovery ran before the first PUT, so reaching this arm means a
            // repeat within the run rather than an unrecovered earlier one.
            Err(ObjectStoreError::AlreadyExists { .. }) if create_only => Ok(()),
            Err(error) => Err(classify_put_error(
                error,
                format!("object-store put '{key}'"),
            )),
        }
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
            let (y, m, d) = if self.hive_partition {
                (
                    format!("year={y:04}"),
                    format!("month={m:02}"),
                    format!("day={d:02}"),
                )
            } else {
                (format!("{y:04}"), format!("{m:02}"), format!("{d:02}"))
            };
            self.base
                .clone()
                .join(y.as_str())
                .join(m.as_str())
                .join(d.as_str())
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
    /// Parquet compresses its columns itself, so it skips the outer compression.
    #[allow(unused_mut)]
    fn encode_object_body(&self, body: ObjectBody) -> Result<Vec<u8>, PublisherError> {
        #[cfg(feature = "parquet")]
        let mut body = if self.format == FileFormat::Parquet {
            body.rows
                .encode(self.compression)
                .map_err(PublisherError::NonRetryable)?
        } else {
            body.bytes
        };
        #[cfg(not(feature = "parquet"))]
        let mut body = body.bytes;
        #[cfg(feature = "compression")]
        if self.compression != Compression::None && self.format != FileFormat::Parquet {
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
        let (objects, failed) = self
            .off_runtime(move |publisher| publisher.encode_runs(runs))
            .await??;
        for (source, start, end, body) in objects {
            self.put_source_range(&source, start, end, body).await?;
        }
        Ok(SentBatch::from_failures(failed))
    }

    /// Runs the CPU-bound part of a send on the blocking pool. Building and encoding a large
    /// batch takes milliseconds; on a runtime worker it stalls the route's producer, which
    /// woke this task and shares its thread.
    async fn off_runtime<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> Result<T, PublisherError> {
        let publisher = self.clone();
        match tokio::task::spawn_blocking(move || work(&publisher)).await {
            Ok(value) => Ok(value),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(PublisherError::Retryable(anyhow!(error))),
        }
    }

    fn encode_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<(Option<Vec<u8>>, FailedMessages), PublisherError> {
        let mut body = ObjectBody::default();
        let mut failed = Vec::new();
        for mut msg in messages {
            msg.strip_source_metadata();
            if let Err(e) = body.push(&msg, &self.format, &self.delimiter, &self.csv) {
                failed.push((msg, PublisherError::NonRetryable(e)));
            }
        }
        if body.is_empty() {
            return Ok((None, failed));
        }
        Ok((Some(self.encode_object_body(body)?), failed))
    }

    fn encode_runs(
        &self,
        runs: Vec<SourceRun>,
    ) -> Result<(Vec<EncodedRange>, FailedMessages), PublisherError> {
        let mut objects = Vec::new();
        let mut failed = Vec::new();
        for run in runs {
            // A record that cannot be encoded splits the run rather than failing the batch:
            // the offsets around it are still written, under names covering exactly what went
            // in, and the bad record goes to the DLQ like it would on the write-time path.
            // Encode failures are a property of the record, so a replay splits identically.
            // Only `parquet` and `csv` can fail here, on a payload that is not a JSON object.
            let mut segment_start = run.start;
            let mut segment_end = None;
            let mut body = ObjectBody::default();
            for (index, mut message) in run.messages.into_iter().enumerate() {
                let offset = run.start.saturating_add(index as u64);
                message.strip_source_metadata();
                match body.push(&message, &self.format, &self.delimiter, &self.csv) {
                    Ok(()) => {
                        segment_end = Some(offset);
                    }
                    Err(error) => {
                        if let Some(end) = segment_end.take() {
                            let body = self.encode_object_body(std::mem::take(&mut body))?;
                            objects.push((run.source.clone(), segment_start, end, body));
                        }
                        // `take` above emptied the body, and it can only be non-empty when
                        // `segment_end` was set, so there is nothing left to discard here.
                        segment_start = offset.saturating_add(1);
                        failed.push((message, PublisherError::NonRetryable(error)));
                    }
                }
            }
            if let Some(end) = segment_end {
                let body = self.encode_object_body(body)?;
                objects.push((run.source, segment_start, end, body));
            }
        }
        Ok((objects, failed))
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
        let key = self.base.clone().join(name.as_str());
        self.write_object(&key, body, true).await?;
        self.covered_ranges
            .lock()
            .await
            .insert(source.clone(), start, end)
            .map_err(PublisherError::NonRetryable)
    }
}

type FailedMessages = Vec<(CanonicalMessage, PublisherError)>;
/// An encoded object body with the source range it covers.
type EncodedRange = (SourcePartition, u64, u64, Vec<u8>);

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
        let (body, failed) = self
            .off_runtime(move |publisher| publisher.encode_batch(messages))
            .await??;
        let Some(body) = body else {
            // Every message failed to encode; nothing to write.
            return Ok(SentBatch::Partial {
                responses: None,
                failed,
            });
        };
        let key = self.next_key();
        self.write_object(&key, body, false).await?;
        trace!(key = %key, "Wrote object to object store");
        Ok(SentBatch::from_failures(failed))
    }

    async fn flush(&self) -> anyhow::Result<()> {
        if let Some(staging) = &self.staging {
            staging.finalize().await?;
        }
        Ok(())
    }

    /// Staged objects are durable only after `flush`, so the route holds commits until then.
    fn deferred_commit_interval(&self) -> Option<Duration> {
        self.staging.as_ref().map(|s| s.interval())
    }

    /// Finalizes what is still staged when the route stops.
    fn on_disconnect_hook(&self) -> Option<crate::traits::BoxFuture<'_, anyhow::Result<()>>> {
        self.staging.as_ref().map(|_| self.flush())
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

/// A fetched object's first records, its record count, and the Parquet rows still to decode.
struct DecodedObject {
    records: Vec<CanonicalMessage>,
    total: usize,
    #[cfg(feature = "parquet")]
    rows: Option<crate::support::parquet::RowStream>,
}

/// Reads objects under a prefix in key order, resuming from a durable cursor.
pub struct ObjectStoreConsumer {
    store: Arc<dyn ObjectStore>,
    base: ObjPath,
    /// `Some(true)`: the URL names one object, read just that key. `None`: undecided until
    /// the prefix lists something or the key itself appears.
    single_object: Option<bool>,
    delimiter: Vec<u8>,
    format: FileFormat,
    csv: CsvDialect,
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
    /// Rows of the current Parquet object not decoded yet.
    #[cfg(feature = "parquet")]
    rows: Option<crate::support::parquet::RowStream>,
    idle_delay: Duration,
    /// Reject objects larger than this many bytes rather than buffering them whole; `None` = no limit.
    max_object_bytes: Option<u64>,
    /// Consecutive decode failures on `decode_failing_key` before it is quarantined.
    decode_failures: u32,
    decode_failing_key: Option<String>,
    /// Until one object decoded, a failure points at the settings, not at the object.
    decoded_any: bool,
    on_decode_error: DecodeErrorPolicy,
    /// The local filesystem lists in directory order, so its listing is sorted here.
    unordered_listing: bool,
    /// Sorted rest of an unordered listing, valid while the cursor is `handed_out`.
    pending: std::collections::VecDeque<object_store::ObjectMeta>,
    handed_out: Option<String>,
    /// The directory or file of a `file:` URL.
    local_path: Option<std::path::PathBuf>,
    /// Set by a draining route: an empty listing then ends the read at once.
    draining: bool,
    /// Signalled when a batch is settled or given back.
    settled: Arc<tokio::sync::Notify>,
}

/// A batch handed out and not settled yet. Dropped without a commit, it goes back to the
/// buffer; either way the reader is woken.
struct Unsettled {
    buffer: Arc<Mutex<Vec<CanonicalMessage>>>,
    messages: Vec<CanonicalMessage>,
    settled: Arc<tokio::sync::Notify>,
}

impl Unsettled {
    fn take(&mut self) -> Vec<CanonicalMessage> {
        std::mem::take(&mut self.messages)
    }
}

impl Drop for Unsettled {
    fn drop(&mut self) {
        let mut messages = self.take();
        if messages.is_empty() {
            self.settled.notify_one();
            return;
        }
        if let Ok(mut buffer) = self.buffer.try_lock() {
            messages.append(&mut buffer);
            *buffer = messages;
            self.settled.notify_one();
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (buffer, settled) = (self.buffer.clone(), self.settled.clone());
            runtime.spawn(async move {
                let mut buffer = buffer.lock().await;
                messages.append(&mut buffer);
                *buffer = messages;
                settled.notify_one();
            });
        }
    }
}

/// How often staged local objects are finalized when nothing else flushes them.
const FINALIZE_INTERVAL: Duration = Duration::from_millis(1000);

/// Consecutive decode failures on one object before it is quarantined (skipped) so a
/// single poison object cannot block the source forever.
const MAX_OBJECT_DECODE_FAILURES: u32 = 5;

/// Opens a consumer's external checkpoint. The source URL is already part of the store key.
async fn open_checkpoint(
    backend: CheckpointBackend,
    config: &ObjectStoreConfig,
    cursor_id: &str,
) -> anyhow::Result<Arc<VersionedCheckpoint>> {
    let store = checkpoint::build_external_store(backend, &config.url, cursor_id).await?;
    Ok(Arc::new(VersionedCheckpoint::new(store, "object_store")))
}

/// The checkpoint of a source, or `None` without both `cursor_id` and `checkpoint_store`.
pub(crate) async fn cursor_checkpoint(
    config: &ObjectStoreConfig,
) -> anyhow::Result<Option<Arc<VersionedCheckpoint>>> {
    let (Some(cursor_id), Some(spec)) = (&config.cursor_id, &config.checkpoint_store) else {
        return Ok(None);
    };
    match checkpoint::parse_checkpoint_store(spec)? {
        CheckpointBackend::Source { .. } => Err(anyhow!(
            "object_store needs an external checkpoint_store (file://, s3://, postgres://, or mongodb://)"
        )),
        external => open_checkpoint(external, config, cursor_id).await.map(Some),
    }
}

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
        let csv = CsvDialect::for_format(&config.format, &config.csv, &delimiter)?;
        csv.check_source()?;

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
                        Some(open_checkpoint(external, config, cid).await?)
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
        // A cursor only moves once an object was decoded, so a resumed source has decoded one.
        let decoded_any = last_key.is_some();

        // A URL naming an object reads that object alone, like DuckDB/Polars paths.
        let single_object = if base.as_ref().is_empty() {
            Some(false)
        } else if store.head(&base).await.is_ok() {
            Some(true)
        } else {
            None
        };

        Ok(Self {
            store: Arc::from(store),
            base,
            single_object,
            csv,
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
            #[cfg(feature = "parquet")]
            rows: None,
            idle_delay: Duration::from_millis(config.polling_interval_ms.unwrap_or(1000)),
            max_object_bytes: config.max_object_bytes,
            decode_failures: 0,
            decode_failing_key: None,
            decoded_any,
            on_decode_error: config.on_decode_error,
            unordered_listing: config.url.starts_with("file:"),
            pending: Default::default(),
            handed_out: None,
            local_path: url::Url::parse(&config.url)
                .ok()
                .filter(|url| url.scheme() == "file")
                .and_then(|url| url.to_file_path().ok()),
            draining: false,
            settled: Default::default(),
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
            single_object: Some(false),
            delimiter: vec![b'\n'],
            format,
            csv: CsvDialect::default(),
            #[cfg(feature = "compression")]
            compression: Compression::None,
            #[cfg(feature = "encryption")]
            crypto: None,
            checkpoint,
            last_key: Arc::new(Mutex::new(last_key)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            progress: Arc::new(Mutex::new(None)),
            #[cfg(feature = "parquet")]
            rows: None,
            idle_delay: Duration::from_millis(10),
            max_object_bytes: None,
            decode_failures: 0,
            decode_failing_key: None,
            decoded_any: false,
            on_decode_error: DecodeErrorPolicy::Skip,
            unordered_listing: false,
            pending: Default::default(),
            handed_out: None,
            local_path: None,
            draining: false,
            settled: Default::default(),
        }
    }

    /// Fetches the next object strictly after `last` (in key order), skipping directory
    /// markers. Relies on the store listing keys in lexicographic order (S3/GCS/Azure/local
    /// /in-memory all do); `list_with_offset` also filters server-side when resuming.
    async fn next_object(
        &mut self,
        last: Option<&str>,
    ) -> anyhow::Result<Option<(String, bytes::Bytes)>> {
        // An exact key may be written after the consumer starts; decide on first sight.
        if self.single_object.is_none() {
            let listed = self.store.list(Some(&self.base)).next().await.transpose()?;
            if listed.is_some() {
                self.single_object = Some(false);
            } else if self.store.head(&self.base).await.is_ok() {
                self.single_object = Some(true);
            }
        }
        // In prefix mode the exact key still counts; it sorts before its children.
        let exact = match last {
            _ if self.single_object == Some(true) || self.base.as_ref().is_empty() => None,
            Some(k) if k >= self.base.as_ref() => None,
            _ => self.store.head(&self.base).await.ok(),
        };
        let mut stream = match last {
            _ if self.single_object == Some(true) => {
                futures::stream::once(self.store.head(&self.base)).boxed()
            }
            // `parse` keeps a listed key as it is; `from` would encode a `~` or `%` in it again.
            Some(k) => self.store.list_with_offset(
                Some(&self.base),
                &ObjPath::parse(k).unwrap_or_else(|_| ObjPath::from(k)),
            ),
            None => self.store.list(Some(&self.base)),
        };
        if let Some(meta) = exact {
            stream = futures::stream::once(async { Ok(meta) })
                .chain(stream)
                .boxed();
        }
        if self.unordered_listing && self.single_object != Some(true) {
            if last != self.handed_out.as_deref() {
                self.pending.clear();
            }
            if self.pending.is_empty() {
                let mut listed: Vec<_> = stream.try_collect().await?;
                listed.sort_unstable_by(|a, b| a.location.cmp(&b.location));
                self.pending = listed.into();
            }
            stream = futures::stream::empty().boxed();
        }
        loop {
            let meta = match self.pending.pop_front() {
                Some(meta) => meta,
                None => match stream.next().await {
                    Some(meta) => meta?,
                    None => break,
                },
            };
            let key = meta.location.to_string();
            if self.unordered_listing {
                self.handed_out = Some(key.clone());
            }
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
            let data = self.store.get(&meta.location).await?.bytes().await?;
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
    #[cfg(any(feature = "compression", feature = "parquet"))]
    fn decompressed_limit(&self) -> Option<u64> {
        const MAX_DECOMPRESSED_EXPANSION: u64 = 20;
        self.max_object_bytes
            .map(|limit| limit.saturating_mul(MAX_DECOMPRESSED_EXPANSION))
    }

    /// Decodes a fetched object into its messages: decrypt, decompress, then split and parse.
    /// A Parquet object yields its first chunk here; the rest is decoded as it is consumed.
    fn decode_records(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<DecodedObject> {
        let data = self.decode_object(key, data)?;
        #[cfg(feature = "parquet")]
        if self.format == FileFormat::Parquet {
            let decode = || -> anyhow::Result<DecodedObject> {
                let mut rest =
                    crate::support::parquet::RowStream::open(data, self.decompressed_limit())?;
                let records = rest.next_chunk().transpose()?.unwrap_or_default();
                Ok(DecodedObject {
                    records,
                    total: rest.rows(),
                    rows: Some(rest),
                })
            };
            return decode().with_context(|| format!("decode parquet object '{key}'"));
        }
        let records = split_and_parse(data, &self.delimiter, &self.format, &self.csv);
        Ok(DecodedObject {
            total: records.len(),
            records,
            #[cfg(feature = "parquet")]
            rows: None,
        })
    }

    /// Decrypt-then-decompress a fetched object whole (the write path compressed first).
    #[allow(unused_variables)]
    fn decode_object(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<bytes::Bytes> {
        #[cfg(feature = "encryption")]
        let data = if let Some(crypto) = &self.crypto {
            bytes::Bytes::from(
                crypto
                    .open(&data, b"")
                    .with_context(|| format!("decrypt object '{key}'"))?,
            )
        } else {
            data
        };
        #[cfg(feature = "compression")]
        let data = if self.compression != Compression::None && self.format != FileFormat::Parquet {
            bytes::Bytes::from(
                crate::support::compression::decompress_all(
                    self.compression,
                    &data,
                    self.decompressed_limit(),
                )
                .with_context(|| format!("decompress object '{key}'"))?,
            )
        } else {
            data
        };
        Ok(data)
    }

    /// Tops the buffer up to `max_messages` from the open Parquet object. Once it is
    /// exhausted, its unread rows no longer hold the cursor back.
    #[cfg(feature = "parquet")]
    async fn fill_from_rows(&mut self, max_messages: usize) -> Result<(), ConsumerError> {
        let Some(rows) = self.rows.as_mut() else {
            return Ok(());
        };
        let mut buffer = self.buffer.lock().await;
        let failure = loop {
            if buffer.len() >= max_messages {
                return Ok(());
            }
            match rows.next_chunk() {
                Some(Ok(chunk)) => buffer.extend(chunk),
                Some(Err(e)) => break Some(e),
                None => break None,
            }
        };
        drop(buffer);
        // Lock first: a cancel at this await must not drop the stream with its rows uncounted.
        let mut progress = self.progress.lock().await;
        let unread = rows.unread();
        self.rows = None;
        let Some(p) = progress.as_mut() else {
            return Ok(());
        };
        if let Some(e) = failure {
            if self.on_decode_error == DecodeErrorPolicy::Fail {
                return Err(ConsumerError::Permanent(anyhow!(
                    "parquet object '{}' cannot be decoded (on_decode_error: fail): {e:#}",
                    p.key
                )));
            }
            warn!(
                key = %p.key,
                error = %e,
                skipped_rows = unread,
                "parquet object failed to decode part way; skipping its remaining rows"
            );
        }
        p.remaining = p.remaining.saturating_sub(unread);
        if p.remaining == 0 {
            let key = p.key.clone();
            *progress = None;
            drop(progress);
            self.save_cursor(&key)
                .await
                .map_err(ConsumerError::Connection)?;
        }
        Ok(())
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
        loop {
            #[cfg(feature = "parquet")]
            self.fill_from_rows(max_messages).await?;
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
                    None if self.draining => {
                        // A cloud prefix may be empty; a local path that is not there is a typo.
                        if let Some(path) = self.local_path.as_ref().filter(|p| !p.exists()) {
                            return Err(ConsumerError::Permanent(anyhow!(
                                "object_store source path '{}' does not exist",
                                path.display()
                            )));
                        }
                        return Ok(empty_batch());
                    }
                    None => {
                        tokio::time::sleep(self.idle_delay).await;
                        return Ok(empty_batch());
                    }
                    Some((key, raw)) => {
                        // Decode here so a poison object is bounded-retried then
                        // quarantined (cursor advanced past it) rather than looping forever.
                        let decoded = match self.decode_records(&key, raw) {
                            Ok(decoded) => {
                                self.decode_failing_key = None;
                                self.decode_failures = 0;
                                self.decoded_any = true;
                                decoded
                            }
                            Err(e) => {
                                if self.decode_failing_key.as_deref() == Some(key.as_str()) {
                                    self.decode_failures += 1;
                                } else {
                                    self.decode_failing_key = Some(key.clone());
                                    self.decode_failures = 1;
                                }
                                if self.decode_failures >= MAX_OBJECT_DECODE_FAILURES {
                                    if self.on_decode_error == DecodeErrorPolicy::Fail {
                                        return Err(ConsumerError::Permanent(anyhow!(
                                            "object '{key}' cannot be decoded (on_decode_error: fail): {e:#}"
                                        )));
                                    }
                                    // Skipping here would walk the checkpoint past a whole prefix.
                                    if !self.decoded_any {
                                        return Err(ConsumerError::Permanent(anyhow!(
                                            "object '{key}' cannot be decoded and no object was decoded before it: check `format`, `compression` and `encryption`, or remove the object if it is damaged: {e:#}"
                                        )));
                                    }
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
                                    continue;
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
                                // A drain takes an empty batch as the end.
                                if self.draining {
                                    continue;
                                }
                                return Ok(empty_batch());
                            }
                        };
                        if decoded.total == 0 {
                            // No data records (e.g. a lone CSV header): advance past it so we
                            // don't re-list it forever, and go on with the next object.
                            self.save_cursor(&key)
                                .await
                                .map_err(ConsumerError::Connection)?;
                            continue;
                        }
                        *self.buffer.lock().await = decoded.records;
                        *self.progress.lock().await = Some(ObjProgress {
                            key,
                            remaining: decoded.total,
                        });
                        #[cfg(feature = "parquet")]
                        {
                            self.rows = decoded.rows;
                        }
                        continue;
                    }
                }
            } else if buffer_empty {
                // Commits for the current object are pending: wait for them. A drain takes
                // an empty batch as the end, so it must not see one here.
                let woken = tokio::time::timeout(self.idle_delay, self.settled.notified()).await;
                if woken.is_err() && !self.draining {
                    return Ok(empty_batch());
                }
                continue;
            }
            break;
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
        let mut unsettled = Unsettled {
            buffer: self.buffer.clone(),
            messages: batch.clone(),
            settled: self.settled.clone(),
        };

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // `unsettled` keeps what is not settled yet, so a cancelled commit gives it back.
                // The leading run of acks counts; the first nack and everything after it, and
                // any tail without a disposition, goes back to the front of the buffer.
                let leading_acks = dispositions
                    .iter()
                    .take_while(|d| !matches!(d, MessageDisposition::Nack))
                    .count()
                    .min(unsettled.messages.len());

                if leading_acks < unsettled.messages.len() {
                    let mut buf = buffer_arc.lock().await;
                    let mut requeue = unsettled.messages.split_off(leading_acks);
                    requeue.append(&mut buf);
                    *buf = requeue;
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
                                    // The object is listed and read again from the start.
                                    unsettled.take();
                                    return Err(anyhow!(e).context("persist object-store cursor"));
                                }
                            }
                            unsettled.take();
                            *prog = None;
                            drop(prog);
                            *last_key_arc.lock().await = Some(key);
                        }
                    }
                }
                unsettled.take();
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch {
            messages: batch,
            commit,
        })
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.draining = exit_on_empty;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests;
