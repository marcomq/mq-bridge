//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Transport-level batch envelope: N logical [`CanonicalMessage`]s in one
//! physical message, and back.
//!
//! # `mqb` wire format (version 1)
//!
//! A plaintext header followed by an optionally compressed body — the same shape
//! as a Kafka `RecordBatch` or an Avro object-container block, so the reader can
//! learn the codec before it decodes anything.
//!
//! ```text
//! header (8 bytes, never compressed)
//!   0..4  magic    b"MQB1"
//!   4     version  u8   = 1
//!   5     codec    u8   0 none | 1 gzip | 2 lz4 | 3 zstd
//!   6..8  flags    u16 LE   bit0 records carry metadata, bit1 records carry message_id
//!
//! body (compressed with `codec` as one self-contained member)
//!   count    uvarint
//!   count x record
//!
//! record
//!   [bit1] message_id  16 bytes LE
//!   payload_len        uvarint
//!   payload            payload_len bytes
//!   [bit0] meta_count  uvarint
//!   [bit0] meta_count x (key_len uvarint, key, value_len uvarint, value)
//! ```
//!
//! Record order is the message order. Lengths are LEB128 so a 60-byte CSV row
//! costs one framing byte, not four.
//!
//! # `benthos_binary`
//!
//! The layout Redpanda Connect's `archive: binary` / `unarchive: binary` writes:
//! `u32 BE count`, then `u32 BE len` + payload per part. Payloads only — metadata
//! and ids have nowhere to go — and no envelope header, so compression has to be
//! composed around it with the `compression` middleware.

use crate::models::{Compression, PackFormat};
use crate::CanonicalMessage;
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use std::collections::HashMap;

const MAGIC: &[u8; 4] = b"MQB1";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 8;

const FLAG_METADATA: u16 = 1 << 0;
const FLAG_MESSAGE_ID: u16 = 1 << 1;

const CODEC_NONE: u8 = 0;
const CODEC_GZIP: u8 = 1;
const CODEC_LZ4: u8 = 2;
const CODEC_ZSTD: u8 = 3;

fn codec_id(algo: Compression) -> u8 {
    match algo {
        Compression::None => CODEC_NONE,
        Compression::Gzip => CODEC_GZIP,
        Compression::Lz4 => CODEC_LZ4,
        Compression::Zstd => CODEC_ZSTD,
    }
}

fn codec_from_id(id: u8) -> Result<Compression> {
    Ok(match id {
        CODEC_NONE => Compression::None,
        CODEC_GZIP => Compression::Gzip,
        CODEC_LZ4 => Compression::Lz4,
        CODEC_ZSTD => Compression::Zstd,
        other => bail!("packed batch uses unknown compression codec {other}"),
    })
}

// --- LEB128 ---

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn uvarint_len(value: u64) -> usize {
    let bits = 64 - value.leading_zeros().min(63);
    (bits as usize).div_ceil(7).max(1)
}

fn get_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| anyhow!("packed batch is truncated inside a length prefix"))?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("packed batch has an overlong length prefix");
        }
    }
}

/// Reads `len` bytes as a zero-copy slice of the body.
fn take<'a>(body: &'a Bytes, pos: &mut usize, len: usize, what: &str) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| anyhow!("packed batch declares an impossible {what} length"))?;
    if end > body.len() {
        bail!(
            "packed batch is truncated: {what} wants {len} bytes, {} remain",
            body.len().saturating_sub(*pos)
        );
    }
    let slice = &body[*pos..end];
    *pos = end;
    Ok(slice)
}

// --- packing ---

/// How a batch is framed. Built once from config and reused for every batch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Packer {
    format: PackFormat,
    codec: Compression,
    include_message_id: bool,
}

impl Packer {
    pub(crate) fn new(
        format: PackFormat,
        codec: Compression,
        include_message_id: bool,
    ) -> Result<Self> {
        if format == PackFormat::BenthosBinary && codec != Compression::None {
            bail!(
                "pack format `benthos_binary` has no envelope header to record a codec in, so \
                 `compression` must be left unset; put a `compression` middleware after `pack` instead"
            );
        }
        #[cfg(not(feature = "compression"))]
        if codec != Compression::None {
            bail!("pack `compression` needs the `compression` feature to be enabled");
        }
        Ok(Self {
            format,
            codec,
            include_message_id: include_message_id && format == PackFormat::Mqb,
        })
    }

    /// Uncompressed body bytes this message will add to a batch. The chunker adds
    /// these up to honour `max_bytes` without serializing anything twice.
    pub(crate) fn record_len(&self, message: &CanonicalMessage) -> usize {
        match self.format {
            PackFormat::BenthosBinary => 4 + message.payload.len(),
            PackFormat::Mqb => {
                let mut len = uvarint_len(message.payload.len() as u64) + message.payload.len();
                if self.include_message_id {
                    len += 16;
                }
                if !message.metadata.is_empty() {
                    len += uvarint_len(message.metadata.len() as u64);
                    for (key, value) in &message.metadata {
                        len += uvarint_len(key.len() as u64) + key.len();
                        len += uvarint_len(value.len() as u64) + value.len();
                    }
                }
                len
            }
        }
    }

    /// Frames `messages` into one physical payload. Borrows throughout: nothing
    /// here clones a payload or a metadata map.
    pub(crate) fn pack(&self, messages: &[CanonicalMessage]) -> Result<Bytes> {
        match self.format {
            PackFormat::BenthosBinary => Ok(self.pack_benthos(messages)),
            PackFormat::Mqb => self.pack_mqb(messages),
        }
    }

    fn pack_benthos(&self, messages: &[CanonicalMessage]) -> Bytes {
        let size = 4 + messages.iter().map(|m| 4 + m.payload.len()).sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&(messages.len() as u32).to_be_bytes());
        for message in messages {
            out.extend_from_slice(&(message.payload.len() as u32).to_be_bytes());
            out.extend_from_slice(&message.payload);
        }
        out.into()
    }

    fn pack_mqb(&self, messages: &[CanonicalMessage]) -> Result<Bytes> {
        let with_metadata = messages.iter().any(|m| !m.metadata.is_empty());
        let mut flags = 0u16;
        if with_metadata {
            flags |= FLAG_METADATA;
        }
        if self.include_message_id {
            flags |= FLAG_MESSAGE_ID;
        }

        let body_len = uvarint_len(messages.len() as u64)
            + messages.iter().map(|m| self.record_len(m)).sum::<usize>();

        // Leave room for the header up front so the uncompressed path needs one
        // allocation and no second copy.
        let mut buffer = Vec::with_capacity(HEADER_LEN + body_len);
        buffer.extend_from_slice(&[0; HEADER_LEN]);
        put_uvarint(&mut buffer, messages.len() as u64);
        for message in messages {
            if self.include_message_id {
                buffer.extend_from_slice(&message.message_id.to_le_bytes());
            }
            put_uvarint(&mut buffer, message.payload.len() as u64);
            buffer.extend_from_slice(&message.payload);
            if with_metadata {
                put_uvarint(&mut buffer, message.metadata.len() as u64);
                for (key, value) in &message.metadata {
                    put_uvarint(&mut buffer, key.len() as u64);
                    buffer.extend_from_slice(key.as_bytes());
                    put_uvarint(&mut buffer, value.len() as u64);
                    buffer.extend_from_slice(value.as_bytes());
                }
            }
        }

        let mut out = if self.codec == Compression::None {
            buffer
        } else {
            #[cfg(feature = "compression")]
            {
                let member = crate::support::compression::compress_member(
                    self.codec,
                    &buffer[HEADER_LEN..],
                )?;
                let mut out = Vec::with_capacity(HEADER_LEN + member.len());
                out.extend_from_slice(&[0; HEADER_LEN]);
                out.extend_from_slice(&member);
                out
            }
            #[cfg(not(feature = "compression"))]
            unreachable!("Packer::new rejects a codec without the compression feature")
        };

        out[..4].copy_from_slice(MAGIC);
        out[4] = VERSION;
        out[5] = codec_id(self.codec);
        out[6..8].copy_from_slice(&flags.to_le_bytes());
        Ok(out.into())
    }
}

// --- unpacking ---

/// Bounds applied to a batch that arrived over the wire. Both are unset by
/// default; a hostile or corrupt envelope is otherwise only bounded by its own
/// declared sizes.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UnpackLimits {
    pub(crate) max_messages: Option<usize>,
    pub(crate) max_decompressed_bytes: Option<u64>,
}

/// Splits one physical payload back into its logical messages.
///
/// Every failure is permanent: malformed bytes never become well-formed on a
/// re-read.
pub(crate) fn unpack(
    format: PackFormat,
    payload: &Bytes,
    limits: &UnpackLimits,
) -> Result<Vec<CanonicalMessage>> {
    match format {
        PackFormat::BenthosBinary => unpack_benthos(payload, limits),
        PackFormat::Mqb => unpack_mqb(payload, limits),
    }
}

fn check_count(count: u64, remaining: usize, limits: &UnpackLimits) -> Result<usize> {
    // Each record needs at least one byte, so the body length caps the count. This
    // is what keeps a bogus header from driving a huge `with_capacity`.
    if count > remaining as u64 {
        bail!("packed batch declares {count} messages but only {remaining} bytes follow");
    }
    let count = count as usize;
    if let Some(max) = limits.max_messages {
        if count > max {
            bail!(
                "packed batch holds {count} messages, above the configured max_messages of {max}"
            );
        }
    }
    Ok(count)
}

fn unpack_benthos(payload: &Bytes, limits: &UnpackLimits) -> Result<Vec<CanonicalMessage>> {
    if payload.len() < 4 {
        bail!("benthos_binary batch is shorter than its 4-byte count prefix");
    }
    let count = u32::from_be_bytes(payload[..4].try_into().expect("4 bytes"));
    let count = check_count(u64::from(count), payload.len() - 4, limits)?;

    let mut pos = 4;
    let mut messages = Vec::with_capacity(count);
    for _ in 0..count {
        let len_bytes = take(payload, &mut pos, 4, "record length")?;
        let len = u32::from_be_bytes(len_bytes.try_into().expect("4 bytes")) as usize;
        let start = pos;
        take(payload, &mut pos, len, "payload")?;
        messages.push(CanonicalMessage::new_bytes(payload.slice(start..pos), None));
    }
    Ok(messages)
}

fn unpack_mqb(payload: &Bytes, limits: &UnpackLimits) -> Result<Vec<CanonicalMessage>> {
    if payload.len() < HEADER_LEN {
        bail!("packed batch is shorter than the {HEADER_LEN}-byte envelope header");
    }
    if &payload[..4] != MAGIC {
        bail!("payload is not an mq-bridge packed batch (bad magic); is `pack` configured on the sending side?");
    }
    let version = payload[4];
    if version != VERSION {
        bail!("packed batch is format version {version}, this build understands {VERSION}");
    }
    let codec = codec_from_id(payload[5])?;
    let flags = u16::from_le_bytes(payload[6..8].try_into().expect("2 bytes"));
    let with_metadata = flags & FLAG_METADATA != 0;
    let with_message_id = flags & FLAG_MESSAGE_ID != 0;

    let body = if codec == Compression::None {
        // Zero-copy: record payloads become slices of the message that arrived.
        payload.slice(HEADER_LEN..)
    } else {
        #[cfg(feature = "compression")]
        {
            Bytes::from(crate::support::compression::decompress_all(
                codec,
                &payload[HEADER_LEN..],
                limits.max_decompressed_bytes,
            )?)
        }
        #[cfg(not(feature = "compression"))]
        bail!("packed batch is {codec:?}-compressed; rebuild with the `compression` feature")
    };

    let mut pos = 0usize;
    let count = get_uvarint(&body, &mut pos)?;
    let count = check_count(count, body.len() - pos, limits)?;

    let mut messages = Vec::with_capacity(count);
    for _ in 0..count {
        let message_id = if with_message_id {
            let raw = take(&body, &mut pos, 16, "message id")?;
            Some(u128::from_le_bytes(raw.try_into().expect("16 bytes")))
        } else {
            None
        };

        let payload_len = get_uvarint(&body, &mut pos)? as usize;
        let start = pos;
        take(&body, &mut pos, payload_len, "payload")?;
        let mut message = CanonicalMessage::new_bytes(body.slice(start..pos), message_id);

        if with_metadata {
            let pairs = get_uvarint(&body, &mut pos)? as usize;
            if pairs > body.len() - pos {
                bail!("packed record declares {pairs} metadata pairs but the batch is shorter");
            }
            let mut metadata = HashMap::with_capacity(pairs);
            for _ in 0..pairs {
                let key_len = get_uvarint(&body, &mut pos)? as usize;
                let key = std::str::from_utf8(take(&body, &mut pos, key_len, "metadata key")?)?;
                let value_len = get_uvarint(&body, &mut pos)? as usize;
                let value =
                    std::str::from_utf8(take(&body, &mut pos, value_len, "metadata value")?)?;
                metadata.insert(key.to_string(), value.to_string());
            }
            message.metadata = metadata;
        }
        messages.push(message);
    }
    if pos != body.len() {
        bail!(
            "packed batch has {} trailing bytes after its {count} records",
            body.len() - pos
        );
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(payload: &str, metadata: &[(&str, &str)]) -> CanonicalMessage {
        let mut message = CanonicalMessage::from(payload);
        for (key, value) in metadata {
            message
                .metadata
                .insert((*key).to_string(), (*value).to_string());
        }
        message
    }

    fn packer(codec: Compression) -> Packer {
        Packer::new(PackFormat::Mqb, codec, true).unwrap()
    }

    fn codecs() -> Vec<Compression> {
        if cfg!(feature = "compression") {
            vec![
                Compression::None,
                Compression::Gzip,
                Compression::Lz4,
                Compression::Zstd,
            ]
        } else {
            vec![Compression::None]
        }
    }

    #[test]
    fn uvarint_length_matches_what_is_written() {
        let mut buffer = Vec::new();
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            buffer.clear();
            put_uvarint(&mut buffer, value);
            assert_eq!(buffer.len(), uvarint_len(value), "value {value}");
            let mut pos = 0;
            assert_eq!(get_uvarint(&buffer, &mut pos).unwrap(), value);
            assert_eq!(pos, buffer.len());
        }
    }

    #[test]
    fn an_empty_batch_round_trips() {
        for codec in codecs() {
            let packed = packer(codec).pack(&[]).unwrap();
            let out = unpack(PackFormat::Mqb, &packed, &UnpackLimits::default()).unwrap();
            assert!(out.is_empty(), "codec {codec:?}");
        }
    }

    #[test]
    fn a_single_message_round_trips() {
        for codec in codecs() {
            let input = vec![message("only one", &[("kind", "note")])];
            let packed = packer(codec).pack(&input).unwrap();
            let out = unpack(PackFormat::Mqb, &packed, &UnpackLimits::default()).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].payload, input[0].payload);
            assert_eq!(out[0].metadata, input[0].metadata);
            assert_eq!(out[0].message_id, input[0].message_id);
        }
    }

    #[test]
    fn many_messages_keep_payload_metadata_and_order() {
        for codec in codecs() {
            let input: Vec<CanonicalMessage> = (0..500)
                .map(|i| {
                    message(
                        &format!("row-{i}"),
                        &[("seq", &i.to_string()), ("kind", "row")],
                    )
                })
                .collect();
            let packed = packer(codec).pack(&input).unwrap();
            let out = unpack(PackFormat::Mqb, &packed, &UnpackLimits::default()).unwrap();

            assert_eq!(out.len(), input.len(), "codec {codec:?}");
            for (i, (got, want)) in out.iter().zip(&input).enumerate() {
                assert_eq!(got.payload, want.payload, "record {i} codec {codec:?}");
                assert_eq!(got.metadata, want.metadata, "record {i} codec {codec:?}");
                assert_eq!(got.message_id, want.message_id, "record {i}");
            }
        }
    }

    /// Payload bytes come straight out of the physical message with no copy.
    #[test]
    fn the_uncompressed_path_slices_rather_than_copies() {
        let input = vec![message("a payload worth pointing at", &[])];
        let packed = packer(Compression::None).pack(&input).unwrap();
        let out = unpack(PackFormat::Mqb, &packed, &UnpackLimits::default()).unwrap();
        let start = out[0].payload.as_ptr() as usize - packed.as_ptr() as usize;
        assert_eq!(
            &packed[start..start + out[0].payload.len()],
            &out[0].payload
        );
    }

    #[test]
    fn a_batch_without_metadata_writes_no_metadata_frames() {
        let bare = packer(Compression::None)
            .pack(&[message("x", &[])])
            .unwrap();
        let tagged = packer(Compression::None)
            .pack(&[message("x", &[("k", "v")])])
            .unwrap();
        assert_eq!(u16::from_le_bytes([bare[6], bare[7]]) & FLAG_METADATA, 0);
        assert_ne!(
            u16::from_le_bytes([tagged[6], tagged[7]]) & FLAG_METADATA,
            0
        );
        assert!(bare.len() < tagged.len());
        let out = unpack(PackFormat::Mqb, &bare, &UnpackLimits::default()).unwrap();
        assert!(out[0].metadata.is_empty());
    }

    #[test]
    fn message_ids_can_be_left_out() {
        let input = vec![message("dense", &[])];
        let dense = Packer::new(PackFormat::Mqb, Compression::None, false)
            .unwrap()
            .pack(&input)
            .unwrap();
        let full = packer(Compression::None).pack(&input).unwrap();
        assert_eq!(full.len(), dense.len() + 16);
        let out = unpack(PackFormat::Mqb, &dense, &UnpackLimits::default()).unwrap();
        assert_eq!(out[0].payload, input[0].payload);
        assert_ne!(
            out[0].message_id, input[0].message_id,
            "a fresh id is minted"
        );
    }

    /// Truncation must never silently deliver a short batch. The record count sits
    /// at the head of the body, so any lost record byte fails the parse; a cut that
    /// only removes an lz4 frame's end mark still yields the whole batch, which the
    /// shared lz4 reader tolerates by design.
    #[test]
    fn a_truncated_batch_never_decodes_to_a_partial_batch() {
        for codec in codecs() {
            let input = vec![message("first", &[]), message("second", &[("k", "v")])];
            let packed = packer(codec).pack(&input).unwrap();
            for cut in 0..packed.len() {
                let short = packed.slice(..cut);
                let Ok(out) = unpack(PackFormat::Mqb, &short, &UnpackLimits::default()) else {
                    continue;
                };
                assert_eq!(out.len(), input.len(), "codec {codec:?} cut at {cut}");
                for (got, want) in out.iter().zip(&input) {
                    assert_eq!(got.payload, want.payload, "codec {codec:?} cut at {cut}");
                    assert_eq!(got.metadata, want.metadata, "codec {codec:?} cut at {cut}");
                }
            }
        }
    }

    /// Losing a record's bytes — rather than only a codec trailer — is always caught.
    #[test]
    fn a_batch_missing_record_bytes_is_rejected() {
        let input: Vec<CanonicalMessage> = (0..20)
            .map(|i| message(&format!("record number {i}"), &[("k", "v")]))
            .collect();
        let packed = packer(Compression::None).pack(&input).unwrap();
        for cut in HEADER_LEN..packed.len() {
            assert!(
                unpack(
                    PackFormat::Mqb,
                    &packed.slice(..cut),
                    &UnpackLimits::default()
                )
                .is_err(),
                "truncated to {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn trailing_bytes_after_the_records_are_rejected() {
        let mut packed = packer(Compression::None)
            .pack(&[message("x", &[])])
            .unwrap()
            .to_vec();
        packed.extend_from_slice(b"junk");
        let error = unpack(PackFormat::Mqb, &packed.into(), &UnpackLimits::default()).unwrap_err();
        assert!(error.to_string().contains("trailing bytes"), "{error}");
    }

    #[test]
    fn a_foreign_payload_is_rejected() {
        let junk = Bytes::from_static(b"just a plain message, not a batch");
        let error = unpack(PackFormat::Mqb, &junk, &UnpackLimits::default()).unwrap_err();
        assert!(error.to_string().contains("bad magic"), "{error}");
    }

    #[test]
    fn an_unknown_format_version_is_rejected() {
        let mut packed = packer(Compression::None)
            .pack(&[message("x", &[])])
            .unwrap()
            .to_vec();
        packed[4] = 99;
        let error = unpack(PackFormat::Mqb, &packed.into(), &UnpackLimits::default()).unwrap_err();
        assert!(error.to_string().contains("version 99"), "{error}");
    }

    #[test]
    fn an_unknown_codec_is_rejected() {
        let mut packed = packer(Compression::None)
            .pack(&[message("x", &[])])
            .unwrap()
            .to_vec();
        packed[5] = 42;
        let error = unpack(PackFormat::Mqb, &packed.into(), &UnpackLimits::default()).unwrap_err();
        assert!(error.to_string().contains("codec 42"), "{error}");
    }

    /// A bogus count must not drive a huge allocation before the body is read.
    #[test]
    fn an_absurd_message_count_is_rejected() {
        let mut body = Vec::new();
        put_uvarint(&mut body, u32::MAX as u64);
        let mut packed = vec![0u8; HEADER_LEN];
        packed[..4].copy_from_slice(MAGIC);
        packed[4] = VERSION;
        packed.extend_from_slice(&body);
        let error = unpack(PackFormat::Mqb, &packed.into(), &UnpackLimits::default()).unwrap_err();
        assert!(error.to_string().contains("bytes follow"), "{error}");
    }

    #[test]
    fn max_messages_caps_an_oversized_batch() {
        let input: Vec<CanonicalMessage> = (0..10).map(|i| message(&i.to_string(), &[])).collect();
        let packed = packer(Compression::None).pack(&input).unwrap();
        let limits = UnpackLimits {
            max_messages: Some(4),
            ..Default::default()
        };
        assert!(unpack(PackFormat::Mqb, &packed, &limits).is_err());
        let limits = UnpackLimits {
            max_messages: Some(10),
            ..Default::default()
        };
        assert_eq!(unpack(PackFormat::Mqb, &packed, &limits).unwrap().len(), 10);
    }

    #[cfg(feature = "compression")]
    #[test]
    fn the_decompressed_size_guard_applies_to_the_body() {
        let input: Vec<CanonicalMessage> = (0..200)
            .map(|i| message(&format!("a fairly repetitive row number {i}"), &[]))
            .collect();
        let packed = packer(Compression::Zstd).pack(&input).unwrap();
        let limits = UnpackLimits {
            max_decompressed_bytes: Some(64),
            ..Default::default()
        };
        assert!(unpack(PackFormat::Mqb, &packed, &limits).is_err());
    }

    #[cfg(feature = "compression")]
    #[test]
    fn compression_shrinks_a_repetitive_batch() {
        let input: Vec<CanonicalMessage> = (0..1000)
            .map(|i| {
                message(
                    &format!("{{\"id\":{i},\"status\":\"ok\",\"region\":\"eu\"}}"),
                    &[],
                )
            })
            .collect();
        let plain = packer(Compression::None).pack(&input).unwrap();
        for codec in [Compression::Gzip, Compression::Lz4, Compression::Zstd] {
            let squeezed = packer(codec).pack(&input).unwrap();
            assert!(squeezed.len() < plain.len() / 2, "codec {codec:?}");
            assert_eq!(
                unpack(PackFormat::Mqb, &squeezed, &UnpackLimits::default())
                    .unwrap()
                    .len(),
                input.len()
            );
        }
    }

    #[test]
    fn benthos_binary_matches_the_documented_layout() {
        let input = vec![message("hello", &[]), message("world", &[("k", "v")])];
        let packed = Packer::new(PackFormat::BenthosBinary, Compression::None, true)
            .unwrap()
            .pack(&input)
            .unwrap();
        assert_eq!(
            packed.as_ref(),
            b"\x00\x00\x00\x02\x00\x00\x00\x05hello\x00\x00\x00\x05world"
        );

        let out = unpack(PackFormat::BenthosBinary, &packed, &UnpackLimits::default()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].payload.as_ref(), b"hello");
        assert_eq!(out[1].payload.as_ref(), b"world");
        assert!(out[1].metadata.is_empty(), "the format carries no metadata");
    }

    #[test]
    fn benthos_binary_rejects_a_truncated_blob() {
        let packed = Packer::new(PackFormat::BenthosBinary, Compression::None, true)
            .unwrap()
            .pack(&[message("hello", &[])])
            .unwrap();
        for cut in 0..packed.len() {
            assert!(unpack(
                PackFormat::BenthosBinary,
                &packed.slice(..cut),
                &UnpackLimits::default()
            )
            .is_err());
        }
    }

    #[test]
    fn benthos_binary_refuses_an_inner_codec() {
        let error = Packer::new(PackFormat::BenthosBinary, Compression::Zstd, true).unwrap_err();
        assert!(error.to_string().contains("no envelope header"), "{error}");
    }
}

/// Round-trip properties of the envelope. `pack`/`unpack` sit between two
/// processes, so "whatever went in comes back" and "nothing panics on hostile
/// bytes" are the two invariants everything downstream rests on.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn messages() -> impl Strategy<Value = Vec<CanonicalMessage>> {
        let metadata = prop::collection::hash_map("[a-z]{1,8}", "[ -~]{0,32}", 0..4);
        prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 0..256), metadata).prop_map(
                |(payload, metadata)| {
                    let mut message = CanonicalMessage::new(payload, None);
                    message.metadata = metadata;
                    message
                },
            ),
            0..32,
        )
    }

    fn codecs() -> impl Strategy<Value = Compression> {
        #[cfg(feature = "compression")]
        {
            prop_oneof![
                Just(Compression::None),
                Just(Compression::Gzip),
                Just(Compression::Lz4),
                Just(Compression::Zstd),
            ]
        }
        #[cfg(not(feature = "compression"))]
        Just(Compression::None)
    }

    proptest! {
        #[test]
        fn a_batch_round_trips(input in messages(), codec in codecs()) {
            let packed = Packer::new(PackFormat::Mqb, codec, true).unwrap().pack(&input).unwrap();
            let out = unpack(PackFormat::Mqb, &packed, &UnpackLimits::default()).unwrap();
            prop_assert_eq!(out.len(), input.len());
            for (got, want) in out.iter().zip(&input) {
                prop_assert_eq!(&got.payload, &want.payload);
                prop_assert_eq!(&got.metadata, &want.metadata);
                prop_assert_eq!(got.message_id, want.message_id);
            }
        }

        /// Arbitrary inbound bytes come from someone else's producer: unpacking must
        /// return a result either way, never panic and never over-allocate.
        #[test]
        fn unpacking_arbitrary_bytes_never_panics(
            raw in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let bytes = Bytes::from(raw);
            for format in [PackFormat::Mqb, PackFormat::BenthosBinary] {
                let _ = unpack(format, &bytes, &UnpackLimits::default());
            }
        }

        /// The same, but starting from a well-formed envelope so the parser gets
        /// past the header and corruption lands in the record stream.
        #[test]
        fn a_corrupted_envelope_never_panics(
            input in messages(),
            index in any::<prop::sample::Index>(),
            mask in 1u8..=255,
        ) {
            let packed = Packer::new(PackFormat::Mqb, Compression::None, true)
                .unwrap()
                .pack(&input)
                .unwrap();
            let mut bytes = packed.to_vec();
            let at = index.index(bytes.len());
            bytes[at] ^= mask;
            let _ = unpack(PackFormat::Mqb, &bytes.into(), &UnpackLimits::default());
        }
    }
}
