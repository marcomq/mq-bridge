//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! CSV encoding and decoding of records, shared by the file and object-store endpoints.

use crate::CanonicalMessage;
use crate::codec::{CsvDialect, CsvSyntax};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

/// Appends a CSV-escaped field to `buf` without allocating for the common
/// (no special characters) case. Hot path for CSV row encoding.
///
/// `delimiter` is the record separator: a field containing it is quoted too, so the
/// reader's quote-aware framing keeps it in one record, and so is one ending in a prefix
/// of a multi-byte delimiter, which would otherwise complete it early. A leading U+FEFF
/// is quoted so the reader cannot mistake it for a byte-order mark.
///
/// With `quote: none` such a field has no spelling, so it fails instead.
fn csv_append_field(
    buf: &mut Vec<u8>,
    s: &str,
    delimiter: &[u8],
    syntax: CsvSyntax,
) -> Result<(), serde_json::Error> {
    let bytes = s.as_bytes();
    let separator = syntax.separator;
    // Without a quote character this compares against the separator twice.
    let quote = syntax.quote.unwrap_or(separator);
    // One byte pass instead of four `contains` scans.
    let special = bytes
        .iter()
        .any(|&b| b == separator || b == quote || b == b'\n' || b == b'\r')
        || bytes.starts_with(UTF8_BOM);
    let has_delimiter = || match delimiter {
        [] | [b'\n'] | [b'\r', b'\n'] => false,
        [d] => bytes.contains(d),
        d => {
            memchr::memmem::find(bytes, d).is_some()
                || (1..d.len()).any(|n| bytes.ends_with(&d[..n]))
        }
    };
    if !special && !has_delimiter() {
        buf.extend_from_slice(bytes);
        return Ok(());
    }
    if syntax.quote.is_none() {
        return Err(invalid_data(
            "the value holds the separator, a line break or the record delimiter, which \
             `quote: none` cannot write"
                .to_string(),
        ));
    }
    buf.push(quote);
    let mut rest = bytes;
    while let Some(at) = next_quote(quote, rest) {
        buf.extend_from_slice(&rest[..=at]);
        buf.push(quote);
        rest = &rest[at + 1..];
    }
    buf.extend_from_slice(rest);
    buf.push(quote);
    Ok(())
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

pub(crate) fn csv_encode_row(
    fields: &[String],
    delimiter: &[u8],
    syntax: CsvSyntax,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut buf = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            buf.push(syntax.separator);
        }
        csv_append_field(&mut buf, f, delimiter, syntax)?;
    }
    quote_blank_record(&mut buf, fields.len(), syntax)?;
    Ok(buf)
}

/// A record of one empty field is written `""`: bare, it would be a blank line, which
/// the reader skips. Without a quote character it has no spelling, so it fails.
fn quote_blank_record(
    record: &mut Vec<u8>,
    fields: usize,
    syntax: CsvSyntax,
) -> Result<(), serde_json::Error> {
    if fields != 1 || !record.is_empty() {
        return Ok(());
    }
    let quote = syntax.quote.ok_or_else(|| {
        invalid_data("a record of one empty field is a blank line under `quote: none`".into())
    })?;
    record.extend_from_slice(&[quote, quote]);
    Ok(())
}

/// Drops raw line breaks from valid JSON text. Inside a JSON string they must be
/// escaped, so a raw one is always insignificant whitespace; every other byte of the
/// producer's spelling is kept. Borrows when there is nothing to drop.
pub(crate) fn strip_json_line_breaks(text: &str) -> std::borrow::Cow<'_, str> {
    if memchr::memchr2(b'\n', b'\r', text.as_bytes()).is_none() {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().filter(|c| !matches!(c, '\n' | '\r')).collect())
}

/// A payload's top-level fields, values left as unparsed JSON. Keys are borrowed when
/// they can be; only keys carrying JSON escapes need owning.
enum CsvPayloadRow<'a> {
    Borrowed(HashMap<&'a str, &'a serde_json::value::RawValue>),
    Owned(HashMap<String, &'a serde_json::value::RawValue>),
}

impl<'a> CsvPayloadRow<'a> {
    fn parse(payload: &'a [u8]) -> Option<Self> {
        match serde_json::from_slice(payload) {
            Ok(row) => Some(Self::Borrowed(row)),
            Err(_) => serde_json::from_slice(payload).ok().map(Self::Owned),
        }
    }

    fn get(&self, key: &str) -> Option<&'a serde_json::value::RawValue> {
        match self {
            Self::Borrowed(row) => row.get(key).copied(),
            Self::Owned(row) => row.get(key).copied(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Borrowed(row) => row.len(),
            Self::Owned(row) => row.len(),
        }
    }

    fn sorted_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = match self {
            Self::Borrowed(row) => row.keys().map(|k| (*k).to_string()).collect(),
            Self::Owned(row) => row.keys().cloned().collect(),
        };
        keys.sort();
        keys
    }
}

type RawEntry<'a> = (String, &'a serde_json::value::RawValue);

/// An object's entries in the order the producer wrote them, values left unparsed.
fn entries_in_payload_order(object: &[u8]) -> Option<Vec<RawEntry<'_>>> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = Vec<RawEntry<'de>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }
    use serde::Deserializer;
    serde_json::Deserializer::from_slice(object)
        .deserialize_map(Entries)
        .ok()
}

/// The CSV columns a payload spells out, in the order the producer wrote them. With
/// `flatten`, a nested object contributes one `parent.child` column per leaf. Runs once
/// per file, for the row that establishes the header.
fn csv_columns(payload: &[u8], flatten: bool) -> Option<Vec<String>> {
    fn collect(
        object: &[u8],
        prefix: &str,
        flatten: bool,
        columns: &mut Vec<String>,
    ) -> Option<()> {
        for (key, value) in entries_in_payload_order(object)? {
            let name = format!("{prefix}{key}");
            if flatten && is_filled_object(value) {
                collect(
                    value.get().as_bytes(),
                    &format!("{name}."),
                    flatten,
                    columns,
                )?;
            } else if !columns.contains(&name) {
                columns.push(name);
            }
        }
        Some(())
    }
    let mut columns = Vec::new();
    collect(payload, "", flatten, &mut columns)?;
    Some(columns)
}

/// An object with at least one field; an empty one stays a cell of its own.
fn is_filled_object(value: &serde_json::value::RawValue) -> bool {
    value
        .get()
        .strip_prefix('{')
        .is_some_and(|rest| !rest.trim_start().starts_with('}'))
}

/// The value a flattened `parent.child` column names, looked up through the nested objects.
fn nested_value<'a>(
    row: &CsvPayloadRow<'a>,
    column: &str,
) -> Option<&'a serde_json::value::RawValue> {
    column.match_indices('.').find_map(|(dot, _)| {
        let parent = row
            .get(&column[..dot])
            .filter(|value| is_filled_object(value))?;
        let parent = CsvPayloadRow::parse(parent.get().as_bytes())?;
        let rest = &column[dot + 1..];
        parent.get(rest).or_else(|| nested_value(&parent, rest))
    })
}

pub(crate) fn invalid_data(message: String) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// What [`csv_encode_ordered`] writes a row with.
#[derive(Clone, Copy)]
struct CsvRowShape<'a> {
    cols: &'a [String],
    delimiter: &'a [u8],
    syntax: CsvSyntax,
    flatten: bool,
}

/// Writes `payload` as a CSV row when its keys spell exactly the header's columns, in
/// that order: the shape every row after the first usually has, read without building a
/// key map. `false` means the row is not that shape or failed; `row_buf` is then garbage.
fn csv_encode_ordered(payload: &[u8], shape: CsvRowShape, row_buf: &mut Vec<u8>) -> bool {
    let mut next = 0;
    !shape.cols.is_empty()
        && csv_encode_ordered_object(payload, "", shape, &mut next, row_buf)
        && next == shape.cols.len()
}

/// One object of [`csv_encode_ordered`]: `prefix` is the flattened path leading to it and
/// `next` the column its next key has to name.
fn csv_encode_ordered_object(
    object: &[u8],
    prefix: &str,
    shape: CsvRowShape,
    next: &mut usize,
    row_buf: &mut Vec<u8>,
) -> bool {
    use serde::de::{DeserializeSeed, Deserializer, Error, MapAccess, Visitor};

    enum KeyMatch {
        Cell,
        /// The key is the parent of the column; holds the length of the nested prefix.
        Parent(usize),
        Other,
    }
    struct Key<'a> {
        col: Option<&'a str>,
        prefix: &'a str,
    }
    impl<'de> DeserializeSeed<'de> for Key<'_> {
        type Value = KeyMatch;
        fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<KeyMatch, D::Error> {
            de.deserialize_str(self)
        }
    }
    impl Visitor<'_> for Key<'_> {
        type Value = KeyMatch;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object key")
        }
        fn visit_str<E>(self, key: &str) -> Result<KeyMatch, E> {
            let rest = self.col.and_then(|col| col.strip_prefix(self.prefix));
            Ok(match rest.and_then(|rest| rest.strip_prefix(key)) {
                Some("") => KeyMatch::Cell,
                Some(tail) if tail.starts_with('.') => {
                    KeyMatch::Parent(self.prefix.len() + key.len() + 1)
                }
                _ => KeyMatch::Other,
            })
        }
    }

    struct Row<'a> {
        prefix: &'a str,
        shape: CsvRowShape<'a>,
        next: &'a mut usize,
        row_buf: &'a mut Vec<u8>,
    }
    impl<'de> Visitor<'de> for Row<'_> {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let Row {
                prefix,
                shape,
                next,
                row_buf,
            } = self;
            loop {
                let col = shape.cols.get(*next).map(String::as_str);
                let Some(found) = map.next_key_seed(Key { col, prefix })? else {
                    return Ok(true);
                };
                let value: &serde_json::value::RawValue = map.next_value()?;
                match (found, col) {
                    (KeyMatch::Cell, _) => {
                        if *next > 0 {
                            row_buf.push(shape.syntax.separator);
                        }
                        csv_append_raw(row_buf, value, shape.delimiter, shape.syntax)
                            .map_err(A::Error::custom)?;
                        *next += 1;
                    }
                    (KeyMatch::Parent(len), Some(col))
                        if shape.flatten && is_filled_object(value) =>
                    {
                        let nested = value.get().as_bytes();
                        if !csv_encode_ordered_object(nested, &col[..len], shape, next, row_buf) {
                            return Ok(false);
                        }
                    }
                    _ => return Ok(false),
                }
            }
        }
    }

    let mut de = serde_json::Deserializer::from_slice(object);
    let row = Row {
        prefix,
        shape,
        next,
        row_buf,
    };
    matches!(de.deserialize_map(row), Ok(true)) && de.end().is_ok()
}

/// Encodes `msg`'s JSON-object payload as a CSV row into `row_buf` (cleared first),
/// establishing the column order from its keys as written when `hdr` is still unset. Returns
/// `true` when this call established the header, so the caller can emit the header
/// line for a new file. Shared by the plain-append and member (compressed/encrypted)
/// write paths. A payload that fails leaves `hdr` untouched.
pub(crate) fn csv_encode_message(
    msg: &CanonicalMessage,
    hdr: &mut Option<Vec<String>>,
    row_buf: &mut Vec<u8>,
    delimiter: &[u8],
    csv: &CsvDialect,
) -> Result<bool, serde_json::Error> {
    let syntax = csv.syntax();
    if let Some(cols) = hdr.as_deref() {
        row_buf.clear();
        row_buf.reserve(msg.payload.len());
        let shape = CsvRowShape {
            cols,
            delimiter,
            syntax,
            flatten: csv.flatten,
        };
        if csv_encode_ordered(&msg.payload, shape, row_buf) {
            quote_blank_record(row_buf, cols.len(), syntax)?;
            return Ok(false);
        }
    }
    // An object with no fields is rejected too: it carries no columns, so letting it
    // establish the header would fix an empty column set for the rest of the file.
    let Some(row) = CsvPayloadRow::parse(&msg.payload).filter(|row| row.len() > 0) else {
        return Err(invalid_data(
            "CSV format requires a non-empty JSON object payload".to_string(),
        ));
    };

    let new_cols = hdr.is_none().then(|| match &csv.columns {
        Some(columns) => columns.to_vec(),
        None => csv_columns(&msg.payload, csv.flatten).unwrap_or_else(|| row.sorted_keys()),
    });
    let cols = match (&new_cols, &*hdr) {
        (Some(cols), _) | (None, Some(cols)) => cols,
        (None, None) => unreachable!("header is either set or being established"),
    };

    // Reused across the batch: rows are all the same shape, so
    // after the first one this never reallocates.
    row_buf.clear();
    row_buf.reserve(msg.payload.len());
    // Columns this payload actually supplied, for the drift check below.
    let mut matched = 0usize;
    // Flattened columns outnumber the payload's top-level keys, which voids the count below.
    let mut flattened = false;
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            row_buf.push(syntax.separator);
        }
        let value = row.get(c).or_else(|| {
            let nested = csv.flatten.then(|| nested_value(&row, c)).flatten();
            flattened |= nested.is_some();
            nested
        });
        if let Some(v) = value {
            csv_append_raw(row_buf, v, delimiter, syntax)
                .map_err(|e| invalid_data(format!("CSV column '{c}': {e}")))?;
            matched += 1;
        }
    }
    quote_blank_record(row_buf, cols.len(), syntax)?;

    // Configured `columns` are a projection: keys outside them are dropped on purpose.
    let has_extra_keys = !flattened && csv.columns.is_none() && row.len() > matched;
    if csv.strict {
        // A flattened row hides its key count, so its columns are compared by name.
        let uncovered = || {
            csv_columns(&msg.payload, csv.flatten)
                .unwrap_or_else(|| row.sorted_keys())
                .into_iter()
                .find(|key| {
                    !cols.iter().any(|col| {
                        key == col
                            || key
                                .strip_prefix(col.as_str())
                                .is_some_and(|rest| rest.starts_with('.'))
                    })
                })
        };
        let extra = if csv.columns.is_some() {
            None
        } else {
            uncovered()
        };
        if matched < cols.len() || extra.is_some() {
            return Err(invalid_data(match extra {
                Some(key) => format!(
                    "CSV payload has key '{key}', which is not a column (`on_mismatch: fail`)"
                ),
                None => format!(
                    "CSV payload supplies {matched} of {} columns (`on_mismatch: fail`)",
                    cols.len()
                ),
            }));
        }
    }
    if new_cols.is_none() && (matched < cols.len() || has_extra_keys) {
        // Keys the payload has beyond the ones the header covers are dropped silently, and
        // missing ones become empty fields; both mean the file's schema drifted. Logged
        // once per process so a whole drifted stream doesn't flood the log.
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                header_columns = cols.len(),
                payload_keys = row.len(),
                matched_columns = matched,
                "CSV payload keys differ from the established header: extra keys are dropped and missing ones written as empty fields. Logged once per process."
            );
        }
    }
    let established = new_cols.is_some();
    if established {
        *hdr = new_cols;
    }
    Ok(established)
}

/// Appends `msg` as one CSV record to `out`. A row that settles the columns is preceded by
/// the header record when `header` is set and the dialect writes one; returns whether it was.
/// A header that cannot be written is forgotten again, so the next row retries it.
pub(crate) fn csv_append_record(
    out: &mut Vec<u8>,
    msg: &CanonicalMessage,
    hdr: &mut Option<Vec<String>>,
    row_buf: &mut Vec<u8>,
    delimiter: &[u8],
    csv: &CsvDialect,
    header: bool,
) -> Result<bool, serde_json::Error> {
    let wrote_header =
        csv_encode_message(msg, hdr, row_buf, delimiter, csv)? && header && csv.header;
    if wrote_header {
        let columns = hdr.as_deref().unwrap_or_default();
        let line = csv_encode_row(columns, delimiter, csv.syntax()).map_err(|e| {
            *hdr = None;
            invalid_data(format!("CSV header: {e}"))
        })?;
        out.extend_from_slice(&line);
        out.extend_from_slice(delimiter);
    }
    out.extend_from_slice(row_buf);
    out.extend_from_slice(delimiter);
    Ok(wrote_header)
}

/// Appends one still-unparsed JSON value as a CSV field. Scalars are copied straight
/// from the source bytes, so numbers keep their spelling; nested arrays/objects are
/// written as their own JSON text, keeping the producer's key order.
///
/// Fails for a string with no UTF-8 spelling (a lone surrogate escape) rather than
/// silently writing the cell empty.
fn csv_append_raw(
    buf: &mut Vec<u8>,
    raw: &serde_json::value::RawValue,
    delimiter: &[u8],
    syntax: CsvSyntax,
) -> Result<(), serde_json::Error> {
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'"') => {
            let inner = &text[1..text.len() - 1];
            if inner.as_bytes().contains(&b'\\') {
                let unescaped = serde_json::from_str::<String>(text)?;
                csv_append_field(buf, &unescaped, delimiter, syntax)
            } else {
                csv_append_field(buf, inner, delimiter, syntax)
            }
        }
        // Raw line breaks are JSON whitespace; dropping them keeps the cell on one line.
        Some(b'{') | Some(b'[') => {
            csv_append_field(buf, &strip_json_line_breaks(text), delimiter, syntax)
        }
        // Numbers, bools, null: never need quoting, but the scan is one pass anyway.
        _ => csv_append_field(buf, text, delimiter, syntax),
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
/// Quote handling matches [`CsvQuoteState`] exactly, including its quirk that a
/// quote opens a section only while the field is still empty — so `in"ch` keeps a literal
/// quote and `"a"x` reads as `ax`. The two must never disagree about where a record ends.
///
/// `bytes` must be valid UTF-8.
fn emit_csv_field(out: &mut Vec<u8>, bytes: &[u8], pos: &mut usize, syntax: CsvSyntax) -> bool {
    let separator = syntax.separator;
    // No quote character: the separator check below wins, so a quote is never seen.
    let quote = syntax.quote.unwrap_or(separator);
    let mut i = *pos;
    let mut in_quotes = false;
    // Mirrors the source parser's `cur.is_empty()`: whether this field has content yet.
    let mut empty = true;

    while i < bytes.len() {
        let rest = &bytes[i..];
        let found = if in_quotes {
            memchr::memchr(quote, rest)
        } else {
            memchr::memchr2(separator, quote, rest)
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

        if !in_quotes && bytes[at] == separator {
            *pos = at + 1;
            return true;
        }

        // A quote: closes an open section, escapes itself when doubled inside one,
        // opens a section on an empty field, and is literal data otherwise.
        if in_quotes {
            if bytes.get(at + 1) == Some(&quote) {
                json_append_escaped(out, &[quote]);
                empty = false;
                i = at + 2;
                continue;
            }
            in_quotes = false;
        } else if empty {
            in_quotes = true;
        } else {
            json_append_escaped(out, &[quote]);
        }
        i = at + 1;
    }

    *pos = i;
    false
}

pub(crate) const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

/// The column state one CSV source threads across the records of a file.
///
/// Each column is stored as the bytes a data row writes ahead of its value — the
/// separating comma, the quoted and JSON-escaped column name, and `:"` — so a row emits
/// one slice per column instead of re-escaping every column name on every row.
pub(crate) struct CsvHeader {
    prefixes: Vec<Vec<u8>>,
    /// Combined length of `prefixes`, to size a row's output buffer in one shot.
    prefix_len: usize,
    syntax: CsvSyntax,
    /// Set until the source's first record settles the columns.
    unread: Option<CsvDialect>,
}

impl CsvHeader {
    /// The slot of a source that has not read a record yet.
    pub(crate) fn unread(dialect: CsvDialect) -> Self {
        Self {
            prefixes: Vec::new(),
            prefix_len: 0,
            syntax: dialect.syntax(),
            unread: Some(dialect),
        }
    }

    /// Reads a header record in the default dialect.
    #[cfg(test)]
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        Self::establish(&CsvDialect::default(), bytes).0
    }

    /// Settles the columns on a source's first record, and says whether that record is
    /// data rather than a header. `first` must be valid UTF-8. A leading byte-order mark
    /// (Excel's "CSV UTF-8") is encoding framing, not part of the first column's name.
    pub(crate) fn establish(dialect: &CsvDialect, first: &[u8]) -> (Self, bool) {
        let bytes = first.strip_prefix(UTF8_BOM).unwrap_or(first);
        let syntax = dialect.resolve(bytes);
        // Kept JSON-escaped: escaping is one-to-one, so comparing them compares the names.
        let mut names: Vec<Vec<u8>> = Vec::new();
        match &dialect.columns {
            Some(columns) => names.extend(columns.iter().map(|column| {
                let mut name = Vec::with_capacity(column.len());
                json_append_escaped(&mut name, column.as_bytes());
                name
            })),
            None => {
                let mut pos = 0;
                loop {
                    let mut name = Vec::with_capacity(16);
                    let more = emit_csv_field(&mut name, bytes, &mut pos, syntax);
                    names.push(name);
                    if !more {
                        break;
                    }
                }
            }
        }
        dedup_column_names(&mut names);
        let prefixes: Vec<Vec<u8>> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let mut prefix = Vec::with_capacity(name.len() + 5);
                if i > 0 {
                    prefix.push(b',');
                }
                prefix.push(b'"');
                prefix.extend_from_slice(&name);
                prefix.extend_from_slice(b"\":\"");
                prefix
            })
            .collect();
        let prefix_len = prefixes.iter().map(Vec::len).sum();
        let header = Self {
            prefixes,
            prefix_len,
            syntax,
            unread: None,
        };
        (header, !dialect.header)
    }

    /// The decoded column names, in file order.
    pub(crate) fn column_names(&self) -> Vec<String> {
        self.prefixes
            .iter()
            .map(|prefix| {
                // `[,]"<escaped name>":"` → the quoted name.
                let prefix = prefix.strip_prefix(b",").unwrap_or(prefix);
                serde_json::from_slice(&prefix[..prefix.len() - 2])
                    .expect("header prefixes hold JSON-escaped names")
            })
            .collect()
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
        let mut missing = 0usize;
        for prefix in &self.prefixes {
            out.extend_from_slice(prefix);
            if has_more {
                has_more = emit_csv_field(&mut out, bytes, &mut pos, self.syntax);
            } else {
                missing += 1;
            }
            out.push(b'"');
        }
        out.push(b'}');

        if has_more {
            self.warn_extra_fields(bytes, pos);
        } else if missing > 0 {
            self.warn_missing_fields(missing);
        }
        out
    }

    /// A short row usually means the separator or the header is not what the file uses.
    #[cold]
    fn warn_missing_fields(&self, missing: usize) {
        static WARNED: AtomicBool = AtomicBool::new(false);
        const MSG: &str = "CSV row has fewer fields than the header has \
                           columns; the missing ones are read as empty. Check \
                           `csv.separator`. Further occurrences are logged at debug level.";
        let columns = self.prefixes.len();
        let separator = (self.syntax.separator as char).escape_default().to_string();
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(columns, missing, separator, "{MSG}");
        } else {
            tracing::debug!(columns, missing, separator, "{MSG}");
        }
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
            if !emit_csv_field(&mut scratch, bytes, &mut pos, self.syntax) {
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
                return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                    "Hex delimiter must be 1 byte (2 hex chars)"
                ))
                .into());
            }
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|e| {
                    crate::errors::InvalidConfig(anyhow::anyhow!("Invalid hex delimiter: {}", e))
                })?
        }
        Some(s) => s.as_bytes().to_vec(),
        None => vec![b'\n'],
    };

    if bytes.is_empty() {
        return Err(
            crate::errors::InvalidConfig(anyhow::anyhow!("Delimiter cannot be empty")).into(),
        );
    }
    Ok(bytes)
}

/// True when `buf` stops inside an open quote, so the delimiter it ended on was field
/// data: RFC 4180 lets a quoted field contain the record separator. Mirrors the quote
/// handling in [`emit_csv_field`], down to only opening a quote at the start of a field,
/// so the splitter and the parser can never disagree about where a record ends.
#[cfg(test)]
fn csv_ends_inside_quotes(buf: &[u8]) -> bool {
    let mut state = CsvQuoteState::default();
    state.feed(buf);
    state.in_quotes()
}

/// Tracks whether CSV bytes fed so far stop inside an open quote. Incremental, so a
/// record read in pieces is scanned once in total, not once per piece.
///
/// A quote seen last inside a quoted section may be the first half of a `""` escape, so
/// it is held as `pending_quote` until the next byte (or the end) decides it.
pub(crate) struct CsvQuoteState {
    in_quotes: bool,
    field_is_empty: bool,
    pending_quote: bool,
    started: bool,
    syntax: CsvSyntax,
}

impl Default for CsvQuoteState {
    fn default() -> Self {
        Self::new(CsvSyntax::default())
    }
}

impl CsvQuoteState {
    pub(crate) fn new(syntax: CsvSyntax) -> Self {
        Self {
            in_quotes: false,
            field_is_empty: true,
            pending_quote: false,
            started: false,
            syntax,
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        // Without a quote character nothing can open a quoted section.
        let Some(quote) = self.syntax.quote else {
            return;
        };
        // A leading BOM is framing, so it must not make the first field non-empty.
        let bytes = if self.started {
            bytes
        } else {
            self.started = true;
            bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes)
        };
        let separator = self.syntax.separator;
        // Only quotes change the state, so jump from one to the next.
        let mut i = 0;
        while i < bytes.len() {
            if self.pending_quote {
                self.pending_quote = false;
                if bytes[i] == quote {
                    i += 1;
                    continue;
                }
                self.in_quotes = false;
            }
            let Some(offset) = next_quote(quote, &bytes[i..]) else {
                if !self.in_quotes {
                    self.field_is_empty = bytes[bytes.len() - 1] == separator;
                }
                return;
            };
            let at = i + offset;
            if self.in_quotes {
                self.pending_quote = true;
            } else {
                if offset > 0 {
                    self.field_is_empty = bytes[at - 1] == separator;
                }
                if self.field_is_empty {
                    self.in_quotes = true;
                } else {
                    self.field_is_empty = quote == separator;
                }
            }
            i = at + 1;
        }
    }

    /// True when the bytes fed so far stop inside an open quote.
    pub(crate) fn in_quotes(&self) -> bool {
        self.in_quotes && !self.pending_quote
    }
}

/// Offset of the next `quote`. Embedded JSON puts one every few bytes, where a short
/// scalar probe beats a `memchr` call; long quote-free runs still get the fast search.
#[inline]
fn next_quote(quote: u8, bytes: &[u8]) -> Option<usize> {
    const PROBE: usize = 16;
    let (head, tail) = bytes.split_at(bytes.len().min(PROBE));
    head.iter()
        .position(|&b| b == quote)
        .or_else(|| memchr::memchr(quote, tail).map(|at| at + PROBE))
}

/// Renames repeated header names `name_2`, `name_3`, … so every column keeps its own
/// JSON key; a repeated key would leave readers to silently keep only one value. A
/// suffix never takes a name the header already has.
fn dedup_column_names(names: &mut [Vec<u8>]) {
    let original: HashSet<Vec<u8>> = names.iter().cloned().collect();
    if original.len() == names.len() {
        return;
    }
    let mut used = HashSet::with_capacity(names.len());
    for name in names.iter_mut() {
        if used.insert(name.clone()) {
            continue;
        }
        let renamed = (2u32..)
            .map(|n| [name.as_slice(), format!("_{n}").as_bytes()].concat())
            .find(|candidate| !original.contains(candidate) && !used.contains(candidate))
            .expect("the suffixes are unbounded");
        used.insert(renamed.clone());
        *name = renamed;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "CSV header repeats column names; repeats are renamed with a _2, _3, … suffix. Logged once per process."
        );
    }
}

/// The dialect a header slot still waits to read its first record with. An empty slot
/// waits with the default dialect; a settled one waits for nothing.
pub(crate) fn pending_csv_dialect(slot: Option<&CsvHeader>) -> Option<CsvDialect> {
    match slot {
        None => Some(CsvDialect::default()),
        Some(header) => header.unread.clone(),
    }
}

/// Decodes one CSV record against an established header.
///
/// Validated once per record so the field walk can stay byte-wise, and lossy so an
/// ill-encoded source still yields a valid JSON string.
pub(crate) fn decode_csv_row(header: &CsvHeader, buffer: &[u8]) -> Option<CanonicalMessage> {
    if buffer.is_empty() {
        return None;
    }
    let line = String::from_utf8_lossy(buffer);
    Some(CanonicalMessage::new(
        header.encode_row(line.as_bytes()),
        None,
    ))
}

#[cfg(test)]
mod tests;
