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

use crate::checkpoint::{self, CheckpointBackend, CheckpointStore, VersionedCheckpoint};
use crate::endpoints::file::{
    csv_append_record, encode_record, parse_delimiter, parse_message, CsvDialect, CsvHeader,
    CsvQuoteState,
};
use crate::models::{
    Compression, DatePartitionStyle, DecodeErrorPolicy, FileFormat, NameBy, ObjectStoreConfig,
    ObjectStoreFsync,
};
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
use futures::{StreamExt, TryStreamExt};
use object_store::local::LocalFileSystem;
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
use crate::checkpoint::object_store_backend::{build_store, build_store_with_fsync};
use crate::support::local_staging::{remove_suffixed_leftovers, LocalStaging, StagedName};

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

/// Splits a fetched object into record slices on `delimiter`, dropping a trailing empty
/// remainder and a stray `\r` before a `\n` delimiter (mirrors the file reader). With
/// `csv`, a delimiter inside a quoted field is data, as it is for the file reader.
fn split_records<'a>(data: &'a [u8], delimiter: &[u8], csv: Option<&CsvDialect>) -> Vec<&'a [u8]> {
    let mut records = Vec::new();
    if delimiter.is_empty() {
        return records;
    }
    let newline = delimiter.len() == 1 && delimiter[0] == b'\n';
    // An `auto` separator is settled on the object's first line.
    let syntax = csv.map(|csv| {
        let first_line = memchr::memmem::find(data, delimiter).unwrap_or(data.len());
        csv.resolve(&data[..first_line])
    });
    let mut quotes = CsvQuoteState::new(syntax.unwrap_or_default());
    let mut scanned = 0;
    let mut start = 0;
    let mut i = 0;
    while i + delimiter.len() <= data.len() {
        if &data[i..i + delimiter.len()] == delimiter {
            if let Some(syntax) = syntax {
                quotes.feed(&data[scanned..i]);
                scanned = i;
                if quotes.in_quotes() {
                    i += 1;
                    continue;
                }
                quotes = CsvQuoteState::new(syntax);
                scanned = i + delimiter.len();
            }
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
fn split_and_parse(
    data: &[u8],
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
) -> Vec<CanonicalMessage> {
    let mut out = Vec::new();
    let csv = matches!(format, FileFormat::Csv).then_some(csv);
    let mut csv_header = csv.map(|csv| CsvHeader::unread(csv.clone()));
    for record in split_records(data, delimiter, csv) {
        if let Some(msg) = parse_message(record, format, &mut csv_header) {
            out.push(msg);
        }
    }
    out
}

/// One object's worth of encoded records: delimited bytes, or the rows of a Parquet body.
#[derive(Default)]
struct ObjectBody {
    bytes: Vec<u8>,
    rows: Vec<serde_json::Value>,
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
            self.rows
                .push(crate::support::parquet::parse_row(&msg.payload)?);
            return Ok(());
        }
        if *format == FileFormat::Csv {
            csv_append_record(
                &mut self.bytes,
                msg,
                &mut self.csv_columns,
                &mut self.csv_row,
                delimiter,
                csv,
            )?;
            return Ok(());
        }
        self.bytes
            .extend_from_slice(&encode_record(msg, format, delimiter)?);
        self.bytes.extend_from_slice(delimiter);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty() && self.rows.is_empty()
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
            warn!("object_store 'date_partition' does not apply to 'name_by: source_position'; part names carry the source range and are written flat under the prefix");
        }
        let staging = if staged {
            let extension = extension.clone();
            let root = LocalFileSystem::new().path_to_filesystem(&base)?;
            let scan = root.clone();
            let removed = tokio::task::spawn_blocking(move || {
                remove_suffixed_leftovers(&scan, |name| is_own_object_name(name, &extension))
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

    /// Writes one object, staged when the local store syncs in groups.
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
            return staging.write(target, body).await.map_err(|e| {
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
            crate::support::parquet::encode_rows(&body.rows, self.compression)
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
                        failed.push((message, PublisherError::NonRetryable(error)));
                    }
                }
            }
            if let Some(end) = segment_end {
                self.put_source_range(&run.source, segment_start, end, body)
                    .await?;
            }
        }
        Ok(SentBatch::from_failures(failed))
    }

    /// Writes one contiguous source range as a single object and records it as covered.
    async fn put_source_range(
        &self,
        source: &SourcePartition,
        start: u64,
        end: u64,
        body: ObjectBody,
    ) -> Result<(), PublisherError> {
        let name = finalized_name(source, start, end, &self.extension)
            .map_err(PublisherError::NonRetryable)?;
        let body = self.encode_object_body(body)?;
        let key = self.base.clone().join(name.as_str());
        self.write_object(&key, body, true).await?;
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
        let mut body = ObjectBody::default();
        let mut failed = Vec::new();
        for mut msg in messages {
            msg.strip_source_metadata();
            if let Err(e) = body.push(&msg, &self.format, &self.delimiter, &self.csv) {
                failed.push((msg, PublisherError::NonRetryable(e)));
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
        let records = split_and_parse(&data, &self.delimiter, &self.format, &self.csv);
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
            csv: CsvDialect::default(),
            #[cfg(feature = "compression")]
            compression: Compression::None,
            #[cfg(feature = "encryption")]
            crypto: None,
            date_partition: false,
            hive_partition: false,
            extension: "jsonl".to_string(),
            name_by: NameBy::WriteTime,
            covered_ranges: Arc::new(Mutex::new(CoveredRanges::default())),
            recovered: Arc::new(tokio::sync::OnceCell::new()),
            keys: Arc::new(StdMutex::new(SequentialGenerator::new())),
            staging: None,
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

    #[test]
    fn hive_style_names_the_date_folders() {
        let mut publisher = test_publisher(Arc::new(InMemory::new()));
        publisher.date_partition = true;
        publisher.hive_partition = true;
        let key = publisher.next_key().to_string();
        let parts: Vec<&str> = key.split('/').collect();
        assert_eq!(parts.len(), 5, "{key}");
        assert!(
            parts[1].starts_with("year=") && parts[1].len() == 9,
            "{key}"
        );
        assert!(
            parts[2].starts_with("month=") && parts[2].len() == 8,
            "{key}"
        );
        assert!(parts[3].starts_with("day=") && parts[3].len() == 6, "{key}");
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
            for record in split_records(&bytes, b"\n", None) {
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
        assert_eq!(split_records(&plain, b"\n", None).len(), 2);

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
        // Nothing decoded before it, so the setting is at fault: the route stops and the
        // checkpoint stays where it is.
        let error = fail_after_retries(&mut wrong).await;
        assert!(error.contains("`encryption`"), "{error}");
        assert!(wrong.last_key.lock().await.is_none());
    }

    /// Reads until the consumer gives up on the object it cannot decode.
    #[cfg(feature = "compression")]
    async fn fail_after_retries(consumer: &mut ObjectStoreConsumer) -> String {
        for _ in 0..MAX_OBJECT_DECODE_FAILURES {
            match consumer.receive_batch(10).await {
                Ok(batch) => assert!(batch.messages.is_empty()),
                Err(ConsumerError::Permanent(error)) => return format!("{error:#}"),
                Err(other) => panic!("expected a permanent error, got {other}"),
            }
        }
        panic!("the undecodable object did not stop the source");
    }

    #[cfg(feature = "compression")]
    async fn gzip_source_with(objects: &[(&str, Vec<u8>)]) -> ObjectStoreConsumer {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for (key, body) in objects {
            store
                .put(&ObjPath::from(*key), PutPayload::from(body.clone()))
                .await
                .unwrap();
        }
        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        consumer.compression = Compression::Gzip;
        consumer
    }

    #[cfg(feature = "compression")]
    fn gzipped(text: &str) -> Vec<u8> {
        crate::support::compression::compress_member(Compression::Gzip, text.as_bytes()).unwrap()
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn gzip_source_stops_when_its_first_object_is_no_gzip() {
        let mut consumer =
            gzip_source_with(&[("data/1-not-gzip.gz", br#"{"n":1}"#.to_vec())]).await;
        let error = fail_after_retries(&mut consumer).await;
        assert!(error.contains("`compression`"), "{error}");
        assert!(consumer.last_key.lock().await.is_none());
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn gzip_source_skips_a_bad_object_after_a_good_one() {
        let mut consumer = gzip_source_with(&[
            ("data/1-good.gz", gzipped("one")),
            ("data/2-not-gzip.gz", b"plain".to_vec()),
            ("data/3-good.gz", gzipped("three")),
        ])
        .await;
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let batch = consumer.receive_batch(10).await.unwrap();
            let acks = vec![MessageDisposition::Ack; batch.messages.len()];
            seen.extend(
                batch
                    .messages
                    .iter()
                    .map(|m| m.get_payload_str().to_string()),
            );
            (batch.commit)(acks).await.unwrap();
        }
        assert_eq!(seen, ["one", "three"]);
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn gzip_source_with_fail_policy_stops_at_a_bad_object_after_a_good_one() {
        let mut consumer = gzip_source_with(&[
            ("data/1-good.gz", gzipped("one")),
            ("data/2-not-gzip.gz", b"plain".to_vec()),
        ])
        .await;
        consumer.on_decode_error = DecodeErrorPolicy::Fail;
        let batch = consumer.receive_batch(10).await.unwrap();
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        let error = fail_after_retries(&mut consumer).await;
        assert!(error.contains("on_decode_error: fail"), "{error}");
        assert_eq!(
            consumer.last_key.lock().await.as_deref(),
            Some("data/1-good.gz")
        );
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
    async fn local_sink_uses_the_configured_fsync_interval() {
        let dir = tempfile::tempdir().unwrap();
        let publisher = ObjectStorePublisher::new(&ObjectStoreConfig {
            url: format!("file://{}", dir.path().display()),
            fsync_interval_ms: Some(25),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            publisher.deferred_commit_interval(),
            Some(Duration::from_millis(25))
        );
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
        assert_eq!(
            publisher.deferred_commit_interval(),
            Some(FINALIZE_INTERVAL)
        );
        let mut early = ObjectStoreConsumer::new(&config).await.unwrap();
        assert!(early.receive_batch(10).await.unwrap().messages.is_empty());
        publisher.flush().await.unwrap();

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"source":"local"}"#);
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        let drained = consumer.receive_batch(10).await.unwrap();
        assert!(drained.messages.is_empty());
    }

    #[tokio::test]
    async fn csv_sink_writes_a_header_into_every_object() {
        let dir = tempfile::tempdir().unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}", dir.path().display()),
            format: FileFormat::Csv,
            csv: crate::models::CsvConfig {
                separator: Some(";".to_string()),
                ..Default::default()
            },
            date_partition: Some(false),
            polling_interval_ms: Some(1),
            ..Default::default()
        };

        let publisher = ObjectStorePublisher::new(&config).await.unwrap();
        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::new(br#"{"id":1,"stats":{"n":2,"avg":1.5}}"#.to_vec(), None),
                CanonicalMessage::new(b"not json".to_vec(), None),
                CanonicalMessage::new(br#"{"id":2,"stats":{"n":3,"avg":"a;b"}}"#.to_vec(), None),
            ])
            .await
            .unwrap();
        assert!(matches!(sent, SentBatch::Partial { ref failed, .. } if failed.len() == 1));
        publisher
            .send_batch(vec![CanonicalMessage::new(
                br#"{"other":"x"}"#.to_vec(),
                None,
            )])
            .await
            .unwrap();
        publisher.flush().await.unwrap();

        let mut objects: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect();
        objects.sort();
        assert_eq!(
            objects,
            ["id;stats.n;stats.avg\n1;2;1.5\n2;3;\"a;b\"\n", "other\nx\n"]
        );

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let mut rows = Vec::new();
        for _ in 0..2 {
            let batch = consumer.receive_batch(10).await.unwrap();
            rows.extend(batch.messages.iter().map(|m| m.payload.to_vec()));
            let acks = vec![MessageDisposition::Ack; batch.messages.len()];
            (batch.commit)(acks).await.unwrap();
        }
        rows.sort();
        assert_eq!(
            rows,
            [
                br#"{"id":"1","stats.n":"2","stats.avg":"1.5"}"#.to_vec(),
                br#"{"id":"2","stats.n":"3","stats.avg":"a;b"}"#.to_vec(),
                br#"{"other":"x"}"#.to_vec(),
            ]
        );
    }

    #[tokio::test]
    async fn csv_sink_cannot_guess_its_separator() {
        let config = ObjectStoreConfig {
            url: "memory:///data".to_string(),
            format: FileFormat::Csv,
            csv: crate::models::CsvConfig {
                separator: Some("auto".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(ObjectStorePublisher::new(&config).await.is_err());
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

    /// RFC 4180 lets a quoted field carry the record separator; the object splitter must
    /// frame records the way the file reader does, CRLF included.
    #[tokio::test]
    async fn csv_drop_file_keeps_quoted_newlines_in_one_record() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("notes.csv"),
            b"id,note\r\n1,\"Line1\r\nLine2\"\r\n2,\"a,\"\"b\"\"\"\r\n",
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
        let rows: Vec<serde_json::Value> = batch
            .messages
            .iter()
            .map(|m| serde_json::from_slice(&m.payload).unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                serde_json::json!({"id": "1", "note": "Line1\r\nLine2"}),
                serde_json::json!({"id": "2", "note": "a,\"b\""}),
            ]
        );
        (batch.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .unwrap();
    }

    #[test]
    fn split_records_is_quote_aware_only_for_csv() {
        let data = b"a,\"x\ny\"\nb\n";
        assert_eq!(
            split_records(data, b"\n", Some(&CsvDialect::default())),
            vec![&b"a,\"x\ny\""[..], b"b"]
        );
        assert_eq!(
            split_records(data, b"\n", None),
            vec![&b"a,\"x"[..], b"y\"", b"b"]
        );
    }

    #[tokio::test]
    async fn a_url_naming_one_object_reads_only_that_object() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"{\"n\":1}\n").unwrap();
        std::fs::write(dir.path().join("a.jsonl.bak"), b"{\"n\":2}\n").unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}/a.jsonl", dir.path().display()),
            format: FileFormat::Json,
            polling_interval_ms: Some(1),
            ..Default::default()
        };

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert!(consumer
            .receive_batch(10)
            .await
            .unwrap()
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn a_url_naming_one_object_reads_it_once_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}/a.jsonl", dir.path().display()),
            format: FileFormat::Json,
            polling_interval_ms: Some(1),
            ..Default::default()
        };

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        assert!(consumer
            .receive_batch(10)
            .await
            .unwrap()
            .messages
            .is_empty());
        std::fs::write(dir.path().join("a.jsonl"), b"{\"n\":1}\n").unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
    }

    #[tokio::test]
    async fn an_exact_key_written_after_a_child_is_still_read() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut consumer = ObjectStoreConsumer::from_store(
            store.clone(),
            ObjPath::from("a"),
            FileFormat::Json,
            None,
            None,
        );
        consumer.single_object = None;
        store
            .put(&ObjPath::from("a/child"), b"{\"n\":2}\n".to_vec().into())
            .await
            .unwrap();
        store
            .put(&ObjPath::from("a"), b"{\"n\":1}\n".to_vec().into())
            .await
            .unwrap();

        let mut payloads = Vec::new();
        for _ in 0..2 {
            let batch = consumer.receive_batch(10).await.unwrap();
            payloads.extend(batch.messages.iter().map(|m| m.payload.to_vec()));
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
        assert_eq!(
            payloads,
            vec![br#"{"n":1}"#.to_vec(), br#"{"n":2}"#.to_vec()]
        );
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

    #[test]
    fn parquet_extension_carries_no_compression_suffix() {
        assert_eq!(
            extension_for(&FileFormat::Parquet, Compression::Zstd, false),
            "parquet"
        );
        assert_eq!(
            extension_for(&FileFormat::Parquet, Compression::Gzip, true),
            "parquet.enc"
        );
    }

    #[cfg(feature = "parquet")]
    async fn object_bytes(store: &dyn ObjectStore, key: &str) -> Vec<u8> {
        let key = ObjPath::from(key);
        store
            .get(&key)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec()
    }

    #[cfg(feature = "parquet")]
    #[tokio::test]
    async fn parquet_round_trips_typed_rows_for_every_codec() {
        let rows = [
            serde_json::json!({"id": 1, "name": "alice", "score": 1.5, "active": true}),
            serde_json::json!({"id": 2, "name": "bob", "score": 2.25, "active": false}),
            serde_json::json!({"id": 3, "name": "carol", "score": 3.0, "active": true}),
        ];
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Lz4,
            Compression::Zstd,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = ObjectStoreConfig {
                url: format!("file://{}", dir.path().display()),
                format: FileFormat::Parquet,
                compression,
                date_partition: Some(false),
                polling_interval_ms: Some(1),
                ..Default::default()
            };
            let publisher = ObjectStorePublisher::new(&config).await.unwrap();
            publisher
                .send_batch(rows.iter().cloned().map(json_msg).collect())
                .await
                .unwrap();
            publisher.flush().await.unwrap();

            let files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].extension().unwrap(), "parquet");
            let bytes = std::fs::read(&files[0]).unwrap();
            assert!(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"));

            let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
            let batch = consumer.receive_batch(10).await.unwrap();
            let read: Vec<serde_json::Value> = batch
                .messages
                .iter()
                .map(|m| serde_json::from_slice(&m.payload).unwrap())
                .collect();
            assert_eq!(read, rows, "{compression:?}");
            assert_eq!(
                batch.messages[0]
                    .metadata
                    .get("mq_bridge.original_format")
                    .map(String::as_str),
                Some("parquet")
            );
            (batch.commit)(vec![MessageDisposition::Ack; 3])
                .await
                .unwrap();
        }
    }

    #[cfg(feature = "parquet")]
    #[tokio::test]
    async fn parquet_object_is_decoded_as_it_is_consumed() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let rows: Vec<_> = (0..200).map(|i| serde_json::json!({"id": i})).collect();
        let body = crate::support::parquet::encode_rows(&rows, Compression::None).unwrap();
        store
            .put(&ObjPath::from("data/a.parquet"), PutPayload::from(body))
            .await
            .unwrap();
        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Parquet,
            None,
            None,
        );
        let mut ids = Vec::new();
        while ids.len() < rows.len() {
            let batch = consumer.receive_batch(50).await.unwrap();
            assert!(!batch.messages.is_empty() && batch.messages.len() <= 50);
            assert!(consumer.buffer.lock().await.len() < 64, "decoded ahead");
            assert_eq!(*consumer.last_key.lock().await, None, "cursor moved early");
            for message in &batch.messages {
                let row: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
                ids.push(row["id"].as_i64().unwrap());
            }
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
        assert_eq!(ids, (0..200).collect::<Vec<_>>());
        assert_eq!(
            consumer.last_key.lock().await.as_deref(),
            Some("data/a.parquet")
        );
    }

    #[cfg(feature = "parquet")]
    #[tokio::test]
    async fn parquet_object_failing_part_way_follows_on_decode_error() {
        let pad = "x".repeat(20_000);
        let rows: Vec<_> = (0..128)
            .map(|i| serde_json::json!({"id": i, "pad": if i < 64 { "" } else { pad.as_str() }}))
            .collect();
        let body = crate::support::parquet::encode_rows(&rows, Compression::Zstd).unwrap();
        // The second chunk decodes past the per-chunk bound, the first does not.
        let max_object_bytes = body.len() as u64;
        assert!(max_object_bytes * 20 < 64 * 20_000);
        for policy in [DecodeErrorPolicy::Skip, DecodeErrorPolicy::Fail] {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            store
                .put(
                    &ObjPath::from("data/a.parquet"),
                    PutPayload::from(body.clone()),
                )
                .await
                .unwrap();
            let mut consumer = ObjectStoreConsumer::from_store(
                store,
                ObjPath::from("data"),
                FileFormat::Parquet,
                None,
                None,
            );
            consumer.max_object_bytes = Some(max_object_bytes);
            consumer.on_decode_error = policy;

            let first = consumer.receive_batch(64).await.unwrap();
            assert_eq!(first.messages.len(), 64);
            (first.commit)(vec![MessageDisposition::Ack; 64])
                .await
                .unwrap();
            let next = consumer.receive_batch(64).await;
            let last_key = consumer.last_key.lock().await.clone();
            match policy {
                DecodeErrorPolicy::Skip => {
                    assert!(next.unwrap().messages.is_empty());
                    assert_eq!(last_key.as_deref(), Some("data/a.parquet"));
                }
                DecodeErrorPolicy::Fail => {
                    assert!(matches!(next, Err(ConsumerError::Permanent(_))));
                    assert_eq!(last_key, None);
                }
            }
        }
    }

    #[cfg(feature = "parquet")]
    #[tokio::test]
    async fn parquet_sink_fails_only_non_object_rows() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut publisher = test_publisher(store.clone());
        publisher.format = FileFormat::Parquet;
        publisher.extension = "parquet".to_string();
        let sent = publisher
            .send_batch(vec![
                json_msg(serde_json::json!({"n": 1})),
                json_msg(serde_json::json!([1, 2])),
                CanonicalMessage::new(b"not json".to_vec(), None),
                json_msg(serde_json::json!({"n": 2})),
            ])
            .await
            .unwrap();
        let SentBatch::Partial { failed, .. } = sent else {
            panic!("expected a partial send");
        };
        assert_eq!(failed.len(), 2);

        let keys = keys(store.as_ref()).await;
        assert_eq!(keys.len(), 1);
        let rows = crate::support::parquet::decode_rows(
            object_bytes(store.as_ref(), &keys[0]).await,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[cfg(feature = "parquet")]
    #[tokio::test]
    async fn parquet_parts_by_source_position_split_at_a_bad_row() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut publisher = test_publisher(store.clone());
        publisher.format = FileFormat::Parquet;
        publisher.compression = Compression::Zstd;
        publisher.extension = extension_for(&FileFormat::Parquet, Compression::Zstd, false);
        publisher.name_by = NameBy::SourcePosition;
        let mut bad = kafka_message(1);
        bad.payload = b"\"a string\"".to_vec().into();
        let sent = publisher
            .send_batch(vec![kafka_message(0), bad, kafka_message(2)])
            .await
            .unwrap();
        assert!(matches!(sent, SentBatch::Partial { ref failed, .. } if failed.len() == 1));

        assert_eq!(
            keys(store.as_ref()).await,
            vec![
                part_key(0, 0, "parquet").to_string(),
                part_key(2, 2, "parquet").to_string(),
            ]
        );

        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Parquet,
            None,
            None,
        );
        let mut offsets = Vec::new();
        for _ in 0..2 {
            let batch = consumer.receive_batch(10).await.unwrap();
            for message in &batch.messages {
                let row: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
                offsets.push(row["offset"].as_i64().unwrap());
            }
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
        assert_eq!(offsets, vec![0, 2]);
    }

    /// The dialect reaches the splitter and the row decoder alike: the quoted line break
    /// stays in one record, and the separator is guessed from the object's first line.
    #[test]
    fn csv_objects_follow_the_configured_dialect() {
        let config = crate::models::CsvConfig {
            separator: Some("auto".to_string()),
            ..Default::default()
        };
        let csv = CsvDialect::from_config(&config, b"\n").unwrap();
        let object = "\u{feff}id;note\r\n1;\"a\nb;c\"\r\n2;x,y\r\n";
        let rows: Vec<serde_json::Value> =
            split_and_parse(object.as_bytes(), b"\n", &FileFormat::Csv, &csv)
                .iter()
                .map(|msg| serde_json::from_slice(&msg.payload).unwrap())
                .collect();
        assert_eq!(
            rows,
            vec![
                serde_json::json!({"id": "1", "note": "a\nb;c"}),
                serde_json::json!({"id": "2", "note": "x,y"}),
            ]
        );
    }

    /// Reads `objects` one-record files through a draining route and returns what arrived.
    async fn drain_through_a_route(objects: usize, empty_object: bool) -> usize {
        use crate::models::{Endpoint, EndpointType};
        let temp = tempfile::tempdir().unwrap();
        // `~` as in a Windows short path: a key has to come back from the listing unchanged.
        let dir = temp.path().join("drain~1");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..objects {
            std::fs::write(dir.join(format!("part-{i}.jsonl")), format!("{i}\n")).unwrap();
        }
        if empty_object {
            std::fs::write(dir.join("part-2x.jsonl"), "").unwrap();
        }
        let config: ObjectStoreConfig = serde_json::from_value(serde_json::json!({
            "url": url::Url::from_directory_path(&dir).unwrap().to_string(),
            "format": "raw",
        }))
        .unwrap();
        let topic = format!("object_drain_{}", fast_uuid_v7::gen_id());
        let route = crate::Route::new(
            Endpoint::new(EndpointType::ObjectStore(config)),
            Endpoint::new_memory(&topic, 100),
        )
        .with_exit_on_empty(true);
        let mut output = route.connect_to_output("object_drain").await.unwrap();
        let handle = route.run(&topic).await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the drain ends");
        handle.join().await.unwrap();
        let mut received = 0;
        while let Ok(Ok(item)) =
            tokio::time::timeout(Duration::from_millis(300), output.receive()).await
        {
            (item.commit)(MessageDisposition::Ack).await.unwrap();
            received += 1;
        }
        received
    }

    #[tokio::test]
    async fn a_batch_dropped_without_a_commit_is_read_again() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(&ObjPath::from("data/a.jsonl"), b"1\n2\n".to_vec().into())
            .await
            .unwrap();
        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        drop(consumer.receive_batch(10).await.unwrap());
        let again = tokio::time::timeout(Duration::from_secs(2), consumer.receive_batch(10))
            .await
            .expect("the object is not stuck in flight")
            .unwrap();
        assert_eq!(again.messages.len(), 2);
    }

    #[tokio::test]
    async fn a_drain_reads_every_object() {
        assert_eq!(drain_through_a_route(6, false).await, 6);
    }

    #[tokio::test]
    async fn an_object_without_records_does_not_end_a_drain() {
        assert_eq!(drain_through_a_route(6, true).await, 6);
    }
}
