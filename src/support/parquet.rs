//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Parquet codec for the object_store `format: parquet`: a batch of JSON-object payloads
//! becomes one Parquet body (schema inferred from that batch), and a Parquet body becomes
//! one JSON-object message per row.

use crate::models::Compression;
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use arrow_json::reader::{infer_json_schema, infer_json_schema_from_iterator, ReaderBuilder};
use arrow_json::writer::{LineDelimited, WriterBuilder};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression as ParquetCompression, GzipLevel, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::sync::Arc;

/// Largest integer magnitude an `f64` holds exactly.
const F64_EXACT_INT: u64 = 1 << 53;

/// Rows whose schema the fast path decodes the whole batch with.
const SAMPLE_ROWS: usize = 1024;

const NULL: u8 = 1;
const BOOL: u8 = 1 << 1;
const INT: u8 = 1 << 2;
const FLOAT: u8 = 1 << 3;
const STR: u8 = 1 << 4;
/// An object, an array, or an integer beyond `i64`.
const OTHER: u8 = 1 << 5;
/// An integer beyond [`F64_EXACT_INT`].
const BIG_INT: u8 = 1 << 6;

/// The JSON-object payloads of one Parquet body, kept as NDJSON bytes.
///
/// A push validates the payload without building a tree and notes the kinds of value its
/// top-level fields hold. A batch of scalars whose kinds the first [`SAMPLE_ROWS`] already
/// show is decoded straight from the bytes with the schema inferred from that sample, which
/// is the schema the whole batch infers. Any other batch goes through [`encode_rows`].
#[derive(Default)]
pub(crate) struct RowBuffer {
    ndjson: Vec<u8>,
    rows: usize,
    fields: Vec<(String, u8)>,
    /// Byte length and field kinds once [`SAMPLE_ROWS`] rows are in.
    sample: Option<(usize, Vec<u8>)>,
    /// Field and kind of each value in the row being pushed, merged once it validates.
    row: Vec<(usize, u8)>,
}

impl RowBuffer {
    /// Adds one payload as a row; anything but a JSON object is rejected and adds none.
    pub(crate) fn push(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let known = self.fields.len();
        self.row.clear();
        let mut de = serde_json::Deserializer::from_slice(payload);
        if let Err(e) = de
            .deserialize_map(RowKinds {
                fields: &mut self.fields,
                row: &mut self.row,
            })
            .and_then(|()| de.end())
        {
            self.fields.truncate(known);
            return Err(if e.is_data() {
                anyhow!("parquet row payload must be a JSON object")
            } else {
                anyhow!(e).context("parquet row payload is not valid JSON")
            });
        }
        self.row.sort_unstable_by_key(|(field, _)| *field);
        let mut previous = None;
        for &(field, kind) in &self.row {
            // A repeated key sends the batch to the full path, which keeps the last value.
            self.fields[field].1 |= if previous == Some(field) { OTHER } else { kind };
            previous = Some(field);
        }
        let payload = payload.trim_ascii();
        if payload.iter().any(|b| matches!(b, b'\n' | b'\r')) {
            // Valid JSON has line breaks only as whitespace, which the schema sample reads by line.
            self.ndjson.extend(payload.iter().map(|&b| {
                if matches!(b, b'\n' | b'\r') {
                    b' '
                } else {
                    b
                }
            }));
        } else {
            self.ndjson.extend_from_slice(payload);
        }
        self.ndjson.push(b'\n');
        self.rows += 1;
        if self.rows == SAMPLE_ROWS {
            let kinds = self.fields.iter().map(|(_, kinds)| *kinds).collect();
            self.sample = Some((self.ndjson.len(), kinds));
        }
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Encodes the rows into one Parquet body. `compression` picks the column codec.
    pub(crate) fn encode(&self, compression: Compression) -> anyhow::Result<Vec<u8>> {
        if let Some(batch) = self.decode_scalars() {
            return write_batch(&batch, compression);
        }
        let rows = serde_json::Deserializer::from_slice(&self.ndjson)
            .into_iter()
            .collect::<Result<Vec<serde_json::Value>, _>>()?;
        encode_rows(&rows, compression)
    }

    fn decode_scalars(&self) -> Option<arrow_array::RecordBatch> {
        let sampled = |i: usize| match &self.sample {
            Some((_, kinds)) => kinds.get(i).copied(),
            None => Some(self.fields[i].1),
        };
        let fits = self.fields.iter().enumerate().all(|(i, (_, kinds))| {
            let lossy = kinds & FLOAT != 0 && kinds & BIG_INT != 0;
            kinds & OTHER == 0
                && !lossy
                && sampled(i).is_some_and(|seen| (seen ^ kinds) & !BIG_INT == 0)
        });
        if !fits {
            return None;
        }
        let end = self
            .sample
            .as_ref()
            .map_or(self.ndjson.len(), |(end, _)| *end);
        let (schema, _) = infer_json_schema(&self.ndjson[..end], None).ok()?;
        let mut decoder = ReaderBuilder::new(Arc::new(schema))
            .with_batch_size(self.rows)
            .with_coerce_primitive(true)
            .with_strict_mode(true)
            .build_decoder()
            .ok()?;
        if decoder.decode(&self.ndjson).ok()? != self.ndjson.len() {
            return None;
        }
        decoder.flush().ok()?
    }
}

/// Visits one JSON object, recording the kind of each top-level value against its field.
struct RowKinds<'a> {
    fields: &'a mut Vec<(String, u8)>,
    row: &'a mut Vec<(usize, u8)>,
}

impl<'de> Visitor<'de> for RowKinds<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut hint = 0;
        while let Some(field) = map.next_key_seed(FieldIndex {
            fields: &mut *self.fields,
            hint,
        })? {
            self.row.push((field, map.next_value_seed(ValueKind)?));
            hint = field + 1;
        }
        Ok(())
    }
}

/// Resolves a key to its field, trying the slot after the previous key first.
struct FieldIndex<'a> {
    fields: &'a mut Vec<(String, u8)>,
    hint: usize,
}

impl<'de> DeserializeSeed<'de> for FieldIndex<'_> {
    type Value = usize;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<usize, D::Error> {
        de.deserialize_str(self)
    }
}

impl Visitor<'_> for FieldIndex<'_> {
    type Value = usize;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }
    fn visit_str<E>(self, key: &str) -> Result<usize, E> {
        if self
            .fields
            .get(self.hint)
            .is_some_and(|(name, _)| name == key)
        {
            return Ok(self.hint);
        }
        Ok(match self.fields.iter().position(|(name, _)| name == key) {
            Some(i) => i,
            None => {
                self.fields.push((key.to_owned(), 0));
                self.fields.len() - 1
            }
        })
    }
}

struct ValueKind;

impl<'de> DeserializeSeed<'de> for ValueKind {
    type Value = u8;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<u8, D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ValueKind {
    type Value = u8;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E>(self) -> Result<u8, E> {
        Ok(NULL)
    }
    fn visit_bool<E>(self, _: bool) -> Result<u8, E> {
        Ok(BOOL)
    }
    fn visit_i64<E>(self, v: i64) -> Result<u8, E> {
        Ok(int_kind(v.unsigned_abs()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<u8, E> {
        Ok(if i64::try_from(v).is_err() {
            OTHER
        } else {
            int_kind(v)
        })
    }
    fn visit_f64<E>(self, _: f64) -> Result<u8, E> {
        Ok(FLOAT)
    }
    fn visit_str<E>(self, _: &str) -> Result<u8, E> {
        Ok(STR)
    }
    // Nested values are walked like top-level ones, not skipped, so they get the string,
    // number and depth checks a `serde_json::Value` parse applies.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<u8, A::Error> {
        while map.next_key_seed(ValueKind)?.is_some() {
            map.next_value_seed(ValueKind)?;
        }
        Ok(OTHER)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<u8, A::Error> {
        while seq.next_element_seed(ValueKind)?.is_some() {}
        Ok(OTHER)
    }
}

fn int_kind(magnitude: u64) -> u8 {
    if magnitude > F64_EXACT_INT {
        INT | BIG_INT
    } else {
        INT
    }
}

/// Encodes rows into one Parquet body. `compression` picks the column codec.
///
/// A top-level field whose values don't share one Arrow type, or that would widen large
/// integers to a lossy `Float64`, is written as JSON text instead of failing the batch.
pub(crate) fn encode_rows(
    rows: &[serde_json::Value],
    compression: Compression,
) -> anyhow::Result<Vec<u8>> {
    let batch = match rows_to_batch(rows) {
        Ok(batch) => batch,
        Err(first) => rows_to_batch(&stringify_conflicting_fields(rows))
            .with_context(|| format!("after stringifying conflicting fields ({first:#})"))?,
    };
    write_batch(&batch, compression)
}

fn write_batch(
    batch: &arrow_array::RecordBatch,
    compression: Compression,
) -> anyhow::Result<Vec<u8>> {
    let schema = batch.schema();
    let codec = match compression {
        Compression::None => ParquetCompression::UNCOMPRESSED,
        Compression::Gzip => ParquetCompression::GZIP(GzipLevel::default()),
        Compression::Lz4 => ParquetCompression::LZ4_RAW,
        Compression::Zstd => ParquetCompression::ZSTD(ZstdLevel::default()),
    };
    let props = WriterProperties::builder().set_compression(codec).build();
    let mut writer = ArrowWriter::try_new(Vec::new(), schema, Some(props))?;
    writer.write(batch)?;
    Ok(writer.into_inner()?)
}

fn rows_to_batch(rows: &[serde_json::Value]) -> anyhow::Result<arrow_array::RecordBatch> {
    let schema =
        infer_json_schema_from_iterator(rows.iter().map(Ok)).context("infer parquet schema")?;
    if let Some(field) = schema
        .fields()
        .iter()
        .find(|field| widens_lossily(field, rows))
    {
        return Err(anyhow!(
            "field '{}' mixes floats with integers beyond 2^53",
            field.name()
        ));
    }
    let mut decoder = ReaderBuilder::new(Arc::new(schema))
        .with_batch_size(rows.len().max(1))
        .with_coerce_primitive(true)
        .build_decoder()?;
    decoder.serialize(rows).context("convert rows to arrow")?;
    decoder
        .flush()?
        .ok_or_else(|| anyhow!("no rows to encode as parquet"))
}

fn widens_lossily(field: &arrow_schema::Field, rows: &[serde_json::Value]) -> bool {
    field.data_type() == &arrow_schema::DataType::Float64
        && rows.iter().any(|row| {
            row.get(field.name()).is_some_and(|value| {
                value.as_i64().map(i64::unsigned_abs).or(value.as_u64()) > Some(F64_EXACT_INT)
            })
        })
}

/// Rewrites every top-level field that has no single lossless Arrow type as JSON text.
fn stringify_conflicting_fields(rows: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let names: std::collections::BTreeSet<&String> = rows
        .iter()
        .filter_map(serde_json::Value::as_object)
        .flat_map(|row| row.keys())
        .collect();
    let conflicting: Vec<&String> = names
        .into_iter()
        .filter(|name| {
            let column: Vec<serde_json::Value> = rows
                .iter()
                .filter_map(|row| row.get(name.as_str()))
                .map(|value| serde_json::json!({ name.as_str(): value }))
                .collect();
            match infer_json_schema_from_iterator(column.iter().map(Ok)) {
                Ok(schema) => schema
                    .fields()
                    .iter()
                    .any(|field| widens_lossily(field, &column)),
                Err(_) => true,
            }
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut row = row.clone();
            if let Some(object) = row.as_object_mut() {
                for name in &conflicting {
                    if let Some(value) = object.get_mut(name.as_str()) {
                        if !value.is_null() && !value.is_string() {
                            *value = serde_json::Value::String(value.to_string());
                        }
                    }
                }
            }
            row
        })
        .collect()
}

const ROWS_PER_CHUNK: usize = 64;

/// Output sink that refuses to grow past `remaining` bytes.
struct BoundedWriter {
    buf: Vec<u8>,
    remaining: u64,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let len = data.len() as u64;
        if len > self.remaining {
            return Err(std::io::Error::other("decoded parquet size limit exceeded"));
        }
        self.remaining -= len;
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decodes a Parquet body chunk by chunk into one message per row, each payload a JSON
/// object, so only the current chunk is held as JSON.
pub(crate) struct RowStream {
    /// The reader is not `Sync`; the mutex is only ever accessed through `get_mut`.
    reader: std::sync::Mutex<ParquetRecordBatchReader>,
    rows: usize,
    emitted: usize,
    max_chunk_bytes: Option<u64>,
}

impl RowStream {
    /// `max_chunk_bytes` bounds each decoded chunk, so a small object cannot expand without limit.
    pub(crate) fn open(data: bytes::Bytes, max_chunk_bytes: Option<u64>) -> anyhow::Result<Self> {
        // Small record batches keep the Arrow side of a decode bounded too, not just the JSON.
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(data)?.with_batch_size(ROWS_PER_CHUNK);
        let rows = usize::try_from(builder.metadata().file_metadata().num_rows())
            .context("parquet footer has a negative row count")?;
        Ok(Self {
            reader: std::sync::Mutex::new(builder.build()?),
            rows,
            emitted: 0,
            max_chunk_bytes,
        })
    }

    /// Row count from the file footer.
    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// Rows not handed out yet.
    pub(crate) fn unread(&self) -> usize {
        self.rows.saturating_sub(self.emitted)
    }

    /// The next chunk of rows; `None` once the body is exhausted.
    pub(crate) fn next_chunk(&mut self) -> Option<anyhow::Result<Vec<CanonicalMessage>>> {
        let reader = self
            .reader
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chunk = match reader.next()? {
            Ok(batch) => batch_to_messages(&batch, self.max_chunk_bytes),
            Err(e) => Err(e.into()),
        };
        if let Ok(messages) = &chunk {
            self.emitted += messages.len();
        }
        Some(chunk)
    }
}

fn batch_to_messages(
    batch: &arrow_array::RecordBatch,
    max_bytes: Option<u64>,
) -> anyhow::Result<Vec<CanonicalMessage>> {
    let mut writer = WriterBuilder::new()
        .with_explicit_nulls(true)
        .build::<_, LineDelimited>(BoundedWriter {
            buf: Vec::new(),
            remaining: max_bytes.unwrap_or(u64::MAX),
        });
    let written = writer.write(batch).and_then(|()| writer.finish());
    if written.is_err() {
        if let Some(limit) = max_bytes {
            return Err(anyhow!(
                "decoded parquet rows exceed {limit} bytes; raise max_object_bytes to read it"
            ));
        }
        written?;
    }
    let sink = writer.into_inner();
    let mut out = Vec::with_capacity(batch.num_rows());
    for line in sink.buf.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut msg = CanonicalMessage::new(line.to_vec(), None);
        msg.set_metadata("mq_bridge.original_format", "parquet");
        out.push(msg);
    }
    Ok(out)
}

/// Decodes a whole Parquet body at once.
#[cfg(test)]
pub(crate) fn decode_rows(
    data: Vec<u8>,
    max_chunk_bytes: Option<u64>,
) -> anyhow::Result<Vec<CanonicalMessage>> {
    let mut stream = RowStream::open(data.into(), max_chunk_bytes)?;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk() {
        out.extend(chunk?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_snappy_files_written_by_other_tools() {
        let rows = vec![serde_json::json!({"id": 7, "name": "x"})];
        let schema = Arc::new(infer_json_schema_from_iterator(rows.iter().map(Ok)).unwrap());
        let mut decoder = ReaderBuilder::new(schema.clone()).build_decoder().unwrap();
        decoder.serialize(&rows).unwrap();
        let batch = decoder.flush().unwrap().unwrap();
        let props = WriterProperties::builder()
            .set_compression(ParquetCompression::SNAPPY)
            .build();
        let mut writer = ArrowWriter::try_new(Vec::new(), schema, Some(props)).unwrap();
        writer.write(&batch).unwrap();

        let messages = decode_rows(writer.into_inner().unwrap(), None).unwrap();
        let row: serde_json::Value = serde_json::from_slice(&messages[0].payload).unwrap();
        assert_eq!(row, rows[0]);
    }

    #[test]
    fn keeps_large_integers_exact_next_to_floats() {
        let big = (1u64 << 53) + 1;
        let rows = vec![serde_json::json!({"n": 1.5}), serde_json::json!({"n": big})];
        let messages = decode_rows(encode_rows(&rows, Compression::None).unwrap(), None).unwrap();
        let row: serde_json::Value = serde_json::from_slice(&messages[1].payload).unwrap();
        assert_eq!(row["n"], big.to_string());
    }

    #[test]
    fn a_row_with_a_conflicting_shape_does_not_fail_the_batch() {
        let rows = vec![
            serde_json::json!({"id": 1, "v": 5}),
            serde_json::json!({"id": 2, "v": {"nested": true}}),
        ];
        let messages = decode_rows(encode_rows(&rows, Compression::None).unwrap(), None).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&messages[1].payload).unwrap();
        assert_eq!(second["id"], 2);
        assert_eq!(second["v"], r#"{"nested":true}"#);
    }

    #[test]
    fn decode_stops_at_the_byte_limit() {
        let rows: Vec<_> = (0..100).map(|i| serde_json::json!({"id": i})).collect();
        let body = encode_rows(&rows, Compression::None).unwrap();
        assert!(decode_rows(body, Some(64)).is_err());
    }

    fn buffer(rows: &[serde_json::Value]) -> RowBuffer {
        let mut buffer = RowBuffer::default();
        for row in rows {
            buffer.push(&serde_json::to_vec(row).unwrap()).unwrap();
        }
        buffer
    }

    fn decoded(body: Vec<u8>) -> Vec<serde_json::Value> {
        decode_rows(body, None)
            .unwrap()
            .iter()
            .map(|msg| serde_json::from_slice(&msg.payload).unwrap())
            .collect()
    }

    #[test]
    fn scalar_rows_decode_from_bytes_as_the_full_path_would() {
        let rows: Vec<_> = (0..SAMPLE_ROWS as i64 + 500)
            .map(|i| {
                let mut row = serde_json::json!({
                    "id": i - 100,
                    "amount": i as f64 + 0.25,
                    "name": format!("c\"{i}\n\u{e9}"),
                    "paid": i % 2 == 0,
                });
                if i % 7 == 0 {
                    row["note"] = serde_json::Value::Null;
                } else if i % 3 == 0 {
                    row["note"] = "late".into();
                }
                row
            })
            .collect();
        let buffer = buffer(&rows);
        assert!(buffer.decode_scalars().is_some());
        assert_eq!(
            decoded(buffer.encode(Compression::Zstd).unwrap()),
            decoded(encode_rows(&rows, Compression::Zstd).unwrap())
        );
    }

    #[test]
    fn a_kind_the_sample_did_not_show_is_not_truncated() {
        let mut rows: Vec<_> = (0..SAMPLE_ROWS)
            .map(|i| serde_json::json!({ "n": i }))
            .collect();
        rows.push(serde_json::json!({"n": 12.5}));
        let buffer = buffer(&rows);
        assert!(buffer.decode_scalars().is_none());
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows[SAMPLE_ROWS]["n"], 12.5);
    }

    #[test]
    fn a_large_integer_after_sampled_floats_stays_exact() {
        let big = (1u64 << 53) + 1;
        let mut rows = vec![serde_json::json!({"n": 1.5}); SAMPLE_ROWS];
        rows.push(serde_json::json!({ "n": big }));
        let buffer = buffer(&rows);
        assert!(buffer.decode_scalars().is_none());
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows[SAMPLE_ROWS]["n"], big.to_string());
    }

    #[test]
    fn a_rejected_payload_adds_no_row() {
        let mut buffer = RowBuffer::default();
        assert!(buffer.push(b"[1]").is_err());
        assert!(buffer.push(br#"{"x": "#).is_err());
        let pretty = serde_json::to_vec_pretty(&serde_json::json!({"a": 1, "b": "x"})).unwrap();
        buffer.push(&pretty).unwrap();
        assert!(buffer.decode_scalars().is_some());
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows, vec![serde_json::json!({"a": 1, "b": "x"})]);
    }

    #[test]
    fn a_payload_rejected_after_its_map_leaves_no_kind_behind() {
        let mut buffer = RowBuffer::default();
        buffer.push(br#"{"n":1}"#).unwrap();
        assert!(buffer.push(br#"{"n":1.5} trailing"#).is_err());
        for i in 1..SAMPLE_ROWS {
            buffer.push(format!(r#"{{"n":{i}}}"#).as_bytes()).unwrap();
        }
        buffer.push(br#"{"n":12.5}"#).unwrap();
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows[SAMPLE_ROWS]["n"], 12.5);
    }

    #[test]
    fn a_repeated_key_keeps_its_last_value() {
        let mut buffer = RowBuffer::default();
        buffer.push(br#"{"n":"x","n":1}"#).unwrap();
        assert!(buffer.decode_scalars().is_none());
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows, vec![serde_json::json!({"n": 1})]);
    }

    #[test]
    fn an_invalid_nested_value_is_rejected_on_push() {
        let mut buffer = RowBuffer::default();
        buffer.push(br#"{"id":1}"#).unwrap();
        assert!(buffer.push(b"{\"a\":{\"b\":\"\xff\"}}").is_err());
        assert!(buffer.push(br#"{"a":[1e400]}"#).is_err());
        let deep = format!(r#"{{"a":{}{}}}"#, "[".repeat(200), "]".repeat(200));
        assert!(buffer.push(deep.as_bytes()).is_err());
        let rows = decoded(buffer.encode(Compression::None).unwrap());
        assert_eq!(rows, vec![serde_json::json!({"id": 1})]);
    }

    #[test]
    fn keeps_null_fields_on_decode() {
        let rows = vec![serde_json::json!({"id": 1, "name": null})];
        let messages = decode_rows(encode_rows(&rows, Compression::None).unwrap(), None).unwrap();
        let row: serde_json::Value = serde_json::from_slice(&messages[0].payload).unwrap();
        assert_eq!(row, rows[0]);
    }
}
