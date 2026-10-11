//! Backend-agnostic ZeroMQ frame codec, shared by the zmq.rs (`zeromq`) and the
//! omq.rs (`zeromq-omq`) endpoints. It operates on plain frame vectors
//! (`Vec<bytes::Bytes>`) so each backend only has to convert to/from its own
//! wire message type; the framing/format rules live here and are exercised by
//! the unit tests in `zeromq.rs`.

use crate::models::{ZeroMqConfig, ZeroMqFormat, ZeroMqSocketType};
use crate::traits::PublisherError;
use crate::CanonicalMessage;
use anyhow::anyhow;
use bytes::Bytes;
use tracing::{debug, warn};

/// Build the wire frames for one message in `raw`/`raw_framed` mode. `raw` sends
/// the payload as a single frame; `raw_framed` prepends a JSON metadata frame so
/// the payload stays binary-safe while headers still travel. (`Json` is a
/// whole-batch format serialized by the caller, not per-message.)
pub(crate) fn encode_frames(
    message: &mut CanonicalMessage,
    format: &ZeroMqFormat,
) -> Result<Vec<Bytes>, PublisherError> {
    if matches!(format, ZeroMqFormat::RawFramed) {
        // Source/provenance keys are per-hop context and must not be forwarded.
        message.strip_source_metadata();
        let meta = serde_json::to_vec(&message.metadata)
            .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
        Ok(vec![Bytes::from(meta), message.payload.clone()])
    } else {
        Ok(vec![message.payload.clone()])
    }
}

/// The topic frame a PUB socket sends first: SUB peers filter on it and drop it on receive.
/// `raw_framed` always has one, or a SUB peer would take the metadata frame for the topic.
pub(crate) fn pub_topic(config: &ZeroMqConfig) -> Option<Bytes> {
    if config.socket_type != Some(ZeroMqSocketType::Pub) {
        return None;
    }
    match &config.topic {
        Some(topic) => Some(Bytes::from(topic.clone())),
        None => matches!(config.format, ZeroMqFormat::RawFramed).then(Bytes::new),
    }
}

pub(crate) fn with_topic(topic: &Option<Bytes>, mut frames: Vec<Bytes>) -> Vec<Bytes> {
    if let Some(topic) = topic {
        frames.insert(0, topic.clone());
    }
    frames
}

/// Decode the frames of one received ZMQ message into canonical messages,
/// honouring the configured `format`. `is_sub` marks SUB traffic, whose leading
/// frame is the subscription topic rather than payload.
pub(crate) fn decode_frames(
    frames: Vec<Bytes>,
    is_sub: bool,
    format: &ZeroMqFormat,
) -> anyhow::Result<Vec<CanonicalMessage>> {
    let payload = frames.last().cloned().unwrap_or_default();
    // Only short-circuit genuinely empty single-frame inputs. Multipart Raw/RawFramed
    // messages may carry a trailing empty payload (or leading metadata) that still
    // needs to be emitted, so don't drop them here.
    if payload.is_empty() && frames.len() <= 1 {
        return Ok(vec![]);
    }
    // Only SUB traffic prepends the subscription topic as a leading frame; PUSH/PULL and
    // REP carry payload frames only, so a leading frame there is not a topic. This holds for
    // raw_framed too: on SUB the topic still comes first, ahead of the metadata frame, so it
    // is skipped before metadata parsing below.
    let topic = if is_sub && frames.len() > 1 {
        Some(String::from_utf8_lossy(frames[0].as_ref()).into_owned())
    } else {
        None
    };
    let mut messages = match format {
        // Opaque bytes; never JSON-decode. A raw producer may send multipart, so
        // emit one message per payload frame (dropping the SUB topic frame).
        ZeroMqFormat::Raw => {
            let payload_frames = if is_sub && frames.len() > 1 {
                &frames[1..]
            } else {
                &frames[..]
            };
            payload_frames
                .iter()
                .map(|f| CanonicalMessage::new_bytes(f.clone(), None))
                .collect()
        }
        // Two-frame layout: [metadata JSON, raw payload]. Degrade to payload-only
        // when a single frame arrives (e.g. a plain raw producer).
        ZeroMqFormat::RawFramed => {
            // Skip the SUB subscription-topic frame first, so the [metadata, payload]
            // framing below lines up rather than mis-reading the topic as metadata.
            let framed: &[Bytes] = if is_sub && frames.len() > 1 {
                &frames[1..]
            } else {
                &frames[..]
            };
            let payload = framed.last().cloned().unwrap_or_default();
            let mut msg = CanonicalMessage::new_bytes(payload, None);
            if framed.len() > 1 {
                match serde_json::from_slice::<std::collections::HashMap<String, String>>(
                    framed[0].as_ref(),
                ) {
                    Ok(meta) => msg.metadata = meta,
                    Err(e) => warn!(
                        is_sub,
                        frames = frames.len(),
                        error = %e,
                        "zeromq raw_framed: leading frame is not valid JSON metadata; treating the message as payload-only. Check the producer's framing."
                    ),
                }
                if framed.len() > 2 {
                    debug!(
                        is_sub,
                        discarded = framed.len() - 2,
                        "zeromq raw_framed: discarding frames beyond [metadata, payload]; a raw_framed message carries at most two payload frames. Check the producer's framing."
                    );
                }
            }
            vec![msg]
        }
        ZeroMqFormat::Json => {
            if let Ok(messages) = serde_json::from_slice::<Vec<CanonicalMessage>>(&payload) {
                messages
            } else if let Ok(message) = serde_json::from_slice::<CanonicalMessage>(&payload) {
                vec![message]
            } else {
                vec![CanonicalMessage::new(payload.to_vec(), None)]
            }
        }
    };
    for message in &mut messages {
        // Never let a spoofed `mqb.src.*` key in the inbound payload survive;
        // the authoritative topic cursor (SUB only) is injected below.
        message.strip_source_metadata();
    }
    // Opt-in via the MQB_SOURCE_METADATA env var; off by default.
    if crate::canonical_message::source_metadata_enabled() {
        if let Some(topic) = topic {
            for message in &mut messages {
                message
                    .metadata
                    .insert("mqb.src.zeromq_topic".to_string(), topic.clone());
            }
        }
    }
    Ok(messages)
}

/// Framing properties shared by both ZeroMQ backends. `raw_framed` is the default format, so
/// what survives the frame boundary — payload bytes and headers — is the contract producers see.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn metadata() -> impl Strategy<Value = HashMap<String, String>> {
        // `mqb.src.*` is stripped by design on both sides, so keep generated keys clear of it.
        prop::collection::hash_map("[a-z]{1,8}", "[ -~]{0,32}", 0..6)
    }

    proptest! {
        #[test]
        fn raw_framed_round_trips_payload_and_metadata(
            payload in prop::collection::vec(any::<u8>(), 0..512),
            meta in metadata(),
        ) {
            let mut msg = CanonicalMessage::new(payload.clone(), None);
            msg.metadata = meta.clone();
            let frames = encode_frames(&mut msg, &ZeroMqFormat::RawFramed).unwrap();
            prop_assert_eq!(frames.len(), 2);

            let decoded = decode_frames(frames, false, &ZeroMqFormat::RawFramed).unwrap();
            prop_assert_eq!(decoded.len(), 1);
            prop_assert_eq!(decoded[0].payload.as_ref(), payload.as_slice());
            prop_assert_eq!(&decoded[0].metadata, &meta);
        }

        /// `raw` carries bytes only; headers are dropped by design.
        #[test]
        fn raw_round_trips_the_payload_verbatim(
            payload in prop::collection::vec(any::<u8>(), 1..512),
            meta in metadata(),
        ) {
            let mut msg = CanonicalMessage::new(payload.clone(), None);
            msg.metadata = meta;
            let frames = encode_frames(&mut msg, &ZeroMqFormat::Raw).unwrap();
            prop_assert_eq!(frames.len(), 1);

            let decoded = decode_frames(frames, false, &ZeroMqFormat::Raw).unwrap();
            prop_assert_eq!(decoded.len(), 1);
            prop_assert_eq!(decoded[0].payload.as_ref(), payload.as_slice());
            prop_assert!(decoded[0].metadata.is_empty());
        }

        /// Arbitrary inbound frames come from other people's producers: decoding must return a
        /// result either way, never panic.
        #[test]
        fn decoding_arbitrary_frames_never_panics(
            frames in prop::collection::vec(
                prop::collection::vec(any::<u8>(), 0..64).prop_map(Bytes::from),
                0..4,
            ),
            is_sub in any::<bool>(),
        ) {
            for format in [ZeroMqFormat::Raw, ZeroMqFormat::RawFramed, ZeroMqFormat::Json] {
                let _ = decode_frames(frames.clone(), is_sub, &format);
            }
        }
    }
}
