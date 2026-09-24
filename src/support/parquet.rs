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
use arrow_json::reader::{infer_json_schema_from_iterator, ReaderBuilder};
use arrow_json::writer::{LineDelimited, WriterBuilder};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression as ParquetCompression, GzipLevel, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::sync::Arc;

/// Parses one payload as a Parquet row; anything but a JSON object is rejected.
pub(crate) fn parse_row(payload: &[u8]) -> anyhow::Result<serde_json::Value> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).context("parquet row payload is not valid JSON")?;
    if !value.is_object() {
        return Err(anyhow!("parquet row payload must be a JSON object"));
    }
    Ok(value)
}

/// Largest integer magnitude an `f64` holds exactly.
const F64_EXACT_INT: u64 = 1 << 53;

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
    let schema = batch.schema();

    let codec = match compression {
        Compression::None => ParquetCompression::UNCOMPRESSED,
        Compression::Gzip => ParquetCompression::GZIP(GzipLevel::default()),
        Compression::Lz4 => ParquetCompression::LZ4_RAW,
        Compression::Zstd => ParquetCompression::ZSTD(ZstdLevel::default()),
    };
    let props = WriterProperties::builder().set_compression(codec).build();
    let mut writer = ArrowWriter::try_new(Vec::new(), schema, Some(props))?;
    writer.write(&batch)?;
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

/// Decodes a Parquet body into one message per row, each payload a JSON object.
/// `max_bytes` bounds the decoded output total, so a small object cannot expand without limit.
pub(crate) fn decode_rows(
    data: Vec<u8>,
    max_bytes: Option<u64>,
) -> anyhow::Result<Vec<CanonicalMessage>> {
    // Small record batches keep the Arrow side of a decode bounded too, not just the JSON.
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(data))?
        .with_batch_size(ROWS_PER_CHUNK)
        .build()?;
    let mut out = Vec::new();
    let mut remaining = max_bytes.unwrap_or(u64::MAX);
    for batch in reader {
        let batch = batch?;
        let mut writer = WriterBuilder::new()
            .with_explicit_nulls(true)
            .build::<_, LineDelimited>(BoundedWriter {
                buf: Vec::new(),
                remaining,
            });
        let written = writer.write(&batch).and_then(|()| writer.finish());
        if written.is_err() {
            if let Some(limit) = max_bytes {
                return Err(anyhow!(
                    "decoded parquet rows exceed {limit} bytes; raise max_object_bytes to read it"
                ));
            }
            written?;
        }
        let sink = writer.into_inner();
        remaining = sink.remaining;
        for line in sink.buf.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut msg = CanonicalMessage::new(line.to_vec(), None);
            msg.metadata.insert(
                "mq_bridge.original_format".to_string(),
                "parquet".to_string(),
            );
            out.push(msg);
        }
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

    #[test]
    fn keeps_null_fields_on_decode() {
        let rows = vec![serde_json::json!({"id": 1, "name": null})];
        let messages = decode_rows(encode_rows(&rows, Compression::None).unwrap(), None).unwrap();
        let row: serde_json::Value = serde_json::from_slice(&messages[0].payload).unwrap();
        assert_eq!(row, rows[0]);
    }
}
