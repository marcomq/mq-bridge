//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Record encoding and decoding per `FileFormat`, shared by the file and object-store endpoints.

use super::csv::{
    CsvHeader, UTF8_BOM, decode_csv_row, invalid_data, pending_csv_dialect, strip_json_line_breaks,
};
use crate::CanonicalMessage;
use crate::canonical_message::deserialize_u128;
use crate::models::FileFormat;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

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
/// state and goes through [`super::csv::csv_append_record`] instead.
///
/// `delimiter` is the record separator the caller appends. JSON records are rewritten so
/// they cannot contain it (see [`escape_delimiter_in_json`]); `raw` is written as is.
pub(crate) fn encode_record(
    msg: &CanonicalMessage,
    format: &FileFormat,
    delimiter: &[u8],
) -> Result<Bytes, serde_json::Error> {
    let record = match format {
        // `Bytes` is refcounted, so a verbatim copy costs no allocation and no memcpy;
        // the caller's append into the batch buffer is then the only copy of the payload.
        FileFormat::Raw => return Ok(msg.payload.clone()),
        // The sink format decides the encoding, not the message's origin: `normal`
        // always writes the wrapper so `message_id` and metadata survive the round
        // trip. Use `format: raw` for verbatim, unwrapped copies.
        FileFormat::Normal => serde_json::to_vec(msg).map(Bytes::from),
        // Parsing only establishes that the payload is JSON; its own bytes are what
        // gets written. A `serde_json::Value` in between would re-sort the object's
        // keys and reformat its numbers, which the reader then cannot undo.
        FileFormat::Json => {
            if let Ok(raw) = serde_json::from_slice::<&serde_json::value::RawValue>(&msg.payload) {
                // A pretty-printed payload's line breaks would split the record.
                let one_line = match strip_json_line_breaks(raw.get()) {
                    std::borrow::Cow::Borrowed(_) => None,
                    std::borrow::Cow::Owned(text) => {
                        Some(serde_json::value::RawValue::from_string(text)?)
                    }
                };
                serde_json::to_vec(&RecordWrapper {
                    message_id: msg.message_id,
                    payload: one_line.as_deref().unwrap_or(raw),
                    metadata: msg.metadata_map(),
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
                    metadata: msg.metadata_map(),
                })
                .map(Bytes::from)
            } else {
                encode_byte_payload(msg)
            }
        }
        FileFormat::Csv => unreachable!("CSV is encoded by the caller, not encode_record"),
        FileFormat::Parquet => Err(serde::ser::Error::custom(
            "parquet is encoded per batch by the object_store sink, not per record",
        )),
    }?;
    escape_delimiter_in_json(record, delimiter)
}

/// Makes a JSON record safe to frame with a custom `delimiter`: inside strings, every
/// character sharing a byte with it becomes a `\u` escape, which decodes to the same
/// value. Fails when the delimiter still occurs, i.e. in JSON syntax outside a string.
fn escape_delimiter_in_json(record: Bytes, delimiter: &[u8]) -> Result<Bytes, serde_json::Error> {
    // Line breaks never occur raw in these records: serde escapes them in strings, and
    // `json` strips them from verbatim payloads.
    // The record's tail plus the appended delimiter must not complete it early either.
    let framed_badly = |r: &[u8]| {
        memchr::memmem::find(r, delimiter).is_some()
            || (1..delimiter.len()).any(|n| r.ends_with(&delimiter[..n]))
    };
    if matches!(delimiter, [b'\n'] | [b'\r', b'\n']) || !framed_badly(&record) {
        return Ok(record);
    }
    let text = std::str::from_utf8(&record)
        .map_err(|e| invalid_data(format!("record is not UTF-8: {e}")))?;
    let shares_byte = |c: char| {
        c.encode_utf8(&mut [0; 4])
            .bytes()
            .any(|b| delimiter.contains(&b))
    };

    let mut out = String::with_capacity(text.len() + 16);
    let mut in_string = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_string => {
                // An existing escape is copied whole; `\uXXXX` carries four more chars.
                out.push(c);
                if let Some(kind) = chars.next() {
                    out.push(kind);
                    if kind == 'u' {
                        out.extend(chars.by_ref().take(4));
                    }
                }
            }
            '"' => {
                in_string = !in_string;
                out.push(c);
            }
            c if in_string && shares_byte(c) => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    use std::fmt::Write as _;
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }

    if framed_badly(out.as_bytes()) {
        return Err(invalid_data(format!(
            "record contains the delimiter {:?} outside a JSON string, where it cannot be \
             escaped; use a delimiter JSON never contains, such as 0x1e",
            String::from_utf8_lossy(delimiter)
        )));
    }
    Ok(Bytes::from(out))
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
    let mut metadata = msg.metadata_map().clone();
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

/// Drops a UTF-8 byte-order mark from the first record of a file. CSV strips its own.
pub(crate) fn strip_leading_bom(record: &mut Vec<u8>, position: u64, format: &FileFormat) {
    if position == 0 && !matches!(format, FileFormat::Csv) && record.starts_with(UTF8_BOM) {
        record.drain(..UTF8_BOM.len());
    }
}

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
        Some(header) => decode_csv_row(header, bytes)?,
        None => parse_message(bytes, format, &mut None)?,
    };
    if with_offset {
        msg.set_metadata("file_offset", position.to_string());
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

    let pending = match format {
        FileFormat::Csv => pending_csv_dialect(csv_header.as_deref()),
        _ => None,
    };
    if let Some(dialect) = pending {
        let blank = spans
            .iter()
            .take_while(|&&(start, end, _)| start == end)
            .count();
        let Some((&(start, end, position), rest)) = spans[blank..].split_first() else {
            return out;
        };
        let first = String::from_utf8_lossy(&buf[start..end]);
        let (header, is_data) = CsvHeader::establish(&dialect, first.as_bytes());
        if is_data {
            let bom = if buf[start..end].starts_with(UTF8_BOM) {
                UTF8_BOM.len()
            } else {
                0
            };
            let span = (start + bom, end, position);
            out.extend(decode_one(buf, &span, format, Some(&header), with_offset));
        }
        *csv_header = Some(Arc::new(header));
        spans = rest;
    }

    let chunks = crate::support::parallel::decode_chunk_count(spans.len());
    if chunks <= 1 {
        crate::support::probe::probe!("file.decode.sequential");
        let header = csv_header.as_deref();
        out.extend(
            spans
                .iter()
                .filter_map(|span| decode_one(buf, span, format, header, with_offset)),
        );
        return out;
    }

    crate::support::probe::probe!("file.decode.parallel");
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

pub(crate) fn parse_message(
    buffer: &[u8],
    format: &FileFormat,
    csv_header: &mut Option<CsvHeader>,
) -> Option<CanonicalMessage> {
    match format {
        FileFormat::Csv => {
            if buffer.is_empty() {
                return None;
            }
            let mut buffer = buffer;
            if let Some(dialect) = pending_csv_dialect(csv_header.as_ref()) {
                let first = String::from_utf8_lossy(buffer);
                let (header, is_data) = CsvHeader::establish(&dialect, first.as_bytes());
                *csv_header = Some(header);
                if !is_data {
                    return None;
                }
                buffer = buffer.strip_prefix(UTF8_BOM).unwrap_or(buffer);
            }
            decode_csv_row(csv_header.as_ref()?, buffer)
        }
        // Parquet objects are decoded whole by the object_store source; there are no lines.
        FileFormat::Parquet => None,
        // A blank line is not a record, in any line format.
        _ if buffer.is_empty() => None,
        FileFormat::Raw => {
            let mut msg = CanonicalMessage::new(buffer.to_vec(), None);
            msg.set_metadata("mq_bridge.original_format", "raw");
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
                            Some(payload) => {
                                CanonicalMessage::from_parts(message_id, payload, metadata)
                            }
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
                                CanonicalMessage::from_parts(
                                    message_id,
                                    payload.get().as_bytes().to_vec().into(),
                                    metadata,
                                )
                            })
                        }
                        (Some(payload), None) => CanonicalMessage::from_parts(
                            message_id,
                            payload.get().as_bytes().to_vec().into(),
                            metadata,
                        ),
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
        Some(CanonicalMessage::from_parts(
            self.message_id,
            payload,
            self.metadata,
        ))
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
             {\"message_id\": ..., \"payload\": ..., \"metadata\": {...}}. Use `format: payload` \
             (also `raw`) for plain JSON lines. Further occurrences are logged at debug level.",
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
    msg.set_metadata("mq_bridge.original_format", "raw");
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
