//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::event_store::{EventStore, EventStoreConsumer, RetentionPolicy};
use crate::models::{Compression, FileConfig, FileConsumerMode, FileFormat, FileFsync, NameBy};
#[cfg(feature = "encryption")]
use crate::support::crypto::Crypto;
use crate::support::local_staging::{LocalStaging, StagedName};
use crate::support::source_ranges::{finalized_name, CoveredRanges};
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::io::Seek;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{self, AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{info, instrument, trace, warn};

use crate::codec::csv::{csv_append_record, parse_delimiter, CsvHeader};
use crate::codec::format::{
    decode_records, encode_record, parse_message, strip_leading_bom, RecordSpan,
};
use crate::codec::framing::record_quotes;
pub(crate) use crate::codec::CsvDialect;

/// A sink that writes messages to a file, one per line.
static FILE_LOCKS: Lazy<StdMutex<HashMap<String, Arc<Mutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

/// One key per file however the path is spelled; the file itself may not exist yet.
fn file_lock_key(path: &str) -> String {
    let p = Path::new(path);
    let resolved = std::fs::canonicalize(p).ok().or_else(|| {
        let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
        let parent = std::fs::canonicalize(parent.unwrap_or(Path::new("."))).ok()?;
        Some(parent.join(p.file_name()?))
    });
    match resolved {
        Some(resolved) => resolved.to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

fn get_file_lock(path: &str) -> Arc<Mutex<()>> {
    let mut locks = FILE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, v| Arc::strong_count(v) > 1);
    locks
        .entry(file_lock_key(path))
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Reads one *record*, which for CSV may span several delimiters. Every read loop and
/// every record count goes through this, so a multi-line row stays one record everywhere
/// — including the `lines_in_memory` bookkeeping that consume mode truncates the file by.
async fn read_record<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let start = buf.len();
    let mut total = read_until_bytes(reader, delimiter, buf).await?;
    let Some(mut quotes) = record_quotes(format, csv, &buf[start..]) else {
        return Ok(total);
    };
    while total > 0 && quotes.in_quotes() {
        let from = buf.len();
        let n = read_until_bytes(reader, delimiter, buf).await?;
        if n == 0 {
            break; // Unterminated quote at EOF: emit what there is rather than hang.
        }
        quotes.feed(&buf[from..]);
        total += n;
    }
    Ok(total)
}

async fn read_until_bytes<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    delimiter: &[u8],
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    if delimiter.len() == 1 {
        return reader.read_until(delimiter[0], buf).await;
    }
    let last_byte = delimiter[delimiter.len() - 1];
    let mut total_read = 0;
    loop {
        let n = reader.read_until(last_byte, buf).await?;
        if n == 0 {
            return Ok(total_read);
        }
        total_read += n;
        if buf.len() >= delimiter.len() && &buf[buf.len() - delimiter.len()..] == delimiter {
            return Ok(total_read);
        }
    }
}

#[derive(Clone)]
pub struct FilePublisher {
    path: String,
    file_lock: Arc<Mutex<()>>,
    delimiter: Vec<u8>,
    format: FileFormat,
    csv: CsvDialect,
    name_by: NameBy,
    part_extension: String,
    covered_ranges: Arc<Mutex<CoveredRanges>>,
    #[cfg(any(feature = "compression", feature = "encryption"))]
    compression: Compression,
    #[cfg(feature = "encryption")]
    crypto: Option<Arc<Crypto>>,
    /// CSV column order, locked in by the first message written. Shared across
    /// clones of this publisher so all writers to the same file agree on it.
    csv_header: Arc<Mutex<Option<Vec<String>>>>,
    fsync: FsyncPolicy,
    /// Part files waiting for a grouped sync; `None` under `fsync: batch`, which syncs each part.
    part_staging: Option<Arc<LocalStaging>>,
}

/// How the appending sink syncs; part files under `source_position` always sync.
#[derive(Clone)]
enum FsyncPolicy {
    Off,
    Batch,
    Periodic(Arc<PeriodicSync>),
}

/// Unsynced-write state shared by a `periodic` sink and its background sync.
struct PeriodicSync {
    /// Set after a write, cleared by the sync that covers it.
    dirty: AtomicBool,
    /// Held across a sync, so `flush` waits for one already running.
    syncing: tokio::sync::Mutex<()>,
    /// The deferred-commit interval; `None` for part files, which sync on their own.
    interval: Option<Duration>,
}

impl PeriodicSync {
    async fn sync(&self, path: &str) -> std::io::Result<()> {
        let _syncing = self.syncing.lock().await;
        if self.dirty.swap(false, Ordering::AcqRel) {
            if let Err(e) = sync_path(path).await {
                self.dirty.store(true, Ordering::Release);
                return Err(e);
            }
        }
        Ok(())
    }
}

async fn sync_path(path: &str) -> std::io::Result<()> {
    let file = OpenOptions::new().append(true).open(path).await?;
    file.sync_data().await
}

/// Syncs `path` every `interval` while it has unsynced writes; ends with the last publisher.
fn spawn_periodic_sync(path: String, interval: Duration, state: Weak<PeriodicSync>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(state) = state.upgrade() else { break };
            if let Err(e) = state.sync(&path).await {
                warn!(path = %path, error = %e, "Periodic fsync of file sink failed");
            }
        }
    });
}

/// Validates the `format`/`compression`/`encryption` settings shared by the file
/// publisher and consumer: both need their Cargo feature enabled.
fn validate_member_settings(config: &FileConfig) -> anyhow::Result<()> {
    if config.format == FileFormat::Parquet {
        return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
            "file 'format: parquet' is not supported (a Parquet file can't be appended to); parquet is only supported by object_store"
        ))
        .into());
    }
    #[cfg(not(feature = "compression"))]
    if config.compression != Compression::None {
        return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
            "file 'compression' requires the `compression` feature"
        ))
        .into());
    }
    #[cfg(not(feature = "encryption"))]
    if config.encryption.is_some() {
        return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
            "file 'encryption' requires the `encryption` feature"
        ))
        .into());
    }
    Ok(())
}

/// Staging files younger than this may belong to a concurrent writer — another worker or an
/// overlapping restart sharing the directory — so only older ones are treated as crash debris.
const STAGING_REAP_AGE: Duration = Duration::from_secs(60);

async fn recover_finalized_file_ranges(
    directory: &Path,
    extension: &str,
) -> anyhow::Result<CoveredRanges> {
    let now = SystemTime::now();
    let mut entries = fs::read_dir(directory).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".stage-") {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age >= STAGING_REAP_AGE {
                match fs::remove_file(entry.path()).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            names.push(name);
        }
    }
    Ok(CoveredRanges::from_finalized_names(
        names.iter().map(String::as_str),
        extension,
    ))
}

async fn write_finalized_file(path: &Path, body: &[u8]) -> Result<(), PublisherError> {
    let directory = path
        .parent()
        .ok_or_else(|| PublisherError::NonRetryable(anyhow::anyhow!("missing output directory")))?;
    let staging = directory.join(format!(".stage-{}", fast_uuid_v7::gen_id_str()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staging)
        .await
        .context("Failed to create idempotent file staging output")?;
    file.write_all(body)
        .await
        .context("Failed to write idempotent file staging output")?;
    file.sync_all()
        .await
        .context("Failed to sync idempotent file staging output")?;
    drop(file);
    fs::rename(&staging, path)
        .await
        .context("Failed to finalize idempotent file output")?;
    Ok(())
}

/// fsyncs `dir` so a new entry in it survives power loss. Windows cannot open a directory
/// handle this way, and some network or FUSE mounts cannot sync one; both are skipped.
async fn sync_directory(dir: &Path) -> std::io::Result<()> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let handle = match File::open(dir).await {
        Ok(handle) => handle,
        Err(_) if cfg!(not(unix)) => return Ok(()),
        Err(error) => return Err(error),
    };
    match handle.sync_all().await {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

/// `create_dir_all` that also syncs each directory it created into its parent.
async fn create_dir_all_durable(dir: &Path) -> std::io::Result<()> {
    let mut created = Vec::new();
    let mut next = Some(dir);
    while let Some(d) = next.filter(|d| !d.as_os_str().is_empty()) {
        if fs::try_exists(d).await? {
            break;
        }
        created.push(d);
        next = d.parent();
    }
    fs::create_dir_all(dir).await?;
    for d in created {
        if let Some(parent) = d.parent() {
            sync_directory(parent).await?;
        }
    }
    Ok(())
}

impl FilePublisher {
    /// Columns of the header an existing CSV file already has, so appended rows line up
    /// with it. `None` when it has none. A header that cannot be read fails the write:
    /// guessing the columns from the first payload could append rows that do not match it.
    async fn existing_csv_header(&self) -> Result<Option<Vec<String>>, PublisherError> {
        let this = self.clone();
        let read = tokio::task::spawn_blocking(move || this.read_csv_header_sync()).await;
        let error: anyhow::Error = match read {
            Ok(Ok(columns)) => return Ok(columns),
            Ok(Err(e)) => e.into(),
            Err(e) => e.into(),
        };
        Err(PublisherError::Retryable(error.context(format!(
            "Could not read the CSV header of '{}'",
            self.path
        ))))
    }

    fn read_csv_header_sync(&self) -> std::io::Result<Option<Vec<String>>> {
        let file = std::fs::File::open(&self.path)?;
        let mut reader = std::io::BufReader::new(self.decoded_reader(file));
        let mut buf = Vec::new();
        while buf.is_empty() {
            if read_record_sync(
                &mut reader,
                &self.delimiter,
                &FileFormat::Csv,
                &self.csv,
                &mut buf,
            )? == 0
            {
                return Ok(None);
            }
            if buf.ends_with(&self.delimiter) {
                buf.truncate(buf.len() - self.delimiter.len());
            }
            if self.delimiter == b"\n" && buf.ends_with(b"\r") {
                buf.pop();
            }
        }
        let (header, _) = CsvHeader::establish(&self.csv, String::from_utf8_lossy(&buf).as_bytes());
        Ok(Some(header.column_names()))
    }

    /// The file's plaintext, undoing whatever compression/encryption the sink applies.
    fn decoded_reader(&self, file: std::fs::File) -> Box<dyn std::io::Read> {
        #[cfg(feature = "encryption")]
        if let Some(crypto) = &self.crypto {
            return Box::new(EncryptedFramesReader::new(
                std::io::BufReader::new(file),
                crypto.clone(),
                self.compression,
            ));
        }
        #[cfg(feature = "compression")]
        if self.compression != Compression::None {
            return crate::support::compression::decompress_reader(
                self.compression,
                std::io::BufReader::new(file),
            );
        }
        Box::new(file)
    }

    /// Opens the sink with the naming scheme the config alone implies. Without a route there
    /// is no input to resolve `auto` against, so it falls back to `write_time`.
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        Self::new_with_name_by(config, config.resolved_name_by(false)).await
    }

    pub async fn new_with_name_by(config: &FileConfig, name_by: NameBy) -> anyhow::Result<Self> {
        validate_member_settings(config)?;
        let path_str = &config.path;
        let path = Path::new(path_str);
        let by_source_position = name_by == NameBy::SourcePosition;
        if by_source_position {
            if matches!(config.format, FileFormat::Csv) {
                return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                    "file 'name_by: source_position' does not support CSV (per-part headers are unimplemented)"
                ))
                .into());
            }
            create_dir_all_durable(path).await.with_context(|| {
                format!("Failed to create part-file sink directory: {path_str}")
            })?;
        }
        if let Some(parent) = path.parent() {
            create_dir_all_durable(parent).await.with_context(|| {
                format!("Failed to create parent directory for file: {:?}", parent)
            })?;
        }

        if !by_source_position {
            let _ = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("Failed to open or create file for writing: {path_str}: {e}")
                })?;
        }

        let file_lock = get_file_lock(path_str);
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let csv = CsvDialect::for_format(&config.format, &config.csv, &delimiter)?;
        csv.check_sink()?;
        let format = config.format.clone();
        // Part names must advertise what the bytes actually are: a compressed or sealed part
        // holds one member, so it earns the same suffixes the appending sink's file would.
        let mut part_extension = match format {
            FileFormat::Csv => "csv",
            FileFormat::Raw => "bin",
            FileFormat::Normal | FileFormat::Json | FileFormat::Text => "jsonl",
            FileFormat::Parquet => "parquet",
        }
        .to_string();
        match config.compression {
            Compression::None => {}
            Compression::Gzip => part_extension.push_str(".gz"),
            Compression::Lz4 => part_extension.push_str(".lz4"),
            Compression::Zstd => part_extension.push_str(".zst"),
        }
        if config.encryption.is_some() {
            part_extension.push_str(".enc");
        }
        let covered_ranges = if by_source_position {
            // A local directory scan, unlike the object store's networked LIST.
            recover_finalized_file_ranges(path, &part_extension).await?
        } else {
            CoveredRanges::default()
        };
        if config.fsync == FileFsync::Off && !by_source_position {
            info!(path = %config.path, "File sink does not fsync; an OS crash or power loss can lose acknowledged batches. Set `fsync: periodic` to prevent that");
        }
        let fsync = match config.fsync {
            FileFsync::Off => FsyncPolicy::Off,
            FileFsync::Batch => FsyncPolicy::Batch,
            FileFsync::Periodic => {
                let interval = Duration::from_millis(config.fsync_interval_ms.unwrap_or(1000));
                if interval.is_zero() {
                    return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                        "file 'fsync_interval_ms' must be greater than 0; use 'fsync: batch' to sync every batch"
                    ))
                    .into());
                }
                let state = Arc::new(PeriodicSync {
                    dirty: AtomicBool::new(false),
                    syncing: tokio::sync::Mutex::new(()),
                    interval: (!by_source_position).then_some(interval),
                });
                if !by_source_position {
                    spawn_periodic_sync(path_str.to_string(), interval, Arc::downgrade(&state));
                }
                FsyncPolicy::Periodic(state)
            }
        };
        let part_staging = (by_source_position && config.fsync != FileFsync::Batch).then(|| {
            let interval = config.fsync_interval_ms.unwrap_or(1000).max(1);
            LocalStaging::new(
                path.to_path_buf(),
                StagedName::Hidden,
                Duration::from_millis(interval),
            )
        });
        info!(path = %path_str, format = ?format, "File sink opened for appending");
        Ok(Self {
            fsync,
            part_staging,
            path: path_str.to_string(),
            file_lock,
            delimiter,
            format,
            name_by,
            part_extension,
            covered_ranges: Arc::new(Mutex::new(covered_ranges)),
            #[cfg(any(feature = "compression", feature = "encryption"))]
            compression: config.compression,
            #[cfg(feature = "encryption")]
            crypto: config
                .encryption
                .as_ref()
                .map(Crypto::new_at_rest)
                .transpose()?
                .map(Arc::new),
            csv_header: Arc::new(Mutex::new(None)),
            csv,
        })
    }

    /// Applies the `fsync` setting to a batch that was just written and flushed.
    /// `was_empty` also syncs the directory, so a newly created file's entry survives power loss.
    async fn sync_written(&self, file: &File, was_empty: bool) -> std::io::Result<()> {
        match &self.fsync {
            FsyncPolicy::Off => return Ok(()),
            FsyncPolicy::Batch => file.sync_data().await?,
            FsyncPolicy::Periodic(state) => state.dirty.store(true, Ordering::Release),
        }
        if was_empty {
            if let Some(parent) = Path::new(&self.path).parent() {
                sync_directory(parent).await?;
            }
        }
        Ok(())
    }

    /// Unlike the `object_store` sink, an encode failure here fails the whole batch rather than
    /// splitting the run around the bad record. That is deliberate: `encode_record` is fallible in
    /// signature only for the formats this path accepts (`normal`, `json`, `text`, `raw`; CSV is
    /// rejected at open), so the split would be untestable code guarding a case that cannot occur.
    /// If a fallible format ever lands, mirror `ObjectStorePublisher::send_batch_by_source_position`.
    async fn send_batch_by_source_position(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let _file_guard = self.file_lock.lock().await;
        let mut covered = self.covered_ranges.lock().await;
        let runs = covered
            .uncovered_runs(messages)
            .map_err(PublisherError::NonRetryable)?;

        for run in runs {
            let name = finalized_name(&run.source, run.start, run.end, &self.part_extension)
                .map_err(PublisherError::NonRetryable)?;
            let final_path = Path::new(&self.path).join(&name);
            if !fs::try_exists(&final_path)
                .await
                .context("Failed to check part-file sink output")?
            {
                let mut body = Vec::new();
                for mut message in run.messages {
                    message.strip_source_metadata();
                    let bytes = encode_record(&message, &self.format, &self.delimiter)
                        .map_err(|error| PublisherError::NonRetryable(anyhow::anyhow!(error)))?;
                    body.extend_from_slice(&bytes);
                    body.extend_from_slice(&self.delimiter);
                }
                let body = self.encode_member(body)?;
                if let Some(staging) = &self.part_staging {
                    staging.write(final_path, body).await.map_err(|e| {
                        PublisherError::Retryable(
                            anyhow::Error::new(e).context("Failed to stage part-file sink output"),
                        )
                    })?;
                } else {
                    write_finalized_file(&final_path, &body).await?;
                    sync_directory(Path::new(&self.path))
                        .await
                        .context("Failed to sync part-file sink directory")?;
                }
            } else {
                // A retry after a failed directory sync lands here.
                sync_directory(Path::new(&self.path))
                    .await
                    .context("Failed to sync part-file sink directory")?;
            }
            covered
                .insert(run.source, run.start, run.end)
                .map_err(PublisherError::NonRetryable)?;
        }
        Ok(SentBatch::Ack)
    }

    /// Turns a fully built batch body into one self-contained member: compress, then seal into
    /// a `[u64 be length][sealed bytes]` frame. Compressed members self-delimit; a sealed one
    /// does not, hence the frame. Returns the body untouched when neither is configured.
    ///
    /// The appending path concatenates these into one file and the idempotent path writes each
    /// as its own part file, but the encoding is identical either way, so the same reader
    /// handles both and neither adds a format of its own.
    #[allow(unused_mut)]
    fn encode_member(&self, mut body: Vec<u8>) -> Result<Vec<u8>, PublisherError> {
        #[cfg(feature = "compression")]
        if self.compression != Compression::None {
            body = crate::support::compression::compress_member(self.compression, &body)
                .map_err(|e| PublisherError::NonRetryable(anyhow::anyhow!(e)))?;
        }
        #[cfg(feature = "encryption")]
        if let Some(crypto) = &self.crypto {
            let sealed = crypto
                .seal(&body, b"")
                .map_err(PublisherError::NonRetryable)?;
            // The consumer rejects any frame whose length prefix exceeds this cap, so a batch
            // sealing larger than it would be written but never read back. Fail fast and tell
            // the operator to shrink batch_size rather than emit a member that corrupts the
            // stream on read.
            if sealed.len() as u64 > MAX_ENCRYPTED_FRAME_BYTES {
                return Err(PublisherError::NonRetryable(anyhow::anyhow!(
                    "encrypted batch frame is {} bytes, exceeding the {} byte cap the consumer can read; reduce batch_size",
                    sealed.len(),
                    MAX_ENCRYPTED_FRAME_BYTES
                )));
            }
            body = Vec::with_capacity(8 + sealed.len());
            body.extend_from_slice(&(sealed.len() as u64).to_be_bytes());
            body.extend_from_slice(&sealed);
        }
        Ok(body)
    }

    /// True when batches are written as self-contained members (compressed
    /// and/or encrypted) rather than plain appended lines.
    #[cfg(any(feature = "compression", feature = "encryption"))]
    fn is_member_mode(&self) -> bool {
        #[allow(unused_mut)]
        let mut member = self.compression != Compression::None;
        #[cfg(feature = "encryption")]
        {
            member |= self.crypto.is_some();
        }
        member
    }

    /// Writes one batch as a single self-contained member appended to the file.
    /// Compressed members (gzip/lz4) self-delimit, so the file stays a standard
    /// `.gz`/`.lz4` stream. An encrypted (sealed) member does not, so it is
    /// wrapped in a `[u64 be length][sealed bytes]` frame instead.
    #[cfg(any(feature = "compression", feature = "encryption"))]
    async fn send_batch_member(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Open up front, before the CPU-bound encode/compress/seal below. Order matters for
        // throughput: this task lands in the producer thread's LIFO slot, so any CPU before its
        // first suspension holds the producer there. `open` always suspends (blocking-pool hop);
        // building the member first cost ~1.9ms/batch of producer stall, ~35% slower compressed
        // writes, and pinned the pipeline to one core regardless of `concurrency`.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .context("Failed to open file for writing batch member")?;
        // Members are concatenated, so the decoded stream is one continuous line
        // stream: the CSV header goes into the first member of a new file only.
        let is_csv = matches!(self.format, FileFormat::Csv);
        // CSV takes the file lock for the whole batch, because its header line has to land
        // in the *first* member of the file: the emptiness check, the header decision and
        // the append must stay atomic. Every other format appends self-contained records in
        // any order, so it leaves this `None` and the CPU-bound encode/compress/seal runs
        // outside the lock — the append below locks only if this is still `None`.
        let mut file_guard = if is_csv {
            Some(self.file_lock.lock().await)
        } else {
            None
        };
        let mut raw = Vec::new();
        let mut failed_messages = Vec::new();
        // Only a successful stat reporting zero bytes counts as empty. A failed stat is
        // treated as non-empty (matching the plain path's `pre_len == Some(0)`), so a
        // header can never be inserted into the middle of a file we could not measure.
        // The not-yet-created case is still empty: the append below creates the file.
        let file_is_empty = is_csv
            && match tokio::fs::metadata(&self.path).await {
                Ok(m) => m.len() == 0,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            };
        let mut csv_header_guard = if is_csv {
            Some(self.csv_header.lock().await)
        } else {
            None
        };
        if let Some(hdr) = csv_header_guard.as_mut() {
            if hdr.is_none() && !file_is_empty && self.csv.reads_header_back() {
                **hdr = self.existing_csv_header().await?;
            }
        }
        let mut wrote_csv_header = false;
        let mut csv_row_buf: Vec<u8> = Vec::new();
        for mut msg in messages {
            msg.strip_source_metadata();
            let encoded = match self.format {
                FileFormat::Csv => {
                    let hdr = csv_header_guard.as_mut().expect("csv header lock held");
                    csv_append_record(
                        &mut raw,
                        &msg,
                        hdr,
                        &mut csv_row_buf,
                        &self.delimiter,
                        &self.csv,
                        file_is_empty,
                    )
                    .map(|wrote| wrote_csv_header |= wrote)
                }
                ref fmt => encode_record(&msg, fmt, &self.delimiter).map(|bytes| {
                    raw.extend_from_slice(&bytes);
                    raw.extend_from_slice(&self.delimiter);
                }),
            };
            if let Err(e) = encoded {
                tracing::error!("Failed to serialize message for file sink member: {}", e);
                failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
            }
        }

        if !raw.is_empty() {
            // Every failure below leaves the member off disk (unwritten or rolled back), so a
            // CSV header established for this batch is cleared afterwards — otherwise the
            // retry would think the header was already written and emit a headerless file.
            let outcome: Result<(), PublisherError> = async {
                let member = self.encode_member(raw)?;

                // The member is fully built by now, so unless the batch already holds the
                // lock (CSV), it is taken here and covers only the append.
                if file_guard.is_none() {
                    file_guard = Some(self.file_lock.lock().await);
                }
                // Length before the append: a failed write_all can leave a partial
                // member behind, which would corrupt the concatenated stream and get
                // compounded by the Retryable re-append. Truncate back to this
                // known-good member boundary on failure so a retry appends cleanly.
                let pre_len = file
                    .metadata()
                    .await
                    .context("Failed to stat file before member write")?
                    .len();
                // Append the whole member in one write so a concurrent reader never
                // observes a torn member (the consumer also guards against it).
                if let Err(e) = file.write_all(&member).await {
                    if let Err(te) = file.set_len(pre_len).await {
                        tracing::error!(
                            "Failed to truncate file back to {} after member write error: {}",
                            pre_len,
                            te
                        );
                        // Rollback failed, so a partial member is still on disk. A Retryable
                        // re-append would concatenate onto that torn member and corrupt the
                        // whole stream, so fail permanently instead of letting a retry compound it.
                        return Err(PublisherError::NonRetryable(anyhow::Error::new(e).context(
                        "Failed to write member to file and could not truncate the partial write",
                    )));
                    }
                    return Err(PublisherError::Retryable(
                        anyhow::Error::new(e).context("Failed to write member to file"),
                    ));
                }
                // Same rollback as the write above: a failed flush can leave a partial
                // member on disk, which a Retryable re-append would concatenate onto.
                if let Err(e) = file.flush().await {
                    if let Err(te) = file.set_len(pre_len).await {
                        tracing::error!(
                            "Failed to truncate file back to {} after member flush error: {}",
                            pre_len,
                            te
                        );
                        return Err(PublisherError::NonRetryable(anyhow::Error::new(e).context(
                        "Failed to flush member to file and could not truncate the partial write",
                    )));
                    }
                    return Err(PublisherError::Retryable(
                        anyhow::Error::new(e).context("Failed to flush file"),
                    ));
                }
                if let Err(e) = self.sync_written(&file, pre_len == 0).await {
                    if let Err(te) = file.set_len(pre_len).await {
                        tracing::error!(
                            "Failed to truncate file back to {} after member sync error: {}",
                            pre_len,
                            te
                        );
                        return Err(PublisherError::NonRetryable(anyhow::Error::new(e).context(
                        "Failed to sync member to file and could not truncate the partial write",
                    )));
                    }
                    return Err(PublisherError::Retryable(
                        anyhow::Error::new(e).context("Failed to sync file"),
                    ));
                }
                Ok(())
            }
            .await;
            if let Err(e) = outcome {
                if wrote_csv_header {
                    if let Some(hdr) = csv_header_guard.as_mut() {
                        **hdr = None;
                    }
                }
                return Err(e);
            }
        }

        Ok(SentBatch::from_failures(failed_messages))
    }
}

/// Truncates the append-mode file back to `pre_len` (its length before the batch) so a Retryable
/// re-append starts from a clean record boundary instead of duplicating a partially written prefix.
/// When the file's CSV header was written in this batch it is rolled off with the prefix, so the
/// in-memory "header written" flag is cleared to make the retry rewrite it. `pre_len == None` (the
/// pre-batch stat failed) skips the rollback and preserves the old duplicate-on-retry behaviour.
async fn roll_back_partial_batch(
    writer: &BufWriter<File>,
    pre_len: Option<u64>,
    wrote_csv_header: bool,
    csv_header: Option<&mut tokio::sync::MutexGuard<'_, Option<Vec<String>>>>,
) {
    let Some(pl) = pre_len else { return };
    // A pipe or terminal (stdout) has nothing to roll back.
    if !matches!(writer.get_ref().metadata().await, Ok(m) if m.is_file()) {
        return;
    }
    if let Err(te) = writer.get_ref().set_len(pl).await {
        tracing::error!(
            "Failed to truncate file back to {} after write error: {}",
            pl,
            te
        );
        return;
    }
    if wrote_csv_header {
        if let Some(hdr) = csv_header {
            **hdr = None;
        }
    }
}

#[async_trait]
impl MessagePublisher for FilePublisher {
    #[instrument(skip_all, fields(batch_size = messages.len()), level = "debug")]
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

        #[cfg(any(feature = "compression", feature = "encryption"))]
        if self.is_member_mode() {
            return self.send_batch_member(messages).await;
        }

        trace!(count = messages.len(), path = %self.path, message_ids = ?LazyMessageIds(&messages), "Writing batch to file");
        let _file_guard = self.file_lock.lock().await;

        // Reopen per batch so external rotation/deletion (e.g. consumer delete mode) can't leave
        // us writing to a stale handle on a deleted inode. Costs some throughput for correctness.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .context("Failed to open file for writing batch")?;

        // Length before the batch: the BufWriter auto-flushes mid-loop, so a mid-batch failure can
        // leave a partial prefix. Truncate back here on failure so the retry re-appends cleanly.
        // `None` (stat failed) means no rollback point, so a failed batch is left as-is.
        let pre_len = file.metadata().await.ok().map(|m| m.len());
        let file_is_empty = matches!(self.format, FileFormat::Csv) && pre_len == Some(0);
        // 1 MiB, not tokio's 8 KiB default: at ~100 B/record a small buffer turns a
        // bulk copy into ~13k write syscalls. Worth ~18% on file-to-file throughput.
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        let mut failed_messages = Vec::new();
        // Tracks whether this batch wrote the CSV header. On rollback the header is truncated off
        // disk, so its in-memory "already written" flag must be cleared or the retry would skip it.
        let mut wrote_csv_header = false;
        let mut csv_header_guard = if matches!(self.format, FileFormat::Csv) {
            Some(self.csv_header.lock().await)
        } else {
            None
        };
        if let Some(hdr) = csv_header_guard.as_mut() {
            if hdr.is_none() && pre_len.is_some_and(|len| len > 0) {
                **hdr = self.existing_csv_header().await?;
            }
        }
        // Row buffer reused for every CSV record in this batch.
        let mut csv_row_buf: Vec<u8> = Vec::new();
        // Body + delimiter, likewise reused, so one contiguous write costs no
        // per-message allocation.
        let mut record_buf: Vec<u8> = Vec::new();

        // Iterate over messages, consuming them
        for mut msg in messages {
            // Strip per-hop source/provenance keys in place — they are not
            // persisted. Done on the owned message (no payload clone); a message
            // pushed to `failed_messages` keeps its remaining fields, and the
            // dropped `mqb.src.*` keys are irrelevant to a retry on the next hop.
            msg.strip_source_metadata();
            record_buf.clear();
            let encoded = match self.format {
                FileFormat::Csv => {
                    let hdr = csv_header_guard.as_mut().expect("csv header lock held");
                    csv_append_record(
                        &mut record_buf,
                        &msg,
                        hdr,
                        &mut csv_row_buf,
                        &self.delimiter,
                        &self.csv,
                        file_is_empty,
                    )
                }
                ref fmt => encode_record(&msg, fmt, &self.delimiter).map(|body| {
                    record_buf.extend_from_slice(&body);
                    record_buf.extend_from_slice(&self.delimiter);
                    false
                }),
            };
            match encoded {
                // Set before the write: a rollback has to clear the header even when the
                // write itself fails.
                Ok(wrote_header) => wrote_csv_header |= wrote_header,
                Err(e) => {
                    tracing::error!("Failed to serialize message for file sink: {}", e);
                    failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
                    continue;
                }
            }
            // Header, body and delimiter go out as one contiguous write, so a concurrent
            // tailing reader never observes a record without its delimiter.
            if let Err(e) = writer.write_all(&record_buf).await {
                tracing::error!("Failed to write message to file: {}", e);
                // A buffered write failure leaves the BufWriter in an undefined state and the
                // remaining messages in this batch are unwritten. Abort so the whole batch is
                // retried rather than reusing the writer, flushing partial data, or acking
                // messages that never reached the file.
                roll_back_partial_batch(
                    &writer,
                    pre_len,
                    wrote_csv_header,
                    csv_header_guard.as_mut(),
                )
                .await;
                return Err(PublisherError::Retryable(
                    anyhow::Error::new(e).context("Failed to write message to file"),
                ));
            }
        }

        if let Err(e) = writer.flush().await {
            // A partial flush can leave part of the batch on disk; roll back so the
            // Retryable re-append doesn't duplicate the flushed prefix.
            roll_back_partial_batch(
                &writer,
                pre_len,
                wrote_csv_header,
                csv_header_guard.as_mut(),
            )
            .await;
            return Err(PublisherError::Retryable(
                anyhow::Error::new(e).context("Failed to flush file writer"),
            ));
        }
        if let Err(e) = self
            .sync_written(writer.get_ref(), pre_len.is_none_or(|len| len == 0))
            .await
        {
            roll_back_partial_batch(
                &writer,
                pre_len,
                wrote_csv_header,
                csv_header_guard.as_mut(),
            )
            .await;
            return Err(PublisherError::Retryable(
                anyhow::Error::new(e).context("Failed to sync file"),
            ));
        }
        Ok(SentBatch::from_failures(failed_messages))
    }

    async fn flush(&self) -> anyhow::Result<()> {
        if let Some(staging) = &self.part_staging {
            return Ok(staging.finalize().await?);
        }
        if let FsyncPolicy::Periodic(state) = &self.fsync {
            state.sync(&self.path).await?;
        }
        Ok(())
    }

    /// `periodic` and staged parts ack before the sync, so the route holds commits until `flush`.
    fn deferred_commit_interval(&self) -> Option<Duration> {
        if let Some(staging) = &self.part_staging {
            return Some(staging.interval());
        }
        match &self.fsync {
            FsyncPolicy::Periodic(state) => state.interval,
            _ => None,
        }
    }

    /// Syncs the last `periodic` window when the route stops; the background task ends unsynced.
    fn on_disconnect_hook(&self) -> Option<crate::traits::BoxFuture<'_, anyhow::Result<()>>> {
        (self.part_staging.is_some() || matches!(self.fsync, FsyncPolicy::Periodic(_)))
            .then(|| self.flush())
    }

    /// A file is an ordered log by construction: appending batches in the order they
    /// were read is the whole point of exporting to JSONL/CSV.
    fn requires_ordered_publish(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static FILE_EVENT_STORES: Lazy<Mutex<HashMap<String, Weak<EventStore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

struct FileFeedState {
    /// For Consume mode: number of lines currently buffered in EventStore.
    lines_in_memory: usize,
}

/// Most lines a subscribe + delete store holds that no subscriber has received yet.
const MAX_UNREAD_LINES: u64 = 10_000;

/// Creates an EventStore backed by a file.
/// The EventStore acts as an in-memory buffer for the file content, allowing unified handling of Consume and Subscribe modes.
async fn create_file_event_store(
    path: &str,
    delimiter: Vec<u8>,
    format: FileFormat,
) -> anyhow::Result<Arc<EventStore>> {
    let path = path.to_string();
    // Shared state to coordinate the reader and the drop (delete) logic.
    let feed_state = Arc::new(Mutex::new(FileFeedState { lines_in_memory: 0 }));

    // Lock to serialize file modification operations
    let file_op_lock = get_file_lock(&path);

    let feed_state_clone = feed_state.clone();
    let path_clone = path.clone();
    let file_op_lock_clone = file_op_lock.clone();
    let delimiter_clone = delimiter.clone();
    let format_gc = format.clone();

    // Never drop unacked lines: the drop callback deletes them from the file. The reader
    // bounds memory instead (`MAX_UNREAD_LINES`).
    let retention = RetentionPolicy {
        gc_interval: std::time::Duration::ZERO,
        max_count: None,
        max_age: None,
        ..Default::default()
    };
    // Use immediate GC for file stores to ensure files are truncated promptly on ack.

    // 1. Create EventStore with on_drop callback
    let store = Arc::new(
        EventStore::new(retention).with_async_drop_callback(move |events| {
            let count = events.len();
            let state = feed_state_clone.clone();
            let path = path_clone.clone();
            let file_op_lock = file_op_lock_clone.clone();
            let delimiter = delimiter_clone.clone();
            let format = format_gc.clone();

            // Awaited by the ack, so acked lines are gone before the commit returns.
            Box::pin(async move {
                if count == 0 {
                    return;
                }
                // Serialize file operations to prevent race conditions between multiple GCs
                let _guard = file_op_lock.lock().await;

                if let Err(e) = remove_lines_from_file(
                    &path,
                    0,
                    count,
                    &delimiter,
                    &format,
                    &CsvDialect::default(),
                )
                .await
                {
                    // The lines stay counted, so this run skips them; a restart reads
                    // them again.
                    tracing::error!("Failed to remove lines from file {}: {}", path, e);
                } else {
                    let mut s = state.lock().await;
                    s.lines_in_memory = s.lines_in_memory.saturating_sub(count);
                    trace!("Removed {} lines from {}", count, path);
                }
            })
        }),
    );

    // 2. Spawn background reader task
    let store_weak = Arc::downgrade(&store);
    let path_clone = path.clone();
    let feed_state_clone = feed_state.clone();
    let file_op_lock_clone = file_op_lock.clone();
    let format_clone = format;

    tokio::spawn(async move {
        let mut current_sleep = std::time::Duration::from_millis(1);
        const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
        // CSV is not supported in this backend (Subscribe + delete); see FileConsumer::new.
        let mut csv_header: Option<CsvHeader> = None;
        let mut held_partial = None;

        loop {
            // Check if the store is still alive
            let store_clone = match store_weak.upgrade() {
                Some(s) => s,
                None => break, // Exit if EventStore is dropped
            };

            // Acquire file op lock first to coordinate with GC
            let file_guard = Some(file_op_lock_clone.lock().await);

            let mut state = feed_state_clone.lock().await;

            // Open file
            let file_res = OpenOptions::new().read(true).open(&path_clone).await;
            let file = match file_res {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open file {}: {}", path_clone, e);
                    drop(state);
                    drop(file_guard);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Position the reader
            // In consume mode, we skip lines that are already buffered in memory
            // because they are still in the file (until dropped).
            let mut reader = BufReader::new(file);
            let mut lines_skipped = 0;
            let mut error = false;
            let lines_to_skip = state.lines_in_memory;
            while lines_skipped < lines_to_skip {
                let mut buf = Vec::new();
                match read_record(
                    &mut reader,
                    &delimiter,
                    &format_clone,
                    &CsvDialect::default(),
                    &mut buf,
                )
                .await
                {
                    Ok(0) => break, // EOF
                    Ok(_) => lines_skipped += 1,
                    Err(e) => {
                        tracing::error!("Error skipping lines in {}: {}", path_clone, e);
                        error = true;
                        break;
                    }
                }
            }
            if error {
                drop(state);
                drop(file_guard);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            // Release file op lock to allow publisher to write while we read
            drop(file_guard);

            // Keep the same reader: its buffer already holds the bytes after the skipped lines.
            let mut lines_read = 0;
            let mut batch = Vec::with_capacity(128);
            let mut pending_partial = false;

            while store_clone.unread() < MAX_UNREAD_LINES {
                let mut buffer = Vec::new();
                match read_record(
                    &mut reader,
                    &delimiter,
                    &format_clone,
                    &CsvDialect::default(),
                    &mut buffer,
                )
                .await
                {
                    Ok(0) => break,
                    Ok(_)
                        if !buffer.ends_with(&delimiter)
                            && !partial_record_settled(&mut held_partial, buffer.len()) =>
                    {
                        pending_partial = true;
                        break;
                    }
                    Ok(_) => {
                        if buffer.ends_with(&delimiter) {
                            buffer.truncate(buffer.len() - delimiter.len());
                        }
                        if delimiter.len() == 1 && delimiter[0] == b'\n' && buffer.ends_with(b"\r")
                        {
                            buffer.pop();
                        }
                        if let Some(msg) = parse_message(&buffer, &format_clone, &mut csv_header) {
                            batch.push(msg);
                        }
                        lines_read += 1;

                        state.lines_in_memory += 1;

                        if batch.len() >= 128 {
                            store_clone.append_batch(std::mem::take(&mut batch)).await;
                            batch.reserve(128);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error reading from {}: {}", path_clone, e);
                        break;
                    }
                }
            }

            if !batch.is_empty() {
                store_clone.append_batch(batch).await;
            }
            if !pending_partial {
                held_partial = None;
            }

            drop(state); // Release lock before sleeping

            // If we didn't read anything, sleep a bit (polling)
            if lines_read == 0 {
                tokio::time::sleep(current_sleep).await;
                current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
            } else {
                current_sleep = std::time::Duration::from_millis(1);
            }
        }
    });

    Ok(store)
}

/// How long a final record without its delimiter must stay unchanged before a delete-mode
/// reader takes it for complete rather than for a write in progress.
const PARTIAL_RECORD_HOLD: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether an unterminated final record of `len` bytes has been unchanged long enough.
fn partial_record_settled(held: &mut Option<(usize, std::time::Instant)>, len: usize) -> bool {
    match held {
        Some((seen, since)) if *seen == len => since.elapsed() >= PARTIAL_RECORD_HOLD,
        _ => {
            *held = Some((len, std::time::Instant::now()));
            false
        }
    }
}

/// Drops `count` *records* from `path`, behind the first `keep` (a CSV header). `format`
/// matters: a CSV record can span several delimiters, and consuming the wrong number of
/// them would corrupt the remainder.
async fn remove_lines_from_file(
    path: &str,
    keep: usize,
    count: usize,
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
) -> anyhow::Result<()> {
    let unique_id = fast_uuid_v7::gen_id_str();
    let temp_path = format!("{}.{}.tmp", path, unique_id);

    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let temp_file = File::create(&temp_path).await?;
    let mut writer = BufWriter::new(temp_file);

    let mut buf = Vec::new();
    for _ in 0..keep {
        buf.clear();
        if read_record(&mut reader, delimiter, format, csv, &mut buf).await? == 0 {
            break;
        }
        if let Err(e) = writer.write_all(&buf).await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(e.into());
        }
    }
    let mut lines_skipped = 0;
    while lines_skipped < count {
        buf.clear();
        if read_record(&mut reader, delimiter, format, csv, &mut buf).await? == 0 {
            break;
        }
        lines_skipped += 1;
    }

    if let Err(e) = io::copy(&mut reader, &mut writer).await {
        let _ = fs::remove_file(&temp_path).await;
        return Err(e.into());
    }

    writer.flush().await?;
    let temp_file = writer.into_inner();
    temp_file.sync_all().await?;
    drop(temp_file); // Close writer handle
    drop(reader); // Close reader handle

    fs::rename(&temp_path, path).await?;

    // Sync parent directory to ensure rename is durable
    if let Some(parent) = Path::new(path).parent() {
        if let Ok(parent_dir) = File::open(parent).await {
            let _ = parent_dir.sync_all().await;
        }
    }

    Ok(())
}
struct FileTailConsumer {
    msg_rx: async_channel::Receiver<Vec<CanonicalMessage>>,
    buffer: Vec<CanonicalMessage>,
    offset_file: Option<Arc<Mutex<tokio::fs::File>>>,
    ready: Arc<AtomicBool>,
    /// Set when a greedy fill consumed the watcher's end-of-file marker after
    /// data; the next `receive_batch` surfaces it as an empty batch so a route
    /// with `exit_on_empty` can drain-then-exit.
    pending_eof: bool,
    /// Member (compressed/encrypted) readers publish the reason they gave up
    /// decoding here before closing the channel. When set, a closed channel is a
    /// permanent decode failure (wrong codec/key), reported as
    /// `ConsumerError::Permanent` so the route fails instead of completing cleanly.
    /// `None` for plain readers, whose closed channel is a normal end-of-stream.
    decode_error: Option<Arc<StdMutex<Option<String>>>>,
    /// Set by the route when `exit_on_empty` is active. Tells the reader thread a
    /// final record without a trailing delimiter is a complete record to emit
    /// (the file is done), not a torn mid-write to withhold. Shared with the thread.
    drain_on_empty: Arc<AtomicBool>,
}

/// Returns the `compression` codec name if the file at `path` begins with that
/// compressor's magic bytes. Used to reject reading a compressed file with no
/// `compression` configured (which would emit undecoded garbage). Names match the
/// `Compression` config spelling. `None` if the file is absent, empty, too short,
/// or has no recognized magic.
fn sniff_compression_magic(path: &str) -> Option<&'static str> {
    use std::io::Read;
    let mut head = [0u8; 4];
    // A pipe cannot be read twice: sniffing would swallow its first bytes.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    compression_magic_name(&head[..n])
}

/// The `compression` codec name whose magic bytes `head` begins with, if any.
fn compression_magic_name(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x1f, 0x8b]) {
        Some("gzip")
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some("zstd")
    } else if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        Some("lz4")
    } else {
        None
    }
}

/// Whether the file at `path` looks like an at-rest encrypted file: an 8-byte
/// big-endian frame length that fits the file, followed by a well-formed crypto
/// envelope header (`[version=1][cipher 0|1][key_id_len>=1]`). Used to reject
/// reading one with no `encryption` configured, which would otherwise emit
/// ciphertext as messages under a clean success.
fn looks_encrypted_at_rest(path: &str) -> bool {
    use crate::support::crypto_envelope::{
        CIPHER_AES_GCM, CIPHER_XCHACHA, ENVELOPE_VERSION, MIN_ENVELOPE_LEN,
    };
    use std::io::Read;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(file_len) = f.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut head = [0u8; 11];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    let frame_len = u64::from_be_bytes(head[..8].try_into().expect("8 bytes"));
    frame_len >= MIN_ENVELOPE_LEN as u64
        && frame_len.saturating_add(8) <= file_len
        && head[8] == ENVELOPE_VERSION
        && (head[9] == CIPHER_XCHACHA || head[9] == CIPHER_AES_GCM)
        && head[10] >= 1
}

/// Blocking [`read_record`].
fn read_record_sync<R: std::io::BufRead>(
    reader: &mut R,
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    read_record_sync_checked(reader, delimiter, format, csv, buf, &mut false)
}

/// [`read_record_sync`]; `open_quote` is set when the input ended inside a quoted field.
fn read_record_sync_checked<R: std::io::BufRead>(
    reader: &mut R,
    delimiter: &[u8],
    format: &FileFormat,
    csv: &CsvDialect,
    buf: &mut Vec<u8>,
    open_quote: &mut bool,
) -> std::io::Result<usize> {
    *open_quote = false;
    let start = buf.len();
    let mut total = read_until_bytes_sync(reader, delimiter, buf)?;
    let Some(mut quotes) = record_quotes(format, csv, &buf[start..]) else {
        return Ok(total);
    };
    while total > 0 && quotes.in_quotes() {
        let from = buf.len();
        let n = read_until_bytes_sync(reader, delimiter, buf)?;
        if n == 0 {
            *open_quote = true;
            break;
        }
        quotes.feed(&buf[from..]);
        total += n;
    }
    Ok(total)
}

fn read_until_bytes_sync<R: std::io::BufRead>(
    reader: &mut R,
    delimiter: &[u8],
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    if delimiter.len() == 1 {
        return reader.read_until(delimiter[0], buf);
    }
    let last_byte = delimiter[delimiter.len() - 1];
    let mut total_read = 0;
    loop {
        let n = reader.read_until(last_byte, buf)?;
        if n == 0 {
            return Ok(total_read);
        }
        total_read += n;
        if buf.len() >= delimiter.len() && &buf[buf.len() - delimiter.len()..] == delimiter {
            return Ok(total_read);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_file_tail_task_sync(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    initial_offset: u64,
    group_id: Option<String>,
    delimiter: Vec<u8>,
    format: FileFormat,
    csv: CsvDialect,
    ready: Arc<AtomicBool>,
    drain_on_empty: Arc<AtomicBool>,
    fatal_error_slot: Arc<StdMutex<Option<String>>>,
) {
    let mut last_position: u64 = initial_offset;
    let mut reader: Option<std::io::BufReader<std::fs::File>> = None;
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(50);
    let mut initialized = false;
    // A pipe, FIFO or terminal: read once without seeking, and its end is final.
    let mut stream = false;
    // Tracks whether we've already emitted the empty end-of-file marker for the
    // current drained state, so we signal it once per EOF transition rather than
    // on every idle poll.
    let mut signaled_eof = false;
    const BATCH_SIZE: usize = 1024;
    let mut buf = Vec::with_capacity(1024);
    let mut records_buf: Vec<u8> = Vec::with_capacity(128 * BATCH_SIZE);
    let mut spans: Vec<RecordSpan> = Vec::with_capacity(BATCH_SIZE);
    let mut csv_header =
        matches!(format, FileFormat::Csv).then(|| Arc::new(CsvHeader::unread(csv.clone())));

    loop {
        if reader.is_none() {
            let mut file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    // The consumer is gone: stop instead of polling for the file forever.
                    if msg_tx.is_closed() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let metadata = file.metadata().ok();
            stream = metadata.as_ref().is_some_and(|m| !m.file_type().is_file());
            if let Some(metadata) = metadata.filter(|_| !stream) {
                if metadata.len() < last_position {
                    tracing::warn!("File {} was truncated. Resetting position to 0.", path);
                    last_position = 0;
                }
            }

            if stream {
                // Not seekable.
            } else if let Err(e) = file.seek(std::io::SeekFrom::Start(last_position)) {
                tracing::error!("Failed to seek in {}: {}", path, e);
                last_position = 0; // Reset on seek failure
                if let Err(e) = file.seek(std::io::SeekFrom::Start(0)) {
                    tracing::error!("Failed to reset seek to 0 in {}: {}", path, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }

            reader = Some(std::io::BufReader::with_capacity(128 * BATCH_SIZE, file));
            if !initialized {
                ready.store(true, Ordering::SeqCst);
                initialized = true;
            }
        }

        // Records are buffered whole, then decoded together below so the decode can be
        // spread across cores while this thread stays on the file.
        records_buf.clear();
        spans.clear();
        let mut lines_read_in_batch = 0;
        // Set when a final record without its delimiter is withheld (live tail). It
        // suppresses the EOF marker below so a route cannot `exit_on_empty` before the
        // record is delivered — closing the race where the reader reaches EOF before
        // the route propagates its drain intent via `set_exit_on_empty`.
        let mut pending_partial = false;
        let mut open_quote = false;
        let mut fatal = None;
        let mut stream_ended = false;
        // The end of a stream is final, as the end of a file is under a drain.
        let complete = stream || drain_on_empty.load(Ordering::SeqCst);

        if let Some(r) = reader.as_mut() {
            for _ in 0..BATCH_SIZE {
                // A slow writer must not hold back what has already arrived.
                if stream && lines_read_in_batch > 0 && r.buffer().is_empty() {
                    break;
                }
                buf.clear();
                match read_record_sync_checked(
                    r,
                    &delimiter,
                    &format,
                    &csv,
                    &mut buf,
                    &mut open_quote,
                ) {
                    Ok(0) => {
                        stream_ended = stream;
                        break; // EOF
                    }
                    Ok(_) if open_quote => {
                        if complete {
                            fatal = Some(format!(
                                "csv: {path} ends inside a quoted field (record at byte {last_position}); the file looks truncated"
                            ));
                        } else {
                            // Live tail: the writer may still close the quote.
                            pending_partial = true;
                            reader = None;
                        }
                        break;
                    }
                    Ok(n) => {
                        if !buf.ends_with(&delimiter) {
                            if complete {
                                // Drain mode (exit_on_empty): the file is complete, so a
                                // final record with no trailing delimiter is a whole
                                // record. Emit it once, advancing past it; the next read
                                // returns 0 (EOF) and the empty marker fires normally.
                                last_position += n as u64;
                                strip_leading_bom(&mut buf, last_position - n as u64, &format);
                                if delimiter.len() == 1
                                    && delimiter[0] == b'\n'
                                    && buf.ends_with(b"\r")
                                {
                                    buf.pop();
                                }
                                let start = records_buf.len();
                                records_buf.extend_from_slice(&buf);
                                spans.push((start, records_buf.len(), last_position));
                                lines_read_in_batch += 1;
                                break;
                            }
                            // Live tail (or drain intent not yet observed): torn/partial
                            // line, the writer's content reached disk ahead of its trailing
                            // delimiter. Don't advance the position or emit a message; drop
                            // the reader so the next iteration reopens and re-seeks to
                            // last_position, re-reading the line whole once the writer
                            // finishes it (or the drain flag flips and it is emitted above).
                            pending_partial = true;
                            reader = None;
                            break;
                        }
                        strip_leading_bom(&mut buf, last_position, &format);
                        last_position += n as u64;
                        buf.truncate(buf.len() - delimiter.len());
                        if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r") {
                            buf.pop();
                        }
                        let start = records_buf.len();
                        records_buf.extend_from_slice(&buf);
                        spans.push((start, records_buf.len(), last_position));
                        lines_read_in_batch += 1;
                    }
                    Err(e) if stream => {
                        fatal = Some(format!("Error reading {path}: {e}"));
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Error reading {}: {}", path, e);
                        reader = None; // Force reopen on next loop
                        break;
                    }
                }
            }
        }

        let batch = decode_records(
            &mut records_buf,
            &spans,
            &format,
            &mut csv_header,
            group_id.is_some(),
        );

        if !batch.is_empty() {
            if msg_tx.send_blocking(batch).is_err() {
                break; // Consumer dropped, exit thread
            }
            current_sleep = std::time::Duration::from_millis(1);
            signaled_eof = false; // data flowed; re-arm the EOF marker
        }

        if let Some(reason) = fatal {
            tracing::error!("{reason}; closing stream");
            *fatal_error_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
            break;
        }

        if stream_ended {
            break; // Closing the channel ends the route: a stream does not resume.
        }

        if lines_read_in_batch == 0 {
            // EOF reached. Emit an empty batch once so a drained route can pause
            // or, with exit_on_empty, terminate. Re-armed when new data arrives.
            // Suppressed while a partial final record is pending (see pending_partial).
            if !signaled_eof && !pending_partial {
                if msg_tx.send_blocking(Vec::new()).is_err() {
                    break; // Consumer dropped, exit thread
                }
                signaled_eof = true;
            }
            std::thread::sleep(current_sleep);
            current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
            // Invalidate reader to check for file changes (like rotation) on next poll
            reader = None;
        }
    }
}

/// Blank lines that precede a queued message in the file, by message id; acking the
/// message deletes them too.
type ExtraLines = Arc<StdMutex<HashMap<u128, usize>>>;

struct FileQueueConsumer {
    msg_rx: async_channel::Receiver<Vec<CanonicalMessage>>,
    lines_in_memory: Arc<AtomicUsize>,
    extra_lines: ExtraLines,
    /// 1 once a CSV header was read: it stays at the front of the file for a restart.
    kept_header: Arc<AtomicUsize>,
    path: String,
    file_lock: Arc<Mutex<()>>,
    buffer: Arc<Mutex<Vec<CanonicalMessage>>>,
    delimiter: Vec<u8>,
    /// Needed on the commit path too: deleting acked records re-splits the file, and CSV
    /// records do not map one-to-one onto delimiters.
    format: FileFormat,
    csv: CsvDialect,
    ready: Arc<AtomicBool>,
    /// See [`FileTailConsumer::pending_eof`].
    pending_eof: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_file_queue_task(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    lines_in_memory: Arc<AtomicUsize>,
    file_lock: Arc<Mutex<()>>,
    runtime_handle: tokio::runtime::Handle,
    delimiter: Vec<u8>,
    format: FileFormat,
    csv: CsvDialect,
    ready: Arc<AtomicBool>,
    extra_lines: ExtraLines,
    kept_header: Arc<AtomicUsize>,
) {
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
    let mut initialized = false;
    // Emit the empty end-of-file marker once per drained state; see the tail task.
    let mut signaled_eof = false;
    let mut buf = Vec::new();
    let mut csv_header = matches!(format, FileFormat::Csv).then(|| CsvHeader::unread(csv.clone()));
    let mut held_partial = None;

    loop {
        buf.clear();
        let mut batch = Vec::with_capacity(128);
        let mut lines_read = 0;
        let mut blanks = 0;
        let mut pending_partial = false;

        {
            let _guard = runtime_handle.block_on(file_lock.lock());
            let skip_count = lines_in_memory.load(Ordering::SeqCst);
            let kept = kept_header.load(Ordering::SeqCst);

            let file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    // The consumer is gone: stop instead of polling for the file forever.
                    if msg_tx.is_closed() {
                        return;
                    }
                    drop(_guard);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let mut reader = std::io::BufReader::new(file);
            let mut skipped = 0;
            let mut error = false;

            while skipped < kept + skip_count {
                buf.clear();
                match read_record_sync(&mut reader, &delimiter, &format, &csv, &mut buf) {
                    Ok(0) => break,
                    Ok(_) => skipped += 1,
                    Err(e) => {
                        tracing::error!("Error skipping lines in {}: {}", path, e);
                        error = true;
                        break;
                    }
                }
            }

            if !error {
                // Bounded by emitted rows, not records: a run of blank records must not end the
                // read early, or an empty batch would pose as EOF.
                let mut at_start = skipped == 0;
                while batch.len() < 128 {
                    buf.clear();
                    match read_record_sync(&mut reader, &delimiter, &format, &csv, &mut buf) {
                        Ok(0) => break,
                        Ok(_)
                            if !buf.ends_with(&delimiter)
                                && !partial_record_settled(&mut held_partial, buf.len()) =>
                        {
                            pending_partial = true;
                            break;
                        }
                        Ok(_) => {
                            if buf.ends_with(&delimiter) {
                                buf.truncate(buf.len() - delimiter.len());
                            }
                            if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r")
                            {
                                buf.pop();
                            }
                            if std::mem::take(&mut at_start) {
                                strip_leading_bom(&mut buf, 0, &format);
                            }
                            match parse_message(&buf, &format, &mut csv_header) {
                                Some(msg) => {
                                    if blanks > 0 {
                                        extra_lines
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .insert(msg.message_id, blanks);
                                    }
                                    lines_read += 1 + blanks;
                                    blanks = 0;
                                    batch.push(msg);
                                }
                                // The CSV header stays in the file: a restart names the
                                // columns by it.
                                None if !buf.is_empty() => kept_header.store(1, Ordering::SeqCst),
                                // A blank line at the front: nothing unacked precedes it, so
                                // remove it now, outside the line accounting.
                                None if skip_count == 0 && lines_read == 0 && blanks == 0 => {
                                    let keep = kept_header.load(Ordering::SeqCst);
                                    if let Err(e) = runtime_handle.block_on(remove_lines_from_file(
                                        &path, keep, 1, &delimiter, &format, &csv,
                                    )) {
                                        tracing::error!(
                                            "Failed to remove blank line from {}: {}",
                                            path,
                                            e
                                        );
                                    }
                                }
                                // A blank line behind unacked rows is deleted with the next row.
                                None => blanks += 1,
                            }
                        }
                        Err(_) => break,
                    }
                }

                if !initialized {
                    ready.store(true, Ordering::SeqCst);
                    initialized = true;
                }
            }
        }
        if !pending_partial {
            held_partial = None;
        }

        if lines_read > 0 {
            lines_in_memory.fetch_add(lines_read, Ordering::SeqCst);
            if msg_tx.send_blocking(batch).is_err() {
                break;
            }
            current_sleep = std::time::Duration::from_millis(1);
            signaled_eof = false; // data flowed; re-arm the EOF marker
        } else if pending_partial {
            // Not the end of the file yet: no EOF marker while a record is held back.
            std::thread::sleep(PARTIAL_RECORD_HOLD / 4);
        } else {
            // EOF: emit an empty batch once so a drained route can pause or,
            // with exit_on_empty, terminate. Re-armed when new data arrives.
            if !signaled_eof {
                if msg_tx.send_blocking(Vec::new()).is_err() {
                    break;
                }
                signaled_eof = true;
            }
            std::thread::sleep(current_sleep);
            current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
        }
    }
}

/// Reader for member-based files (compressed and/or encrypted). Such a stream
/// can't be seeked to a line boundary, so on each growth of the file it
/// re-decodes from the start and skips the records already emitted. For the
/// common write-once-then-read ETL case the file is decoded exactly once;
/// live-tailing a growing file costs a re-scan per growth (acceptable for v1).
///
/// Operational note: because each growth re-decodes from the start, tailing a
/// member stream that grows unboundedly is O(n²) in total CPU over its lifetime.
/// Size or rotate compressed/encrypted inputs (finite members, then a new file)
/// rather than appending to a single member stream indefinitely.
///
/// `make_reader` builds the decoding [`Read`](std::io::Read) chain
/// (decrypt frames and/or decompress members) over a freshly opened file.
#[cfg(any(feature = "compression", feature = "encryption"))]
#[allow(clippy::too_many_arguments)]
fn run_file_member_consume_task_sync<F>(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    delimiter: Vec<u8>,
    format: FileFormat,
    csv: CsvDialect,
    ready: Arc<AtomicBool>,
    decode_error_slot: Arc<StdMutex<Option<String>>>,
    make_reader: F,
) where
    F: Fn(std::fs::File) -> Box<dyn std::io::Read>,
{
    const BATCH_SIZE: usize = 1024;
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
    // Consecutive decode failures at an unchanged file length before we give up and
    // close the stream (surfacing EndOfStream) rather than retrying forever or
    // silently emitting drain markers for records we can no longer reach.
    const MAX_DECODE_FAILURES: u32 = 5;
    let mut records_emitted: usize = 0;
    let mut last_len: u64 = u64::MAX; // force the first read
    let mut initialized = false;
    let mut signaled_eof = false;
    let mut current_sleep = std::time::Duration::from_millis(1);
    let mut buf = Vec::new();
    let mut decode_failures: u32 = 0;
    let mut failure_len: u64 = u64::MAX;
    // Kept across passes: the header is read once, later passes skip past it.
    let mut csv_header = matches!(format, FileFormat::Csv).then(|| CsvHeader::unread(csv.clone()));
    // A final record without its delimiter, and since when the file has not grown past it.
    let mut held_tail: Option<std::time::Instant> = None;

    loop {
        let cur_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        // The file shrank (truncated or rotated): records we already emitted no longer
        // exist, so re-read the member stream from the start instead of skipping past
        // live data. Mirrors the tail reader resetting its byte position to 0. Guarded by
        // a real baseline (`last_len` is `u64::MAX` until the first clean pass) so a
        // decode-error pass — which never advances `last_len` — is not seen as a shrink.
        if initialized && last_len != u64::MAX && cur_len < last_len {
            tracing::warn!(
                "File {} was truncated; re-reading member stream from the start.",
                path
            );
            records_emitted = 0;
            decode_failures = 0;
            failure_len = u64::MAX;
            signaled_eof = false;
            csv_header = matches!(format, FileFormat::Csv).then(|| CsvHeader::unread(csv.clone()));
        }

        // No growth since the last full pass: emit the drain marker once, then poll.
        let emit_tail = if initialized && cur_len == last_len {
            match held_tail {
                // The unterminated last record did not grow: it is the last record.
                Some(since) if since.elapsed() >= PARTIAL_RECORD_HOLD => true,
                Some(_) => {
                    std::thread::sleep(PARTIAL_RECORD_HOLD / 4);
                    continue;
                }
                None => {
                    if !signaled_eof {
                        if msg_tx.send_blocking(Vec::new()).is_err() {
                            break;
                        }
                        signaled_eof = true;
                    }
                    std::thread::sleep(current_sleep);
                    current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
                    continue;
                }
            }
        } else {
            held_tail = None;
            false
        };

        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to open {}: {}", path, e);
                // The consumer is gone: stop instead of polling for the file forever.
                if msg_tx.is_closed() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };
        let mut reader = std::io::BufReader::new(make_reader(file));

        // Skip records emitted on a previous pass (file re-read from the start).
        let mut skipped = 0;
        let mut decode_error = false;
        while skipped < records_emitted {
            buf.clear();
            match read_record_sync(&mut reader, &delimiter, &format, &csv, &mut buf) {
                Ok(0) => break,
                Ok(_) => skipped += 1,
                Err(e) => {
                    tracing::error!("Error decoding {}: {}", path, e);
                    decode_error = true;
                    break;
                }
            }
        }
        if decode_error {
            // Skip failed: do NOT advance records_emitted/last_len. Bound the retries
            // so permanent corruption surfaces instead of spinning forever.
            if cur_len == failure_len {
                decode_failures += 1;
            } else {
                failure_len = cur_len;
                decode_failures = 1;
            }
            if decode_failures >= MAX_DECODE_FAILURES {
                let msg = format!(
                    "Giving up decoding {path} after {decode_failures} failed attempts at {cur_len} bytes; \
                     the file's compression/encryption likely does not match this endpoint's config"
                );
                tracing::error!("{msg}; closing stream");
                *decode_error_slot.lock().unwrap() = Some(msg);
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        let mut new_count = 0;
        let mut read_error = false;
        let mut tail_pending = false;
        let mut batch = Vec::with_capacity(256);
        let mut at_start = skipped == 0;
        loop {
            buf.clear();
            match read_record_sync(&mut reader, &delimiter, &format, &csv, &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if buf.ends_with(&delimiter) {
                        buf.truncate(buf.len() - delimiter.len());
                    } else if !emit_tail {
                        // Maybe a writer mid-append: held back until the file has stopped
                        // growing, then read again and emitted.
                        tail_pending = true;
                        break;
                    }
                    if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r") {
                        buf.pop();
                    }
                    if std::mem::take(&mut at_start) {
                        strip_leading_bom(&mut buf, 0, &format);
                    }
                    if let Some(msg) = parse_message(&buf, &format, &mut csv_header) {
                        batch.push(msg);
                    }
                    new_count += 1;
                    if batch.len() >= BATCH_SIZE {
                        if msg_tx.send_blocking(std::mem::take(&mut batch)).is_err() {
                            return;
                        }
                        batch = Vec::with_capacity(256);
                    }
                }
                Err(e) => {
                    // Truncated/torn member: stop; retry once the writer finishes it.
                    tracing::debug!("Partial member decode of {}: {}", path, e);
                    read_error = true;
                    break;
                }
            }
        }
        if !batch.is_empty() && msg_tx.send_blocking(batch).is_err() {
            return;
        }

        // Records actually emitted this pass must be skipped next time, even if the
        // pass then hit a decode error on the tail.
        records_emitted += new_count;
        if !initialized {
            ready.store(true, Ordering::SeqCst);
            initialized = true;
        }

        if read_error {
            // The pass ended on a decode error, not a clean EOF. Do NOT advance
            // last_len: a torn member the writer is still completing is retried on the
            // next pass (once the file grows), while permanent corruption at a fixed
            // length is bounded here so it surfaces as EndOfStream instead of silently
            // emitting drain markers for records we can no longer reach.
            if cur_len == failure_len {
                decode_failures += 1;
            } else {
                failure_len = cur_len;
                decode_failures = 1;
            }
            if decode_failures >= MAX_DECODE_FAILURES {
                let msg = format!(
                    "Giving up decoding {path} after {decode_failures} failed attempts at {cur_len} bytes; \
                     the file's compression/encryption likely does not match this endpoint's config"
                );
                tracing::error!("{msg}; closing stream");
                *decode_error_slot.lock().unwrap() = Some(msg);
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        // Clean pass: the tail decoded to EOF, so reset the failure tracker and advance.
        decode_failures = 0;
        failure_len = u64::MAX;
        last_len = cur_len;
        held_tail = tail_pending.then(std::time::Instant::now);
        if new_count > 0 {
            signaled_eof = false;
            current_sleep = std::time::Duration::from_millis(1);
        }
    }
}

/// Upper bound for one encrypted frame (one sealed batch). A larger length
/// prefix means corruption; refusing it avoids a huge bogus allocation.
#[cfg(feature = "encryption")]
const MAX_ENCRYPTED_FRAME_BYTES: u64 = 1 << 30;

/// Decodes a file of `[u64 be length][sealed member]` frames: each frame is
/// decrypted and (if configured) decompressed, and the resulting plaintext is
/// served as one continuous stream. A torn trailing frame surfaces as an
/// `UnexpectedEof` error, which the member consume task treats like a torn
/// compressed member (retried once the writer completes it).
#[cfg(feature = "encryption")]
struct EncryptedFramesReader<R: std::io::Read> {
    inner: R,
    crypto: Arc<Crypto>,
    #[cfg_attr(not(feature = "compression"), allow(dead_code))]
    compression: Compression,
    current: std::io::Cursor<Vec<u8>>,
}

#[cfg(feature = "encryption")]
impl<R: std::io::Read> EncryptedFramesReader<R> {
    fn new(inner: R, crypto: Arc<Crypto>, compression: Compression) -> Self {
        Self {
            inner,
            crypto,
            compression,
            current: std::io::Cursor::new(Vec::new()),
        }
    }

    /// Reads and decodes the next frame. `Ok(false)` = clean end of file.
    fn refill(&mut self) -> std::io::Result<bool> {
        // Read the 8-byte length prefix, distinguishing clean EOF (no bytes at
        // all) from a torn prefix (some bytes, then EOF).
        let mut len_buf = [0u8; 8];
        let mut filled = 0;
        while filled < len_buf.len() {
            let n = self.inner.read(&mut len_buf[filled..])?;
            if n == 0 {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "torn encrypted frame length prefix",
                ));
            }
            filled += n;
        }
        let len = u64::from_be_bytes(len_buf);
        if len > MAX_ENCRYPTED_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("encrypted frame length {len} exceeds the {MAX_ENCRYPTED_FRAME_BYTES} byte cap (corrupt file?)"),
            ));
        }
        let mut sealed = vec![0u8; len as usize];
        self.inner.read_exact(&mut sealed)?;
        let member = self
            .crypto
            .open(&sealed, b"")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        #[cfg(feature = "compression")]
        let member = if self.compression != Compression::None {
            crate::support::compression::decompress_all(self.compression, &member, None)?
        } else {
            member
        };
        // Decryption succeeds regardless of the inner codec, so an unconfigured
        // compression would otherwise be emitted as one binary "message".
        if self.compression == Compression::None {
            if let Some(codec) = compression_magic_name(&member) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "decrypted data begins with a {codec} magic header but no `compression` \
                         is configured; set `compression: {codec}`"
                    ),
                ));
            }
        }
        self.current = std::io::Cursor::new(member);
        Ok(true)
    }
}

#[cfg(feature = "encryption")]
impl<R: std::io::Read> std::io::Read for EncryptedFramesReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.current.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            if !self.refill()? {
                return Ok(0);
            }
        }
    }
}

enum ConsumerBackend {
    EventStore(EventStoreConsumer),
    Tail(FileTailConsumer),
    Queue(FileQueueConsumer),
}

/// Probes a file source path. `Err` for a path that can never yield data;
/// `Ok(Some(reason))` for one that is unopenable now but could still appear
/// (missing file, missing parent directory, not yet readable).
fn probe_source_path(path: &str) -> anyhow::Result<Option<String>> {
    // Checked before opening: Windows refuses to open a directory at all, while
    // on Unix the open succeeds and only the read fails, which the reader threads
    // report as an ordinary end-of-file.
    if std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
            "file source '{path}' is a directory, not a file"
        ))
        .into());
    }
    match std::fs::File::open(path) {
        Ok(_) => Ok(None),
        Err(e) => Ok(Some(format!("cannot open file source '{path}': {e}"))),
    }
}

/// A consumer that reads messages from a file and removes them upon commit.
pub struct FileConsumer {
    backend: ConsumerBackend,
    path: String,
    /// Why the source path could not be opened at construction, if it could not.
    /// A live tail keeps waiting for the file to appear; a drain (`exit_on_empty`)
    /// cannot, so it fails with this instead of blocking forever.
    startup_open_error: Option<String>,
    /// Stamp `mqb.src.file_*` so an idempotent sink can name records in source order.
    source_metadata: bool,
    /// Index of the next record in this file.
    next_record: u64,
    /// Set only for the modes that do not start at byte 0, so their record indexes cannot
    /// collide with a previous run's. `None` for `consume`, whose names must repeat.
    run_epoch: Option<u64>,
    exit_on_empty: bool,
}

/// Hands out a strictly increasing run epoch. Seeded from wall-clock millis so epochs still
/// sort across process restarts, but never repeats or goes backwards within one process —
/// consumers built in the same millisecond, or across a clock step back, stay distinct.
fn next_run_epoch() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let epoch = now.max(last + 1);
        match LAST.compare_exchange_weak(last, epoch, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return epoch,
            Err(observed) => last = observed,
        }
    }
}

impl FileConsumer {
    fn wrap(backend: ConsumerBackend) -> Self {
        Self {
            backend,
            path: String::new(),
            startup_open_error: None,
            source_metadata: false,
            next_record: 0,
            run_epoch: None,
            exit_on_empty: false,
        }
    }

    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, config.source_metadata).await
    }

    /// `source_metadata` is the effective flag: the route enables it for an idempotent
    /// output even when the input config leaves it unset.
    pub async fn new_with_source_metadata(
        config: &FileConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        let startup_open_error = probe_source_path(&config.path)?;
        let mut consumer = Self::new_backend(config).await?;
        consumer.path = config.path.clone();
        consumer.startup_open_error = startup_open_error;
        consumer.source_metadata = source_metadata;
        // `consume` always reads from byte 0, so its record index is reproducible and two
        // runs deliberately produce the same names — that is what makes the sink idempotent.
        // `subscribe` starts at the current end and `group_subscribe` resumes at a stored
        // byte offset, so their index restarts at 0 over records a previous run already
        // numbered. Without an epoch those names would collide and the sink would discard
        // the new records as already-covered; with one they stay distinct and ordered, at
        // the cost of cross-restart deduplication.
        consumer.run_epoch = (source_metadata
            && !matches!(&config.mode, None | Some(FileConsumerMode::Consume { .. })))
        .then(next_run_epoch);
        Ok(consumer)
    }

    async fn new_backend(config: &FileConfig) -> anyhow::Result<Self> {
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let csv = CsvDialect::for_format(&config.format, &config.csv, &delimiter)?;
        csv.check_source()?;
        let format = config.format.clone();
        if matches!(format, FileFormat::Csv)
            && matches!(
                &config.mode,
                Some(FileConsumerMode::Subscribe { delete: true })
            )
        {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "FileFormat::Csv is not supported with Subscribe {{ delete: true }} mode"
            ))
            .into());
        }
        validate_member_settings(config)?;
        if config.compression != Compression::None || config.encryption.is_some() {
            if !matches!(
                &config.mode,
                None | Some(FileConsumerMode::Consume { delete: false })
            ) {
                return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                    "file 'compression'/'encryption' is only supported with the default `consume` mode (no delete, no group_id)"
                ))
                .into());
            }
            // Member-based files (compressed and/or encrypted) have no seekable
            // line offsets, so they use a dedicated reader that decodes from the
            // start of the file.
            #[cfg(any(feature = "compression", feature = "encryption"))]
            return Self::new_member_consumer(config, delimiter, format, csv).await;
        }
        // No codec configured: guard against reading a compressed file as plaintext,
        // which would otherwise split the raw bytes on newlines and emit binary garbage
        // as "messages" under a clean success. A known compressor magic at offset 0 is
        // unambiguous here (a JSON/text member never starts with these bytes).
        if let Some(codec) = sniff_compression_magic(&config.path) {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "file '{}' begins with a {codec} magic header but no `compression` is configured; \
                 set `compression: {codec}` (and any `encryption`) to match how it was written",
                config.path
            ))
            .into());
        }
        // Same guard for encryption: the envelope is behind an 8-byte frame prefix,
        // so a compressor magic never shows up at offset 0 for an encrypted file.
        if looks_encrypted_at_rest(&config.path) {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "file '{}' looks encrypted but no `encryption` is configured; \
                 set `encryption` (and any `compression`) to match how it was written",
                config.path
            ))
            .into());
        }
        match &config.mode {
            None | Some(FileConsumerMode::Consume { delete: false }) => {
                Self::new_tail(&config.path, false, None, delimiter.clone(), format, csv).await
            }
            Some(FileConsumerMode::Subscribe { delete: false }) => {
                Self::new_tail(&config.path, true, None, delimiter.clone(), format, csv).await
            }
            Some(FileConsumerMode::GroupSubscribe {
                group_id,
                read_from_tail,
            }) => {
                let start_at_end = *read_from_tail;
                Self::new_tail(
                    &config.path,
                    start_at_end,
                    Some(group_id.clone()),
                    delimiter.clone(),
                    format,
                    csv,
                )
                .await
            }
            Some(FileConsumerMode::Consume { delete: true }) => {
                let (msg_tx, msg_rx) = async_channel::bounded(100);
                let file_lock = get_file_lock(&config.path);
                let lines_in_memory = Arc::new(AtomicUsize::new(0));
                let extra_lines = ExtraLines::default();
                let extra_lines_clone = extra_lines.clone();
                let kept_header = Arc::new(AtomicUsize::new(0));
                let kept_header_clone = kept_header.clone();
                let ready = Arc::new(AtomicBool::new(false));
                let ready_clone = ready.clone();
                let lines_clone = lines_in_memory.clone();
                let lock_clone = file_lock.clone();
                let runtime = tokio::runtime::Handle::current();
                let path_clone = config.path.clone();

                let delimiter_clone = delimiter.clone();
                let format_clone = format.clone();
                let csv_clone = csv.clone();
                std::thread::spawn(move || {
                    run_file_queue_task(
                        path_clone,
                        msg_tx,
                        lines_clone,
                        lock_clone,
                        runtime,
                        delimiter_clone,
                        format_clone,
                        csv_clone,
                        ready_clone,
                        extra_lines_clone,
                        kept_header_clone,
                    );
                });

                info!(path = %config.path, mode = "queue (delete, optimized)", "File consumer connected");
                Ok(Self::wrap(ConsumerBackend::Queue(FileQueueConsumer {
                    msg_rx,
                    lines_in_memory,
                    extra_lines,
                    kept_header,
                    path: config.path.clone(),
                    file_lock,
                    buffer: Arc::new(Mutex::new(Vec::new())),
                    delimiter,
                    format,
                    csv,
                    ready,
                    pending_eof: false,
                })))
            }
            Some(FileConsumerMode::Subscribe { delete: true }) => {
                let key = format!(
                    "{}|subscribe|delete|{:?}|{:?}",
                    config.path, format, delimiter
                );

                let store = if let Some(store) = {
                    let mut stores = FILE_EVENT_STORES.lock().await;
                    stores.retain(|_, v| v.strong_count() > 0);
                    stores.get(&key).and_then(|w| w.upgrade())
                } {
                    store
                } else {
                    let created =
                        create_file_event_store(&config.path, delimiter.clone(), format).await?;
                    let mut stores = FILE_EVENT_STORES.lock().await;
                    stores
                        .get(&key)
                        .and_then(|w| w.upgrade())
                        .unwrap_or_else(|| {
                            stores.insert(key.clone(), Arc::downgrade(&created));
                            created
                        })
                };

                let subscriber_id = format!("file-sub-{}", fast_uuid_v7::gen_id_str());
                info!(path = %config.path, mode = "subscribe (delete)", subscriber_id = %subscriber_id, "File consumer connected");

                Ok(Self::wrap(ConsumerBackend::EventStore(
                    store.consumer(subscriber_id),
                )))
            }
        }
    }

    /// Consumer for member-based files (compressed and/or encrypted): a
    /// dedicated reader thread decodes the whole stream from the start.
    /// Restricted to the plain consume mode by the validation in `new`.
    #[cfg(any(feature = "compression", feature = "encryption"))]
    async fn new_member_consumer(
        config: &FileConfig,
        delimiter: Vec<u8>,
        format: FileFormat,
        csv: CsvDialect,
    ) -> anyhow::Result<Self> {
        let (msg_tx, msg_rx) = async_channel::bounded(100);
        let ready = Arc::new(AtomicBool::new(false));
        let ready_clone = ready.clone();
        let decode_error: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let decode_error_clone = decode_error.clone();

        let compression = config.compression;
        #[cfg(feature = "encryption")]
        let crypto = config
            .encryption
            .as_ref()
            .map(Crypto::new_at_rest)
            .transpose()?
            .map(Arc::new);
        let make_reader = move |file: std::fs::File| -> Box<dyn std::io::Read> {
            #[cfg(feature = "encryption")]
            if let Some(crypto) = &crypto {
                return Box::new(EncryptedFramesReader::new(
                    std::io::BufReader::new(file),
                    crypto.clone(),
                    compression,
                ));
            }
            #[cfg(feature = "compression")]
            return crate::support::compression::decompress_reader(
                compression,
                std::io::BufReader::new(file),
            );
            #[cfg(not(feature = "compression"))]
            unreachable!("member consumer without compression or encryption")
        };

        let path_clone = config.path.clone();
        let format_clone = format;
        let csv_clone = csv;
        std::thread::spawn(move || {
            run_file_member_consume_task_sync(
                path_clone,
                msg_tx,
                delimiter,
                format_clone,
                csv_clone,
                ready_clone,
                decode_error_clone,
                make_reader,
            );
        });
        info!(path = %config.path, mode = "member consume (compressed/encrypted)", "File consumer connected");
        Ok(Self::wrap(ConsumerBackend::Tail(FileTailConsumer {
            msg_rx,
            buffer: Vec::new(),
            offset_file: None,
            ready,
            pending_eof: false,
            decode_error: Some(decode_error),
            // Member (compressed/encrypted) readers decode framed members, not
            // delimiter-split lines, so the trailing-partial-line rule doesn't apply.
            drain_on_empty: Arc::new(AtomicBool::new(false)),
        })))
    }

    async fn new_tail(
        path: &str,
        start_at_end: bool,
        group_id: Option<String>,
        delimiter: Vec<u8>,
        format: FileFormat,
        csv: CsvDialect,
    ) -> anyhow::Result<Self> {
        let (msg_tx, msg_rx) = async_channel::bounded(100);
        let mut initial_offset = 0;
        let ready = Arc::new(AtomicBool::new(false));
        let ready_clone = ready.clone();
        let drain_on_empty = Arc::new(AtomicBool::new(false));
        let drain_on_empty_clone = drain_on_empty.clone();
        let mut offset_file = None;

        if let Some(gid) = &group_id {
            let offset_path = format!("{}.{}.offset", path, gid);
            if let Ok(content) = tokio::fs::read_to_string(&offset_path).await {
                if let Ok(pos) = content.trim().parse::<u64>() {
                    initial_offset = pos;
                    info!(
                        "Restored offset {} for group {} from {}",
                        pos, gid, offset_path
                    );
                }
            }
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&offset_path)
                .await?;
            offset_file = Some(Arc::new(Mutex::new(file)));
        }

        if initial_offset == 0 && start_at_end {
            if let Ok(metadata) = tokio::fs::metadata(path).await {
                initial_offset = metadata.len();
            }
        }

        let path_clone = path.to_string();
        let format_clone = format;
        let csv_clone = csv;
        let fatal_error: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let fatal_error_clone = fatal_error.clone();
        std::thread::spawn(move || {
            run_file_tail_task_sync(
                path_clone,
                msg_tx,
                initial_offset,
                group_id,
                delimiter,
                format_clone,
                csv_clone,
                ready_clone,
                drain_on_empty_clone,
                fatal_error_clone,
            );
        });

        info!(path = %path, mode = "tail (no-delete, optimized)", "File consumer connected");

        Ok(Self::wrap(ConsumerBackend::Tail(FileTailConsumer {
            msg_rx,
            buffer: Vec::new(),
            offset_file,
            ready,
            pending_eof: false,
            decode_error: Some(fatal_error),
            drain_on_empty,
        })))
    }

    /// Returns true if the consumer is ready to receive messages.
    pub fn is_ready(&self) -> bool {
        match &self.backend {
            ConsumerBackend::EventStore(_) => true,
            ConsumerBackend::Tail(c) => c.ready.load(Ordering::SeqCst),
            ConsumerBackend::Queue(c) => c.ready.load(Ordering::SeqCst),
        }
    }
}

#[async_trait]
impl MessageConsumer for FileConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
        match &mut self.backend {
            // Only the delimiter-splitting tail reader distinguishes a complete final
            // record from a torn mid-write; propagate the drain intent to its thread.
            ConsumerBackend::Tail(c) => c.drain_on_empty.store(exit_on_empty, Ordering::SeqCst),
            // The event-store backend blocks for new events; forward so it can drain too.
            ConsumerBackend::EventStore(c) => c.set_exit_on_empty(exit_on_empty),
            ConsumerBackend::Queue(_) => {}
        }
    }

    // Intentionally keeps the ordered default: the offset-tracking backend commits
    // a cumulative byte offset (the max acked `file_offset`), so out-of-order
    // commits could advance the offset past un-acked messages and lose them.
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.receive_batch_inner(max_messages).await?;
        if self.source_metadata {
            for message in &mut batch.messages {
                message.set_metadata("mqb.src.file_path", self.path.clone());
                message.set_metadata("mqb.src.file_record", self.next_record.to_string());
                if let Some(epoch) = self.run_epoch {
                    message.set_metadata("mqb.src.file_epoch", epoch.to_string());
                }
                self.next_record += 1;
            }
        }
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileConsumer {
    async fn receive_batch_inner(
        &mut self,
        max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        // The path was unopenable at startup. A live tail waits for it (rotation,
        // a writer that hasn't created it yet), but a drain has nothing to wait
        // for, so re-probe and fail rather than block forever on a typo'd path.
        if self.exit_on_empty && self.startup_open_error.is_some() {
            match probe_source_path(&self.path) {
                Ok(None) => self.startup_open_error = None,
                Ok(Some(reason)) => return Err(ConsumerError::Permanent(anyhow::anyhow!(reason))),
                Err(e) => return Err(ConsumerError::Permanent(e)),
            }
        }

        match &mut self.backend {
            ConsumerBackend::EventStore(c) => c.receive_batch(max_messages).await,
            ConsumerBackend::Tail(c) => {
                // A previous greedy fill saw the end-of-file marker trailing the
                // data it returned; surface it now as an empty batch.
                if c.pending_eof {
                    c.pending_eof = false;
                    return Ok(ReceivedBatch {
                        messages: Vec::new(),
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    });
                }

                if c.buffer.is_empty() {
                    match c.msg_rx.recv().await {
                        // An empty batch is the watcher's end-of-file marker; fall
                        // through to return it as an empty batch.
                        Ok(batch) => c.buffer = batch,
                        // Channel closed. For a member reader, a recorded decode
                        // error means the codec/key didn't match — fail the route
                        // rather than masquerade as a clean end-of-stream.
                        Err(_) => {
                            if let Some(reason) = c
                                .decode_error
                                .as_ref()
                                .and_then(|slot| slot.lock().unwrap().take())
                            {
                                return Err(ConsumerError::Permanent(anyhow::anyhow!(reason)));
                            }
                            return Err(ConsumerError::EndOfStream);
                        }
                    }
                }

                // Greedily fill buffer from channel if more messages are available
                while c.buffer.len() < max_messages {
                    match c.msg_rx.try_recv() {
                        // Stop at the end-of-file marker; remember it so the next
                        // call surfaces the empty batch after this data is served.
                        Ok(next_batch) if next_batch.is_empty() => {
                            c.pending_eof = true;
                            break;
                        }
                        Ok(mut next_batch) => c.buffer.append(&mut next_batch),
                        Err(_) => break, // Channel is empty or disconnected
                    }
                }

                let count = std::cmp::min(c.buffer.len(), max_messages);
                let messages: Vec<_> = c.buffer.drain(0..count).collect();

                let commit: crate::traits::BatchCommitFunc =
                    if let Some(offset_file) = &c.offset_file {
                        let offset_file = offset_file.clone();
                        let captured_messages = messages.clone();

                        Box::new(
                            move |dispositions: Vec<crate::traits::MessageDisposition>| {
                                Box::pin(async move {
                                    // The offset stops before the first nack, so a restart
                                    // reads that message again.
                                    let max_offset = dispositions
                                        .iter()
                                        .zip(captured_messages.iter())
                                        .take_while(|(d, _)| {
                                            !matches!(d, crate::traits::MessageDisposition::Nack)
                                        })
                                        .filter_map(|(_, m)| {
                                            m.get_metadata("file_offset")
                                                .and_then(|s| s.parse::<u64>().ok())
                                        })
                                        .max();

                                    if let Some(offset) = max_offset {
                                        // Fixed width, written in place: the file is never
                                        // empty, which would read as offset 0.
                                        let mut file = offset_file.lock().await;
                                        file.rewind().await?;
                                        file.write_all(format!("{offset:020}").as_bytes()).await?;
                                        file.flush().await?;
                                        file.sync_data().await?;
                                    }
                                    Ok(())
                                })
                                    as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                            },
                        )
                    } else {
                        // No-op commit since we are not deleting and no group_id to track
                        Box::new(|_dispositions: Vec<crate::traits::MessageDisposition>| {
                            Box::pin(async move { Ok(()) })
                                as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                        })
                    };

                Ok(ReceivedBatch { messages, commit })
            }
            ConsumerBackend::Queue(c) => {
                // A previous greedy fill saw the watcher's end-of-file marker
                // after data; surface it now as an empty batch.
                if c.pending_eof {
                    c.pending_eof = false;
                    return Ok(ReceivedBatch {
                        messages: Vec::new(),
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    });
                }

                {
                    let buffer = c.buffer.lock().await;
                    if buffer.is_empty() {
                        drop(buffer);
                        match c.msg_rx.recv().await {
                            // An empty batch is the watcher's end-of-file marker;
                            // fall through to return it as an empty batch.
                            Ok(b) => c.buffer.lock().await.extend(b),
                            Err(_) => return Err(ConsumerError::EndOfStream),
                        }
                    }
                }
                let mut buffer = c.buffer.lock().await;

                while buffer.len() < max_messages {
                    match c.msg_rx.try_recv() {
                        // Stop at the end-of-file marker; remember it so the next
                        // call surfaces the empty batch after this data is served.
                        Ok(b) if b.is_empty() => {
                            c.pending_eof = true;
                            break;
                        }
                        Ok(mut b) => buffer.append(&mut b),
                        Err(_) => break,
                    }
                }

                let count = std::cmp::min(buffer.len(), max_messages);
                let batch: Vec<_> = buffer.drain(0..count).collect();
                drop(buffer);

                let path = c.path.clone();
                let lock = c.file_lock.clone();
                let buffer_clone = c.buffer.clone();
                let lines_mem = c.lines_in_memory.clone();
                let extra_lines = c.extra_lines.clone();
                let kept_header = c.kept_header.clone();
                let batch_for_commit = batch.clone();
                let delimiter = c.delimiter.clone();
                let format = c.format.clone();
                let csv = c.csv.clone();

                let commit = Box::new(
                    move |dispositions: Vec<crate::traits::MessageDisposition>| {
                        Box::pin(async move {
                            let mut leading_acks = 0;
                            let mut nacked_msgs = Vec::new();
                            let mut encountered_nack = false;

                            for (i, d) in dispositions.iter().enumerate() {
                                if encountered_nack {
                                    if let Some(msg) = batch_for_commit.get(i) {
                                        nacked_msgs.push(msg.clone());
                                    }
                                    continue;
                                }
                                match d {
                                    crate::traits::MessageDisposition::Ack
                                    | crate::traits::MessageDisposition::Reply(_) => {
                                        leading_acks += 1;
                                    }
                                    crate::traits::MessageDisposition::Nack => {
                                        encountered_nack = true;
                                        if let Some(msg) = batch_for_commit.get(i) {
                                            nacked_msgs.push(msg.clone());
                                        }
                                    }
                                }
                            }

                            if !nacked_msgs.is_empty() {
                                let mut buf = buffer_clone.lock().await;
                                let old_buf = std::mem::take(&mut *buf);
                                let mut new_buf = nacked_msgs;
                                new_buf.extend(old_buf);
                                *buf = new_buf;
                            }

                            if leading_acks > 0 {
                                let lines = {
                                    let mut extra =
                                        extra_lines.lock().unwrap_or_else(|e| e.into_inner());
                                    batch_for_commit[..leading_acks]
                                        .iter()
                                        .map(|m| 1 + extra.remove(&m.message_id).unwrap_or(0))
                                        .sum()
                                };
                                let _guard = lock.lock().await;
                                let keep = kept_header.load(Ordering::SeqCst);
                                if let Err(e) = remove_lines_from_file(
                                    &path, keep, lines, &delimiter, &format, &csv,
                                )
                                .await
                                {
                                    // The lines stay counted, so this run skips them.
                                    tracing::error!("Failed to remove lines from {}: {}", path, e);
                                } else {
                                    lines_mem.fetch_sub(lines, Ordering::SeqCst);
                                }
                            }
                            Ok(())
                        })
                            as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                    },
                );

                Ok(ReceivedBatch {
                    messages: batch,
                    commit,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;
