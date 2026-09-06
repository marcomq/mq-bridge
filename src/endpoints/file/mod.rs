//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::{deserialize_u128, tracing_support::LazyMessageIds};
use crate::event_store::{EventStore, EventStoreConsumer, RetentionPolicy};
use crate::models::{Compression, FileConfig, FileConsumerMode, FileFormat, NameBy};
#[cfg(feature = "encryption")]
use crate::support::crypto::Crypto;
use crate::support::source_ranges::{finalized_name, CoveredRanges};
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
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

/// A sink that writes messages to a file, one per line.
static FILE_LOCKS: Lazy<StdMutex<HashMap<String, Arc<Mutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

fn get_file_lock(path: &str) -> Arc<Mutex<()>> {
    let mut locks = FILE_LOCKS.lock().unwrap();
    locks.retain(|_, v| Arc::strong_count(v) > 1);
    locks
        .entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Appends a CSV-escaped field to `buf` without allocating for the common
/// (no special characters) case. Hot path for CSV row encoding.
fn csv_append_field(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    // One byte pass instead of four `contains` scans.
    if !bytes
        .iter()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'))
    {
        buf.extend_from_slice(bytes);
        return;
    }
    buf.push(b'"');
    for &b in bytes {
        if b == b'"' {
            buf.push(b'"');
        }
        buf.push(b);
    }
    buf.push(b'"');
}

/// Appends `s` to `buf` with JSON string escaping (no surrounding quotes).
///
/// `s` must be valid UTF-8. Only ASCII bytes below 0x20, `"` and `\` need escaping, and
/// no continuation byte of a multi-byte sequence can collide with them, so the scan is
/// byte-wise and every run between escapes is copied in one go. Hot path for decoding
/// CSV rows into JSON objects.
fn json_append_escaped(buf: &mut Vec<u8>, s: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let Some(first) = s.iter().position(|&b| b < 0x20 || b == b'"' || b == b'\\') else {
        buf.extend_from_slice(s);
        return;
    };

    buf.extend_from_slice(&s[..first]);
    let mut run = first;
    for i in first..s.len() {
        let escape: &[u8] = match s[i] {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            b if b < 0x20 => {
                buf.extend_from_slice(&s[run..i]);
                buf.extend_from_slice(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(b >> 4) as usize],
                    HEX[(b & 0x0f) as usize],
                ]);
                run = i + 1;
                continue;
            }
            _ => continue,
        };
        buf.extend_from_slice(&s[run..i]);
        buf.extend_from_slice(escape);
        run = i + 1;
    }
    buf.extend_from_slice(&s[run..]);
}

fn csv_encode_row(fields: &[String]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        csv_append_field(&mut buf, f);
    }
    buf
}

/// Encodes `msg`'s JSON-object payload as a CSV row into `row_buf` (cleared first),
/// establishing the column order from its keys when `hdr` is still unset. Returns
/// `true` when this call established the header, so the caller can emit the header
/// line for a new file. Shared by the plain-append and member (compressed/encrypted)
/// write paths.
fn csv_encode_message(
    msg: &CanonicalMessage,
    hdr: &mut Option<Vec<String>>,
    row_buf: &mut Vec<u8>,
) -> Result<bool, serde_json::Error> {
    // Preferred path: borrow the keys and leave the values as unparsed
    // JSON slices, so a row costs one scan plus byte copies instead of
    // building (and re-serializing) a whole `Value` tree. Payloads with
    // escaped keys can't be borrowed, so those fall back to the tree.
    let raw_row =
        serde_json::from_slice::<HashMap<&str, &serde_json::value::RawValue>>(&msg.payload).ok();
    let parsed_row = match raw_row {
        Some(_) => None,
        None => match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            Ok(serde_json::Value::Object(obj)) => Some(obj),
            _ => None,
        },
    };
    // An object with no fields is rejected too: it carries no columns, so letting it
    // establish the header would fix an empty column set for the rest of the file.
    let no_columns = match (&raw_row, &parsed_row) {
        (Some(raw), _) => raw.is_empty(),
        (_, Some(obj)) => obj.is_empty(),
        _ => true,
    };
    if no_columns {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CSV format requires a non-empty JSON object payload",
        )));
    }

    let mut header_established = false;
    if hdr.is_none() {
        // Sort keys so the column order is deterministic and
        // independent of serde_json's map type (BTreeMap vs the
        // IndexMap enabled by the `preserve_order` feature, which
        // `bson`/mongodb turns on under feature unification).
        let mut cols: Vec<String> = match (&raw_row, &parsed_row) {
            (Some(raw), _) => raw.keys().map(|k| (*k).to_string()).collect(),
            (_, Some(obj)) => obj.keys().cloned().collect(),
            _ => unreachable!(),
        };
        cols.sort();
        *hdr = Some(cols);
        header_established = true;
    }

    let cols = hdr.as_ref().expect("header set above");
    // Reused across the batch: rows are all the same shape, so
    // after the first one this never reallocates.
    row_buf.clear();
    row_buf.reserve(msg.payload.len());
    // Columns this payload actually supplied, for the drift check below.
    let mut matched = 0usize;
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            row_buf.push(b',');
        }
        match (&raw_row, &parsed_row) {
            (Some(raw), _) => {
                if let Some(v) = raw.get(c.as_str()) {
                    csv_append_raw(row_buf, v);
                    matched += 1;
                }
            }
            (_, Some(obj)) => {
                if let Some(v) = obj.get(c) {
                    csv_append_value(row_buf, v);
                    matched += 1;
                }
            }
            _ => unreachable!(),
        }
    }
    if !header_established {
        // Keys the payload has beyond the ones the header covers are dropped silently, and
        // missing ones become empty fields; both mean the file's schema drifted. Counting
        // matches costs nothing extra, and the diagnostic is emitted once per process so a
        // whole drifted stream doesn't flood the log.
        let row_len = match (&raw_row, &parsed_row) {
            (Some(raw), _) => raw.len(),
            (_, Some(obj)) => obj.len(),
            _ => 0,
        };
        if matched < cols.len() || row_len > matched {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                warn!(
                    header_columns = cols.len(),
                    payload_keys = row_len,
                    matched_columns = matched,
                    "CSV payload keys differ from the established header: extra keys are dropped and missing ones written as empty fields. Logged once per process."
                );
            }
        }
    }
    Ok(header_established)
}

/// Appends one still-unparsed JSON value as a CSV field. Scalars are copied
/// straight from the source bytes; only escaped strings and nested
/// arrays/objects need any decoding.
fn csv_append_raw(buf: &mut Vec<u8>, raw: &serde_json::value::RawValue) {
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'"') => {
            let inner = &text[1..text.len() - 1];
            if !inner.as_bytes().contains(&b'\\') {
                csv_append_field(buf, inner);
            } else if let Ok(decoded) = serde_json::from_str::<String>(text) {
                csv_append_field(buf, &decoded);
            }
        }
        // Nested values are re-serialized compactly, matching the parsed path.
        Some(b'{') | Some(b'[') => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                csv_append_field(buf, &v.to_string());
            }
        }
        // Numbers, bools, null: never need quoting, but the scan is one pass anyway.
        _ => csv_append_field(buf, text),
    }
}

/// Appends one JSON value as a CSV field. Numbers, bools and nulls are written
/// straight into `buf` — they can never need CSV quoting, so this skips both the
/// escape scan and the `to_string` allocation that dominates numeric-heavy rows.
fn csv_append_value(buf: &mut Vec<u8>, v: &serde_json::Value) {
    use std::io::Write as _;
    match v {
        serde_json::Value::String(s) => csv_append_field(buf, s),
        serde_json::Value::Number(n) => {
            let _ = write!(buf, "{n}");
        }
        serde_json::Value::Bool(true) => buf.extend_from_slice(b"true"),
        serde_json::Value::Bool(false) => buf.extend_from_slice(b"false"),
        serde_json::Value::Null => buf.extend_from_slice(b"null"),
        // Nested arrays/objects keep their JSON spelling and do need escaping.
        other => csv_append_field(buf, &other.to_string()),
    }
}

/// Parses a single CSV line into fields. Supports quoted fields with escaped `""`.
/// Appends the next CSV field, JSON-escaped and without surrounding quotes, to `out`,
/// and advances `pos` past the field and the delimiter that ended it. Returns `true`
/// when a delimiter was consumed, meaning another field follows.
///
/// Parsing and encoding are fused so a field is walked once and its unescaped runs are
/// copied in bulk: no per-field `String`, and a row costs one allocation (the payload)
/// rather than one per column.
///
/// Quote handling matches [`csv_ends_inside_quotes`] exactly, including its quirk that a
/// quote opens a section only while the field is still empty — so `in"ch` keeps a literal
/// quote and `"a"x` reads as `ax`. The two must never disagree about where a record ends.
///
/// `bytes` must be valid UTF-8.
fn emit_csv_field(out: &mut Vec<u8>, bytes: &[u8], pos: &mut usize) -> bool {
    let mut i = *pos;
    let mut in_quotes = false;
    // Mirrors the source parser's `cur.is_empty()`: whether this field has content yet.
    let mut empty = true;

    while i < bytes.len() {
        let rest = &bytes[i..];
        let found = if in_quotes {
            memchr::memchr(b'"', rest)
        } else {
            memchr::memchr2(b',', b'"', rest)
        };
        let Some(offset) = found else {
            json_append_escaped(out, rest);
            *pos = bytes.len();
            return false;
        };

        if offset > 0 {
            json_append_escaped(out, &rest[..offset]);
            empty = false;
        }
        let at = i + offset;

        if !in_quotes && bytes[at] == b',' {
            *pos = at + 1;
            return true;
        }

        // A quote: closes an open section, escapes itself when doubled inside one,
        // opens a section on an empty field, and is literal data otherwise.
        if in_quotes {
            if bytes.get(at + 1) == Some(&b'"') {
                out.extend_from_slice(b"\\\"");
                empty = false;
                i = at + 2;
                continue;
            }
            in_quotes = false;
        } else if empty {
            in_quotes = true;
        } else {
            out.extend_from_slice(b"\\\"");
        }
        i = at + 1;
    }

    *pos = i;
    false
}

/// The column state one CSV source threads across the records of a file.
///
/// Each column is stored as the bytes a data row writes ahead of its value — the
/// separating comma, the quoted and JSON-escaped column name, and `:"` — so a row emits
/// one slice per column instead of re-escaping every column name on every row.
pub(crate) struct CsvHeader {
    prefixes: Vec<Vec<u8>>,
    /// Combined length of `prefixes`, to size a row's output buffer in one shot.
    prefix_len: usize,
}

impl CsvHeader {
    /// Reads the header record. `bytes` must be valid UTF-8.
    fn parse(bytes: &[u8]) -> Self {
        let mut prefixes: Vec<Vec<u8>> = Vec::new();
        let mut pos = 0;
        loop {
            let mut prefix = Vec::with_capacity(16);
            if !prefixes.is_empty() {
                prefix.push(b',');
            }
            prefix.push(b'"');
            let more = emit_csv_field(&mut prefix, bytes, &mut pos);
            prefix.extend_from_slice(b"\":\"");
            prefixes.push(prefix);
            if !more {
                break;
            }
        }
        let prefix_len = prefixes.iter().map(Vec::len).sum();
        Self {
            prefixes,
            prefix_len,
        }
    }

    /// Encodes one data record as a JSON object of string values.
    ///
    /// Columns the record runs out of values for are emitted empty, and values with no
    /// column to land in are dropped — the same contract the per-field parser had.
    fn encode_row(&self, bytes: &[u8]) -> Vec<u8> {
        // `+ 2` for the braces, `+ prefixes.len()` for each value's closing quote.
        let mut out = Vec::with_capacity(bytes.len() + self.prefix_len + self.prefixes.len() + 2);
        out.push(b'{');
        let mut pos = 0;
        let mut has_more = true;
        for prefix in &self.prefixes {
            out.extend_from_slice(prefix);
            if has_more {
                has_more = emit_csv_field(&mut out, bytes, &mut pos);
            }
            out.push(b'"');
        }
        out.push(b'}');

        if has_more {
            self.warn_extra_fields(bytes, pos);
        }
        out
    }

    /// Extra fields have no column to land in, so they are dropped. Say so once: the row
    /// still copies under a clean success. Walks the leftovers with the same parser so the
    /// reported count cannot drift from what was actually skipped.
    #[cold]
    fn warn_extra_fields(&self, bytes: &[u8], mut pos: usize) {
        let mut scratch = Vec::new();
        let mut fields = self.prefixes.len();
        loop {
            scratch.clear();
            fields += 1;
            if !emit_csv_field(&mut scratch, bytes, &mut pos) {
                break;
            }
        }

        static WARNED: AtomicBool = AtomicBool::new(false);
        const MSG: &str = "CSV row has more fields than the header has \
                           columns; the extras are dropped. Further \
                           occurrences are logged at debug level.";
        let columns = self.prefixes.len();
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(columns, fields, "{MSG}");
        } else {
            tracing::debug!(columns, fields, "{MSG}");
        }
    }
}

pub(crate) fn parse_delimiter(delimiter: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let bytes = match delimiter {
        Some(s) if s.starts_with("0x") => {
            let hex = s.trim_start_matches("0x");
            if hex.len() != 2 {
                return Err(anyhow::anyhow!(
                    "Hex delimiter must be 1 byte (2 hex chars)"
                ));
            }
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|e| anyhow::anyhow!("Invalid hex delimiter: {}", e))?
        }
        Some(s) => s.as_bytes().to_vec(),
        None => vec![b'\n'],
    };

    if bytes.is_empty() {
        return Err(anyhow::anyhow!("Delimiter cannot be empty"));
    }

    Ok(bytes)
}

/// True when `buf` stops inside an open quote, so the delimiter it ended on was field
/// data: RFC 4180 lets a quoted field contain the record separator. Mirrors the quote
/// handling in [`parse_csv_row`], down to only opening a quote at the start of a field,
/// so the splitter and the parser can never disagree about where a record ends.
fn csv_ends_inside_quotes(buf: &[u8]) -> bool {
    let mut in_quotes = false;
    let mut field_is_empty = true;
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if in_quotes {
            if b == b'"' {
                if buf.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    in_quotes = false;
                }
            }
        } else if b == b'"' && field_is_empty {
            in_quotes = true;
        } else if b == b',' {
            field_is_empty = true;
            i += 1;
            continue;
        } else {
            field_is_empty = false;
        }
        i += 1;
    }
    in_quotes
}

/// Reads one *record*, which for CSV may span several delimiters. Every read loop and
/// every record count goes through this, so a multi-line row stays one record everywhere
/// — including the `lines_in_memory` bookkeeping that consume mode truncates the file by.
async fn read_record<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    delimiter: &[u8],
    format: &FileFormat,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut total = read_until_bytes(reader, delimiter, buf).await?;
    if !matches!(format, FileFormat::Csv) {
        return Ok(total);
    }
    while total > 0 && csv_ends_inside_quotes(buf) {
        let n = read_until_bytes(reader, delimiter, buf).await?;
        if n == 0 {
            break; // Unterminated quote at EOF: emit what there is rather than hang.
        }
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
}

/// Validates the `compression`/`encryption` settings shared by the file
/// publisher and consumer: both need their Cargo feature enabled.
fn validate_member_settings(config: &FileConfig) -> anyhow::Result<()> {
    // Only the feature-gated checks below read it, so it is unused with both features on.
    let _ = config;
    #[cfg(not(feature = "compression"))]
    if config.compression != Compression::None {
        return Err(anyhow::anyhow!(
            "file 'compression' requires the `compression` feature"
        ));
    }
    #[cfg(not(feature = "encryption"))]
    if config.encryption.is_some() {
        return Err(anyhow::anyhow!(
            "file 'encryption' requires the `encryption` feature"
        ));
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
    // Best effort: Windows cannot open a directory handle this way.
    if let Ok(directory) = File::open(directory).await {
        let _ = directory.sync_all().await;
    }
    Ok(())
}

impl FilePublisher {
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
                return Err(anyhow::anyhow!(
                    "file 'name_by: source_position' does not support CSV (per-part headers are unimplemented)"
                ));
            }
            tokio::fs::create_dir_all(path).await.with_context(|| {
                format!("Failed to create part-file sink directory: {path_str}")
            })?;
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create parent directory for file: {:?}", parent)
            })?;
        }

        if !by_source_position {
            let _ = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .with_context(|| {
                    format!("Failed to open or create file for writing: {}", path_str)
                })?;
        }

        let file_lock = get_file_lock(path_str);
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let format = config.format.clone();
        // Part names must advertise what the bytes actually are: a compressed or sealed part
        // holds one member, so it earns the same suffixes the appending sink's file would.
        let mut part_extension = match format {
            FileFormat::Csv => "csv",
            FileFormat::Raw => "bin",
            FileFormat::Normal | FileFormat::Json | FileFormat::Text => "jsonl",
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
        info!(path = %path_str, format = ?format, "File sink opened for appending");
        Ok(Self {
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
        })
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
                    let bytes = encode_record(&message, &self.format)
                        .map_err(|error| PublisherError::NonRetryable(anyhow::anyhow!(error)))?;
                    body.extend_from_slice(&bytes);
                    body.extend_from_slice(&self.delimiter);
                }
                let body = self.encode_member(body)?;
                write_finalized_file(&final_path, &body).await?;
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
        let mut wrote_csv_header = false;
        let mut csv_row_buf: Vec<u8> = Vec::new();
        for mut msg in messages {
            msg.strip_source_metadata();
            // `Ok(None)` means the body is in the reused CSV row buffer.
            let encoded = match self.format {
                FileFormat::Csv => {
                    let hdr = csv_header_guard.as_mut().expect("csv header lock held");
                    match csv_encode_message(&msg, hdr, &mut csv_row_buf) {
                        Ok(header_established) => {
                            if header_established && file_is_empty {
                                raw.extend_from_slice(&csv_encode_row(
                                    hdr.as_ref().expect("header set above"),
                                ));
                                raw.extend_from_slice(&self.delimiter);
                                wrote_csv_header = true;
                            }
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }
                }
                ref fmt => encode_record(&msg, fmt).map(Some),
            };
            match encoded {
                Ok(Some(bytes)) => {
                    raw.extend_from_slice(&bytes);
                    raw.extend_from_slice(&self.delimiter);
                }
                Ok(None) => {
                    raw.extend_from_slice(&csv_row_buf);
                    raw.extend_from_slice(&self.delimiter);
                }
                Err(e) => {
                    tracing::error!("Failed to serialize message for file sink member: {}", e);
                    failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
                }
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

        if failed_messages.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
            })
        }
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
            // Carried out of the match so the `hdr` borrow ends before the write,
            // which needs the guard again for the rollback path.
            let mut csv_header_line: Option<Vec<u8>> = None;
            let serialized_msg = match self.format {
                FileFormat::Csv => {
                    let hdr = csv_header_guard.as_mut().expect("csv header lock held");
                    match csv_encode_message(&msg, hdr, &mut csv_row_buf) {
                        Ok(header_established) => {
                            if header_established && file_is_empty {
                                let mut line =
                                    csv_encode_row(hdr.as_ref().expect("header set above"));
                                line.extend_from_slice(&self.delimiter);
                                csv_header_line = Some(line);
                            }
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }
                }
                ref fmt => encode_record(&msg, fmt).map(Some),
            };
            if let Some(line) = csv_header_line {
                // Set before the write: the header is established in memory either way, so a
                // rollback has to clear it even when the write itself failed.
                wrote_csv_header = true;
                if let Err(e) = writer.write_all(&line).await {
                    roll_back_partial_batch(
                        &writer,
                        pre_len,
                        wrote_csv_header,
                        csv_header_guard.as_mut(),
                    )
                    .await;
                    return Err(PublisherError::Retryable(anyhow::anyhow!(e)));
                }
            }
            let serialized_msg = match serialized_msg {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to serialize message for file sink: {}", e);
                    failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
                    continue;
                }
            };

            // Write body + delimiter as one contiguous buffer so a concurrent
            // tailing reader never observes the record without its delimiter
            // (shrinks the torn-write window; the reader also guards against it).
            // `None` means the body is already in the reused CSV row buffer.
            let record: &[u8] = match serialized_msg {
                Some(body) => {
                    record_buf.clear();
                    record_buf.extend_from_slice(&body);
                    record_buf.extend_from_slice(&self.delimiter);
                    &record_buf
                }
                None => {
                    csv_row_buf.extend_from_slice(&self.delimiter);
                    &csv_row_buf
                }
            };
            if let Err(e) = writer.write_all(record).await {
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
        if failed_messages.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
            })
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
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

    let retention = RetentionPolicy {
        gc_interval: std::time::Duration::ZERO,
        ..Default::default()
    };
    // Use immediate GC for file stores to ensure files are truncated promptly on ack.

    // 1. Create EventStore with on_drop callback
    let store = Arc::new(
        EventStore::new(retention).with_drop_callback(move |events| {
            // In EventStore mode (Subscribe + Delete), we always delete.
            let count = events.len();
            if count == 0 {
                return;
            }
            let state = feed_state_clone.clone();
            let path = path_clone.clone();
            let file_op_lock = file_op_lock_clone.clone();
            let delimiter = delimiter_clone.clone();
            let format = format_gc.clone();

            tokio::spawn(async move {
                // Serialize file operations to prevent race conditions between multiple GCs
                let _guard = file_op_lock.lock().await;

                {
                    let mut s = state.lock().await;
                    s.lines_in_memory = s.lines_in_memory.saturating_sub(count);
                }

                if let Err(e) = remove_lines_from_file(&path, count, &delimiter, &format).await {
                    tracing::error!("Failed to remove lines from file {}: {}", path, e);
                    // Note: In this simplified model, if deletion fails, lines_in_memory
                    // might become out of sync, leading to reprocessing on restart.
                } else {
                    trace!("Removed {} lines from {}", count, path);
                }
            });
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
            let mut file = match file_res {
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
                match read_record(&mut reader, &delimiter, &format_clone, &mut buf).await {
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
            file = reader.into_inner();

            // Release file op lock to allow publisher to write while we read
            drop(file_guard);

            // Read new lines
            let mut reader = BufReader::new(file);
            let mut lines_read = 0;
            let mut batch = Vec::with_capacity(128);

            loop {
                let mut buffer = Vec::new();
                match read_record(&mut reader, &delimiter, &format_clone, &mut buffer).await {
                    Ok(0) => break,
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

/// Drops the first `count` *records* from `path`. `format` matters: a CSV record can span
/// several delimiters, and consuming the wrong number of them would corrupt the remainder.
async fn remove_lines_from_file(
    path: &str,
    count: usize,
    delimiter: &[u8],
    format: &FileFormat,
) -> anyhow::Result<()> {
    let unique_id = fast_uuid_v7::gen_id_str();
    let temp_path = format!("{}.{}.tmp", path, unique_id);

    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let temp_file = File::create(&temp_path).await?;
    let mut writer = BufWriter::new(temp_file);

    let mut lines_skipped = 0;
    while lines_skipped < count {
        let mut buf = Vec::new();
        if read_record(&mut reader, delimiter, format, &mut buf).await? == 0 {
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
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut total = read_until_bytes_sync(reader, delimiter, buf)?;
    if !matches!(format, FileFormat::Csv) {
        return Ok(total);
    }
    while total > 0 && csv_ends_inside_quotes(buf) {
        let n = read_until_bytes_sync(reader, delimiter, buf)?;
        if n == 0 {
            break;
        }
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
    ready: Arc<AtomicBool>,
    drain_on_empty: Arc<AtomicBool>,
) {
    let mut last_position: u64 = initial_offset;
    let mut reader: Option<std::io::BufReader<std::fs::File>> = None;
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(50);
    let mut initialized = false;
    // Tracks whether we've already emitted the empty end-of-file marker for the
    // current drained state, so we signal it once per EOF transition rather than
    // on every idle poll.
    let mut signaled_eof = false;
    const BATCH_SIZE: usize = 1024;
    let mut buf = Vec::with_capacity(1024);
    let mut records_buf: Vec<u8> = Vec::with_capacity(128 * BATCH_SIZE);
    let mut spans: Vec<RecordSpan> = Vec::with_capacity(BATCH_SIZE);
    let mut csv_header: Option<Arc<CsvHeader>> = None;

    loop {
        if reader.is_none() {
            let mut file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            if let Ok(metadata) = file.metadata() {
                if metadata.len() < last_position {
                    tracing::warn!("File {} was truncated. Resetting position to 0.", path);
                    last_position = 0;
                }
            }

            if let Err(e) = file.seek(std::io::SeekFrom::Start(last_position)) {
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

        if let Some(r) = reader.as_mut() {
            for _ in 0..BATCH_SIZE {
                buf.clear();
                match read_record_sync(r, &delimiter, &format, &mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if !buf.ends_with(&delimiter) {
                            if drain_on_empty.load(Ordering::SeqCst) {
                                // Drain mode (exit_on_empty): the file is complete, so a
                                // final record with no trailing delimiter is a whole
                                // record. Emit it once, advancing past it; the next read
                                // returns 0 (EOF) and the empty marker fires normally.
                                last_position += n as u64;
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

struct FileQueueConsumer {
    msg_rx: async_channel::Receiver<Vec<CanonicalMessage>>,
    lines_in_memory: Arc<AtomicUsize>,
    path: String,
    file_lock: Arc<Mutex<()>>,
    buffer: Arc<Mutex<Vec<CanonicalMessage>>>,
    delimiter: Vec<u8>,
    /// Needed on the commit path too: deleting acked records re-splits the file, and CSV
    /// records do not map one-to-one onto delimiters.
    format: FileFormat,
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
    ready: Arc<AtomicBool>,
) {
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
    let mut initialized = false;
    // Emit the empty end-of-file marker once per drained state; see the tail task.
    let mut signaled_eof = false;
    let mut buf = Vec::new();
    let mut csv_header: Option<CsvHeader> = None;

    loop {
        buf.clear();
        let mut batch = Vec::with_capacity(128);
        let mut lines_read = 0;

        {
            let _guard = runtime_handle.block_on(file_lock.lock());
            let skip_count = lines_in_memory.load(Ordering::SeqCst);

            let file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    drop(_guard);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let mut reader = std::io::BufReader::new(file);
            let mut skipped = 0;
            let mut error = false;

            while skipped < skip_count {
                buf.clear();
                match read_record_sync(&mut reader, &delimiter, &format, &mut buf) {
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
                for _ in 0..128 {
                    buf.clear();
                    match read_record_sync(&mut reader, &delimiter, &format, &mut buf) {
                        Ok(0) => break,
                        Ok(_) => {
                            if buf.ends_with(&delimiter) {
                                buf.truncate(buf.len() - delimiter.len());
                            }
                            if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r")
                            {
                                buf.pop();
                            }
                            match parse_message(&buf, &format, &mut csv_header) {
                                Some(msg) => {
                                    batch.push(msg);
                                    lines_read += 1;
                                }
                                None => {
                                    // CSV header line: remove it immediately so it never
                                    // occupies a slot in the ack/delete line accounting.
                                    if let Err(e) = runtime_handle.block_on(remove_lines_from_file(
                                        &path, 1, &delimiter, &format,
                                    )) {
                                        tracing::error!(
                                            "Failed to remove CSV header line from {}: {}",
                                            path,
                                            e
                                        );
                                    }
                                }
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

        if lines_read > 0 {
            lines_in_memory.fetch_add(lines_read, Ordering::SeqCst);
            if msg_tx.send_blocking(batch).is_err() {
                break;
            }
            current_sleep = std::time::Duration::from_millis(1);
            signaled_eof = false; // data flowed; re-arm the EOF marker
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
fn run_file_member_consume_task_sync<F>(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    delimiter: Vec<u8>,
    format: FileFormat,
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
        }

        // No growth since the last full pass: emit the drain marker once, then poll.
        if initialized && cur_len == last_len {
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

        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to open {}: {}", path, e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };
        let mut reader = std::io::BufReader::new(make_reader(file));

        // Skip records emitted on a previous pass (file re-read from the start).
        let mut csv_header: Option<CsvHeader> = None;
        let mut skipped = 0;
        let mut decode_error = false;
        while skipped < records_emitted {
            buf.clear();
            match read_record_sync(&mut reader, &delimiter, &format, &mut buf) {
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
        let mut batch = Vec::with_capacity(256);
        loop {
            buf.clear();
            match read_record_sync(&mut reader, &delimiter, &format, &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if !buf.ends_with(&delimiter) {
                        // Torn final member (writer mid-append): don't emit or count
                        // it; it completes and grows the file on a later poll.
                        break;
                    }
                    buf.truncate(buf.len() - delimiter.len());
                    if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r") {
                        buf.pop();
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
        anyhow::bail!("file source '{path}' is a directory, not a file");
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
        let format = config.format.clone();
        if matches!(format, FileFormat::Csv)
            && matches!(
                &config.mode,
                Some(FileConsumerMode::Subscribe { delete: true })
            )
        {
            return Err(anyhow::anyhow!(
                "FileFormat::Csv is not supported with Subscribe {{ delete: true }} mode"
            ));
        }
        validate_member_settings(config)?;
        if config.compression != Compression::None || config.encryption.is_some() {
            if !matches!(
                &config.mode,
                None | Some(FileConsumerMode::Consume { delete: false })
            ) {
                return Err(anyhow::anyhow!(
                    "file 'compression'/'encryption' is only supported with the default `consume` mode (no delete, no group_id)"
                ));
            }
            // Member-based files (compressed and/or encrypted) have no seekable
            // line offsets, so they use a dedicated reader that decodes from the
            // start of the file.
            #[cfg(any(feature = "compression", feature = "encryption"))]
            return Self::new_member_consumer(config, delimiter, format).await;
        }
        // No codec configured: guard against reading a compressed file as plaintext,
        // which would otherwise split the raw bytes on newlines and emit binary garbage
        // as "messages" under a clean success. A known compressor magic at offset 0 is
        // unambiguous here (a JSON/text member never starts with these bytes).
        if let Some(codec) = sniff_compression_magic(&config.path) {
            return Err(anyhow::anyhow!(
                "file '{}' begins with a {codec} magic header but no `compression` is configured; \
                 set `compression: {codec}` (and any `encryption`) to match how it was written",
                config.path
            ));
        }
        // Same guard for encryption: the envelope is behind an 8-byte frame prefix,
        // so a compressor magic never shows up at offset 0 for an encrypted file.
        if looks_encrypted_at_rest(&config.path) {
            return Err(anyhow::anyhow!(
                "file '{}' looks encrypted but no `encryption` is configured; \
                 set `encryption` (and any `compression`) to match how it was written",
                config.path
            ));
        }
        match &config.mode {
            None | Some(FileConsumerMode::Consume { delete: false }) => {
                Self::new_tail(&config.path, false, None, delimiter.clone(), format).await
            }
            Some(FileConsumerMode::Subscribe { delete: false }) => {
                Self::new_tail(&config.path, true, None, delimiter.clone(), format).await
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
                )
                .await
            }
            Some(FileConsumerMode::Consume { delete: true }) => {
                let (msg_tx, msg_rx) = async_channel::bounded(100);
                let file_lock = get_file_lock(&config.path);
                let lines_in_memory = Arc::new(AtomicUsize::new(0));
                let ready = Arc::new(AtomicBool::new(false));
                let ready_clone = ready.clone();
                let lines_clone = lines_in_memory.clone();
                let lock_clone = file_lock.clone();
                let runtime = tokio::runtime::Handle::current();
                let path_clone = config.path.clone();

                let delimiter_clone = delimiter.clone();
                let format_clone = format.clone();
                std::thread::spawn(move || {
                    run_file_queue_task(
                        path_clone,
                        msg_tx,
                        lines_clone,
                        lock_clone,
                        runtime,
                        delimiter_clone,
                        format_clone,
                        ready_clone,
                    );
                });

                info!(path = %config.path, mode = "queue (delete, optimized)", "File consumer connected");
                Ok(Self::wrap(ConsumerBackend::Queue(FileQueueConsumer {
                    msg_rx,
                    lines_in_memory,
                    path: config.path.clone(),
                    file_lock,
                    buffer: Arc::new(Mutex::new(Vec::new())),
                    delimiter,
                    format,
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
                    let store = stores
                        .get(&key)
                        .and_then(|w| w.upgrade())
                        .unwrap_or_else(|| {
                            stores.insert(key.clone(), Arc::downgrade(&created));
                            created
                        });
                    store
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
        std::thread::spawn(move || {
            run_file_member_consume_task_sync(
                path_clone,
                msg_tx,
                delimiter,
                format_clone,
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
        std::thread::spawn(move || {
            run_file_tail_task_sync(
                path_clone,
                msg_tx,
                initial_offset,
                group_id,
                delimiter,
                format_clone,
                ready_clone,
                drain_on_empty_clone,
            );
        });

        info!(path = %path, mode = "tail (no-delete, optimized)", "File consumer connected");

        Ok(Self::wrap(ConsumerBackend::Tail(FileTailConsumer {
            msg_rx,
            buffer: Vec::new(),
            offset_file,
            ready,
            pending_eof: false,
            decode_error: None,
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
                message
                    .metadata
                    .insert("mqb.src.file_path".to_string(), self.path.clone());
                message.metadata.insert(
                    "mqb.src.file_record".to_string(),
                    self.next_record.to_string(),
                );
                if let Some(epoch) = self.run_epoch {
                    message
                        .metadata
                        .insert("mqb.src.file_epoch".to_string(), epoch.to_string());
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

                let commit: crate::traits::BatchCommitFunc = if let Some(offset_file) =
                    &c.offset_file
                {
                    let offset_file = offset_file.clone();
                    let captured_messages = messages.clone();

                    Box::new(
                        move |dispositions: Vec<crate::traits::MessageDisposition>| {
                            Box::pin(async move {
                                let max_offset = dispositions
                                    .iter()
                                    .zip(captured_messages.iter())
                                    .filter_map(|(d, m)| match d {
                                        crate::traits::MessageDisposition::Ack
                                        | crate::traits::MessageDisposition::Reply(_) => m
                                            .metadata
                                            .get("file_offset")
                                            .and_then(|s| s.parse::<u64>().ok()),
                                        _ => None,
                                    })
                                    .max();

                                if let Some(offset) = max_offset {
                                    let mut file = offset_file.lock().await;
                                    if let Err(e) = file.rewind().await {
                                        tracing::error!("Failed to rewind offset file: {}", e);
                                    } else if let Err(e) = file.set_len(0).await {
                                        tracing::error!("Failed to truncate offset file: {}", e);
                                    } else if let Err(e) =
                                        file.write_all(offset.to_string().as_bytes()).await
                                    {
                                        tracing::error!("Failed to write offset file: {}", e);
                                    } else if let Err(e) = file.flush().await {
                                        tracing::error!("Failed to flush offset file: {}", e);
                                    }
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
                let batch_for_commit = batch.clone();
                let delimiter = c.delimiter.clone();
                let format = c.format.clone();

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
                                let _guard = lock.lock().await;
                                if let Err(e) =
                                    remove_lines_from_file(&path, leading_acks, &delimiter, &format)
                                        .await
                                {
                                    tracing::error!("Failed to remove lines from {}: {}", path, e);
                                }
                                lines_mem.fetch_sub(leading_acks, Ordering::SeqCst);
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

/// Wraps a message body for the Json/Text file formats. Generic over the payload type
/// so both formats share one struct while keeping the message_id serializer and field
/// layout (and thus the on-disk output) identical.
#[derive(serde::Serialize)]
struct RecordWrapper<'a, P: serde::Serialize> {
    #[serde(serialize_with = "crate::canonical_message::print_uuidv7")]
    message_id: u128,
    payload: P,
    metadata: &'a HashMap<String, String>,
}

/// Encodes a single message body for a non-CSV [`FileFormat`] (Raw/Normal/Json/Text).
/// Shared by the file sink and the object-store sink. CSV needs cross-record header
/// state, so it is handled inline by the file sink and rejected by the object sink.
pub(crate) fn encode_record(
    msg: &CanonicalMessage,
    format: &FileFormat,
) -> Result<Bytes, serde_json::Error> {
    match format {
        // `Bytes` is refcounted, so a verbatim copy costs no allocation and no memcpy;
        // the caller's append into the batch buffer is then the only copy of the payload.
        FileFormat::Raw => Ok(msg.payload.clone()),
        // The sink format decides the encoding, not the message's origin: `normal`
        // always writes the wrapper so `message_id` and metadata survive the round
        // trip. Use `format: raw` for verbatim, unwrapped copies.
        FileFormat::Normal => serde_json::to_vec(msg).map(Bytes::from),
        // Parsing only establishes that the payload is JSON; its own bytes are what
        // gets written. A `serde_json::Value` in between would re-sort the object's
        // keys and reformat its numbers, which the reader then cannot undo.
        FileFormat::Json => {
            if let Ok(raw) = serde_json::from_slice::<&serde_json::value::RawValue>(&msg.payload) {
                serde_json::to_vec(&RecordWrapper {
                    message_id: msg.message_id,
                    payload: raw,
                    metadata: &msg.metadata,
                })
                .map(Bytes::from)
            } else {
                encode_byte_payload(msg)
            }
        }
        FileFormat::Text => {
            if let Ok(text) = std::str::from_utf8(&msg.payload) {
                serde_json::to_vec(&RecordWrapper {
                    message_id: msg.message_id,
                    payload: text,
                    metadata: &msg.metadata,
                })
                .map(Bytes::from)
            } else {
                encode_byte_payload(msg)
            }
        }
        FileFormat::Csv => unreachable!("CSV is encoded by the caller, not encode_record"),
    }
}

/// Marks a record whose payload is neither JSON nor UTF-8 and was therefore written as a
/// byte array. Without it a `json` reader hands the next hop the *textual* array
/// `[40,181,47,…]`, which breaks any binary payload (compression, encryption).
pub(crate) const BYTE_PAYLOAD_KEY: &str = "mq_bridge.payload_bytes";
/// The only value this crate writes for [`BYTE_PAYLOAD_KEY`]. A reader honours the marker
/// only at this exact value, so an unrelated key of the same name cannot redirect decoding.
const BYTE_PAYLOAD_MARK: &str = "1";

/// Write a binary payload under a `json`/`text` sink as the byte-array wrapper, marked so
/// the reader restores the bytes instead of the array's JSON text.
fn encode_byte_payload(msg: &CanonicalMessage) -> Result<Bytes, serde_json::Error> {
    let mut metadata = msg.metadata.clone();
    metadata.insert(BYTE_PAYLOAD_KEY.to_string(), BYTE_PAYLOAD_MARK.to_string());
    serde_json::to_vec(&RecordWrapper {
        message_id: msg.message_id,
        payload: &msg.payload,
        metadata: &metadata,
    })
    .map(Bytes::from)
}

/// Parses one file line into a message. Returns `None` for CSV header lines,
/// which establish the schema but carry no data of their own.
/// One buffered record: its span in the batch buffer, and the file offset just past it.
pub(crate) type RecordSpan = (usize, usize, u64);

/// Decodes one buffered record, stamping the file offset when the source tracks them.
fn decode_one(
    buf: &[u8],
    &(start, end, position): &RecordSpan,
    format: &FileFormat,
    header: Option<&CsvHeader>,
    with_offset: bool,
) -> Option<CanonicalMessage> {
    let bytes = &buf[start..end];
    // `Some` only for CSV, where it makes the decode a pure function of the header;
    // every other format ignores the header slot entirely.
    let mut msg = match header {
        Some(header) => decode_csv_row(header, bytes),
        None => parse_message(bytes, format, &mut None)?,
    };
    if with_offset {
        msg.metadata
            .insert("file_offset".to_string(), position.to_string());
    }
    Some(msg)
}

/// Decodes a batch of already-delimited records, splitting the work across cores.
///
/// Reading a file is one thread's job, but decoding its records is not: each record is
/// independent once the CSV header is known, so the reader buffers a whole batch and
/// hands most of it to the shared decode pool. The header, when still unseen, is read
/// here first — every later row depends on it.
///
/// `buf` is taken and handed back so its allocation survives the batch. Records come
/// back in file order, so a parallel decode is indistinguishable from a sequential one.
pub(crate) fn decode_records(
    buf: &mut Vec<u8>,
    spans: &[RecordSpan],
    format: &FileFormat,
    csv_header: &mut Option<Arc<CsvHeader>>,
    with_offset: bool,
) -> Vec<CanonicalMessage> {
    let mut spans = spans;
    let mut out = Vec::with_capacity(spans.len());

    if matches!(format, FileFormat::Csv) && csv_header.is_none() {
        let Some((&(start, end, _), rest)) = spans.split_first() else {
            return out;
        };
        *csv_header = Some(Arc::new(CsvHeader::parse(
            String::from_utf8_lossy(&buf[start..end]).as_bytes(),
        )));
        spans = rest;
    }

    let chunks = crate::support::parallel::decode_chunk_count(spans.len());
    if chunks <= 1 {
        let header = csv_header.as_deref();
        out.extend(
            spans
                .iter()
                .filter_map(|span| decode_one(buf, span, format, header, with_offset)),
        );
        return out;
    }

    // Workers need the buffer for as long as they run, so it is shared for the batch and
    // its allocation reclaimed below.
    let shared = Arc::new(std::mem::take(buf));
    let per_chunk = spans.len().div_ceil(chunks);
    let mut parts = spans.chunks(per_chunk);
    // The first chunk stays here: one fewer hand-off, and this thread would otherwise
    // just block waiting for the others.
    let mine = parts.next().unwrap_or(&[]);

    let (tx, rx) = std::sync::mpsc::channel();
    let mut queued = 0;
    for (index, part) in parts.enumerate() {
        let buf = Arc::clone(&shared);
        let header = csv_header.clone();
        let format = format.clone();
        let part = part.to_vec();
        let tx = tx.clone();
        crate::support::parallel::pool().submit(Box::new(move || {
            // Caught here so one bad record cannot take a shared worker down with it;
            // re-raised on the reader thread below.
            let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let header = header.as_deref();
                part.iter()
                    .filter_map(|span| decode_one(&buf, span, &format, header, with_offset))
                    .collect::<Vec<_>>()
            }));
            // Released before the result is sent: the reader reclaims the buffer as soon
            // as it has every chunk, and only the last live clone lets it.
            drop(buf);
            let _ = tx.send((index, decoded));
        }));
        queued += 1;
    }
    drop(tx);

    let header = csv_header.as_deref();
    out.extend(
        mine.iter()
            .filter_map(|span| decode_one(&shared, span, format, header, with_offset)),
    );

    let mut decoded: Vec<Option<Vec<CanonicalMessage>>> = (0..queued).map(|_| None).collect();
    for _ in 0..queued {
        match rx.recv() {
            Ok((index, Ok(part))) => decoded[index] = Some(part),
            Ok((_, Err(panic))) => std::panic::resume_unwind(panic),
            // Every job sends before dropping its sender, so this cannot happen.
            Err(_) => unreachable!("decode worker dropped its sender without sending"),
        }
    }
    for part in decoded.into_iter().flatten() {
        out.extend(part);
    }

    *buf = Arc::try_unwrap(shared).unwrap_or_default();
    out
}

/// Decodes one CSV record against an established header.
///
/// Validated once per record so the field walk can stay byte-wise, and lossy so an
/// ill-encoded source still yields a valid JSON string.
fn decode_csv_row(header: &CsvHeader, buffer: &[u8]) -> CanonicalMessage {
    let line = String::from_utf8_lossy(buffer);
    CanonicalMessage::new(header.encode_row(line.as_bytes()), None)
}

pub(crate) fn parse_message(
    buffer: &[u8],
    format: &FileFormat,
    csv_header: &mut Option<CsvHeader>,
) -> Option<CanonicalMessage> {
    match format {
        FileFormat::Csv => match csv_header {
            None => {
                *csv_header = Some(CsvHeader::parse(String::from_utf8_lossy(buffer).as_bytes()));
                None
            }
            Some(header) => Some(decode_csv_row(header, buffer)),
        },
        FileFormat::Raw => {
            let mut msg = CanonicalMessage::new(buffer.to_vec(), None);
            msg.metadata
                .insert("mq_bridge.original_format".to_string(), "raw".to_string());
            Some(msg)
        }
        // `json` keeps the payload as JSON, so its bytes are copied out of the line
        // verbatim. A `serde_json::Value` round trip would sort the object's keys and
        // reformat its numbers, neither of which the payload asked for.
        FileFormat::Json => {
            #[derive(serde::Deserialize)]
            struct AnyPayloadMessage<'a> {
                #[serde(deserialize_with = "deserialize_u128")]
                message_id: u128,
                #[serde(default, borrow)]
                payload: MaybePayload<&'a serde_json::value::RawValue>,
                #[serde(default)]
                payload_base64: Option<String>,
                #[serde(default)]
                metadata: HashMap<String, String>,
            }

            let msg = match serde_json::from_slice::<AnyPayloadMessage>(buffer) {
                Ok(wrapper) => {
                    let AnyPayloadMessage {
                        message_id,
                        payload,
                        payload_base64,
                        mut metadata,
                    } = wrapper;
                    match (payload.into_option(), payload_base64) {
                        // A `normal`-written binary record read back through a `json` source.
                        (None, Some(b64)) => match decode_base64_payload(&b64) {
                            Some(payload) => CanonicalMessage {
                                message_id,
                                payload,
                                metadata,
                            },
                            None => raw_fallback_message(buffer, bad_payload_error()),
                        },
                        // A marked record holds a byte array the sink could not represent as
                        // JSON; re-read it as bytes so a binary payload survives the round
                        // trip. The payload must really be an array — a marked string or
                        // object is not ours.
                        (Some(payload), None)
                            if metadata.get(BYTE_PAYLOAD_KEY).map(String::as_str)
                                == Some(BYTE_PAYLOAD_MARK)
                                && payload.get().starts_with('[') =>
                        {
                            decode_byte_payload_record(buffer).unwrap_or_else(|| {
                                strip_byte_marker(&mut metadata);
                                CanonicalMessage {
                                    message_id,
                                    payload: payload.get().as_bytes().to_vec().into(),
                                    metadata,
                                }
                            })
                        }
                        (Some(payload), None) => CanonicalMessage {
                            message_id,
                            payload: payload.get().as_bytes().to_vec().into(),
                            metadata,
                        },
                        // Mutually exclusive, like CloudEvents `data`/`data_base64`.
                        (Some(_), Some(_)) | (None, None) => {
                            raw_fallback_message(buffer, bad_payload_error())
                        }
                    }
                }
                Err(e) => raw_fallback_message(buffer, e),
            };
            Some(msg)
        }
        // `normal` and `text` want the payload as bytes, so it is decoded in one
        // pass by [`RawPayload`] rather than through a `serde_json::Value`.
        FileFormat::Normal | FileFormat::Text => {
            let msg = match serde_json::from_slice::<BytePayloadMessage>(buffer) {
                Ok(wrapper) => match wrapper.into_message() {
                    Some(msg) => msg,
                    None => raw_fallback_message(buffer, bad_payload_error()),
                },
                Err(e) => raw_fallback_message(buffer, e),
            };
            Some(msg)
        }
    }
}

#[derive(serde::Deserialize)]
struct BytePayloadMessage {
    #[serde(deserialize_with = "deserialize_u128")]
    message_id: u128,
    #[serde(default)]
    payload: MaybePayload<RawPayload>,
    #[serde(default)]
    payload_base64: Option<String>,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

impl BytePayloadMessage {
    /// `None` when the line carries no usable payload field — i.e. it is not the
    /// envelope after all, and the caller falls back to keeping the line verbatim.
    fn into_message(mut self) -> Option<CanonicalMessage> {
        strip_byte_marker(&mut self.metadata);
        let payload = match (self.payload.into_option(), self.payload_base64) {
            (None, Some(b64)) => decode_base64_payload(&b64)?,
            (Some(payload), None) => payload.into_bytes().into(),
            // Mutually exclusive, like CloudEvents `data`/`data_base64`.
            (Some(_), Some(_)) | (None, None) => return None,
        };
        Some(CanonicalMessage {
            message_id: self.message_id,
            payload,
            metadata: self.metadata,
        })
    }
}

/// Tells "field absent" apart from an explicit `null` payload — `Option` alone
/// cannot, because serde maps JSON `null` to `None`, and a `null` payload has
/// always decoded to the text `null`.
#[derive(Default)]
enum MaybePayload<T> {
    #[default]
    Missing,
    Present(T),
}

impl<T> MaybePayload<T> {
    fn into_option(self) -> Option<T> {
        match self {
            MaybePayload::Missing => None,
            MaybePayload::Present(v) => Some(v),
        }
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for MaybePayload<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(MaybePayload::Present)
    }
}

/// Decode the `payload_base64` field written for binary payloads.
fn decode_base64_payload(encoded: &str) -> Option<bytes::Bytes> {
    crate::support::base64_engine::decode(encoded)
        .ok()
        .map(bytes::Bytes::from)
}

/// A `Data`-category error so a record that parsed but carries no usable payload
/// gets the same "not a record envelope" warning as a shape mismatch.
fn bad_payload_error() -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(
        "record has neither a `payload` nor a `payload_base64` field",
    )
}

/// Decode a record whose `payload` is wanted as bytes, in one pass via [`RawPayload`]
/// rather than through a `serde_json::Value`. `None` if the line is not that envelope.
fn decode_byte_payload_record(buffer: &[u8]) -> Option<CanonicalMessage> {
    serde_json::from_slice::<BytePayloadMessage>(buffer)
        .ok()?
        .into_message()
}

/// Drop the marker this crate wrote — it is a storage detail, not the message's own
/// metadata. Any other value under that key belongs to the producer and is left alone.
fn strip_byte_marker(metadata: &mut HashMap<String, String>) {
    if metadata.get(BYTE_PAYLOAD_KEY).map(String::as_str) == Some(BYTE_PAYLOAD_MARK) {
        metadata.remove(BYTE_PAYLOAD_KEY);
    }
}

/// A line that is not the JSON envelope the format promised is kept verbatim as a
/// raw payload rather than dropped, and marked so the next hop can tell.
fn raw_fallback_message(buffer: &[u8], err: serde_json::Error) -> CanonicalMessage {
    // Two very different situations end up here, and they get their own one-shot flag so
    // one cannot mask the other:
    //
    // - not JSON at all (a plain text file): expected, and every line hits it.
    // - valid JSON that is not the record envelope: almost always a mistake in a
    //   hand-written file, and the silent fallback discards the `metadata` the line
    //   carried. Worth naming the envelope so the fix is obvious.
    static WARNED_SHAPE: AtomicBool = AtomicBool::new(false);
    static WARNED_SYNTAX: AtomicBool = AtomicBool::new(false);
    let (warned, message) = if err.classify() == serde_json::error::Category::Data {
        (
            &WARNED_SHAPE,
            "File line is valid JSON but not a record envelope, so the whole line is \
             taken as the payload and its own `metadata` is discarded. A `json`/`normal`/\
             `text` source expects the envelope a matching sink writes: \
             {\"message_id\": ..., \"payload\": ..., \"metadata\": {...}}. Use `format: raw` \
             for plain JSON lines. Further occurrences are logged at debug level.",
        )
    } else {
        (
            &WARNED_SYNTAX,
            "Failed to parse file line as JSON, treating as raw. Further occurrences are \
             logged at debug level.",
        )
    };
    if !warned.swap(true, Ordering::Relaxed) {
        warn!(error = %err, content_length = buffer.len(), "{message}");
    } else {
        tracing::debug!(error = %err, content_length = buffer.len(), "{message}");
    }
    let mut msg = CanonicalMessage::new(buffer.to_vec(), None);
    msg.metadata
        .insert("mq_bridge.original_format".to_string(), "raw".to_string());
    msg
}

/// The payload of a `normal`/`text` line, decoded in a single pass.
///
/// `normal` serializes the payload as a JSON array of byte values, which is the
/// common case and the expensive one: routing it through `serde_json::Value`
/// allocates a boxed number per byte and then walks the array a second time to
/// turn it back into `Vec<u8>`. This collects those bytes straight off the
/// parser and only materializes a `Value` for payloads that are not byte arrays
/// (a `json`-format file read back as `normal`, say), which keeps the fallback
/// behaviour — render the payload as JSON text — byte-for-byte the same.
enum RawPayload {
    Bytes(Vec<u8>),
    Str(String),
    Other(serde_json::Value),
}

impl RawPayload {
    fn into_bytes(self) -> Vec<u8> {
        match self {
            RawPayload::Bytes(b) => b,
            RawPayload::Str(s) => s.into_bytes(),
            RawPayload::Other(v) => serde_json::to_vec(&v).unwrap_or_default(),
        }
    }
}

/// One element of a payload array: a byte on the fast path, anything else kept
/// as a `Value` so a non-byte array still round-trips as JSON text.
enum PayloadElement {
    Byte(u8),
    Other(serde_json::Value),
}

impl<'de> serde::Deserialize<'de> for PayloadElement {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ElementVisitor;

        impl<'de> serde::de::Visitor<'de> for ElementVisitor {
            type Value = PayloadElement;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<PayloadElement, E> {
                Ok(match u8::try_from(v) {
                    Ok(b) => PayloadElement::Byte(b),
                    Err(_) => PayloadElement::Other(v.into()),
                })
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<PayloadElement, E> {
                Ok(match u8::try_from(v) {
                    Ok(b) => PayloadElement::Byte(b),
                    Err(_) => PayloadElement::Other(v.into()),
                })
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<PayloadElement, E> {
                Ok(PayloadElement::Other(
                    serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, Into::into),
                ))
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<PayloadElement, E> {
                Ok(PayloadElement::Other(v.into()))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PayloadElement, E> {
                Ok(PayloadElement::Other(v.into()))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<PayloadElement, E> {
                Ok(PayloadElement::Other(serde_json::Value::Null))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<PayloadElement, E> {
                Ok(PayloadElement::Other(serde_json::Value::Null))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<PayloadElement, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
                    .map(PayloadElement::Other)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<PayloadElement, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(PayloadElement::Other)
            }
        }

        d.deserialize_any(ElementVisitor)
    }
}

impl<'de> serde::Deserialize<'de> for RawPayload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct PayloadVisitor;

        impl<'de> serde::de::Visitor<'de> for PayloadVisitor {
            type Value = RawPayload;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte array, a string or any JSON value")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<RawPayload, E> {
                Ok(RawPayload::Str(v.to_string()))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<RawPayload, E> {
                Ok(RawPayload::Str(v))
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<RawPayload, E> {
                Ok(RawPayload::Bytes(v.to_vec()))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<RawPayload, E> {
                Ok(RawPayload::Bytes(v))
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(v.into()))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(v.into()))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(v.into()))
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(
                    serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, Into::into),
                ))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(serde_json::Value::Null))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<RawPayload, E> {
                Ok(RawPayload::Other(serde_json::Value::Null))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<RawPayload, A::Error> {
                serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(RawPayload::Other)
            }

            /// Bytes accumulate until an element turns out not to be one; from
            /// there the array is rebuilt as a `Value` so it renders as JSON text,
            /// matching what the `Value`-based decode used to produce.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<RawPayload, A::Error> {
                let mut bytes: Vec<u8> = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(element) = seq.next_element::<PayloadElement>()? {
                    match element {
                        PayloadElement::Byte(b) => bytes.push(b),
                        PayloadElement::Other(value) => {
                            let mut values: Vec<serde_json::Value> =
                                bytes.into_iter().map(serde_json::Value::from).collect();
                            values.push(value);
                            while let Some(rest) = seq.next_element::<PayloadElement>()? {
                                values.push(match rest {
                                    PayloadElement::Byte(b) => b.into(),
                                    PayloadElement::Other(v) => v,
                                });
                            }
                            return Ok(RawPayload::Other(serde_json::Value::Array(values)));
                        }
                    }
                }
                Ok(RawPayload::Bytes(bytes))
            }
        }

        d.deserialize_any(PayloadVisitor)
    }
}

#[cfg(test)]
mod tests;
