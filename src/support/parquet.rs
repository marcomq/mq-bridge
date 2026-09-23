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
use arrow_json::LineDelimitedWriter;
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

/// Encodes rows into one Parquet body. `compression` picks the column codec.
pub(crate) fn encode_rows(
    rows: &[serde_json::Value],
    compression: Compression,
) -> anyhow::Result<Vec<u8>> {
    let schema = Arc::new(
        infer_json_schema_from_iterator(rows.iter().map(Ok)).context("infer parquet schema")?,
    );
    let mut decoder = ReaderBuilder::new(schema.clone())
        .with_batch_size(rows.len().max(1))
        .with_coerce_primitive(true)
        .build_decoder()?;
    decoder.serialize(rows).context("convert rows to arrow")?;
    let batch = decoder
        .flush()?
        .ok_or_else(|| anyhow!("no rows to encode as parquet"))?;

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

/// Decodes a Parquet body into one message per row, each payload a JSON object.
pub(crate) fn decode_rows(data: Vec<u8>) -> anyhow::Result<Vec<CanonicalMessage>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(data))?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let mut writer = LineDelimitedWriter::new(Vec::new());
        writer.write_batches(&[&batch?])?;
        writer.finish()?;
        for line in writer.into_inner().split(|b| *b == b'\n') {
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

        let messages = decode_rows(writer.into_inner().unwrap()).unwrap();
        let row: serde_json::Value = serde_json::from_slice(&messages[0].payload).unwrap();
        assert_eq!(row, rows[0]);
    }
}
