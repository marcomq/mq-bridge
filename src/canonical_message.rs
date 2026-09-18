//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::type_handler::KIND_KEY;

/// The unified message format.
///
/// `Serialize`/`Deserialize` are hand-written (see below) because the derived JSON
/// form wrote the payload as a per-byte array (`[123,34,…]`) — ~3.9x the payload
/// size and an `itoa` call per byte. Text payloads now cost ~1.0x, binary 1.33x.
#[derive(Debug, Clone)]
pub struct CanonicalMessage {
    pub message_id: u128,
    pub payload: Bytes,
    pub metadata: HashMap<String, String>,
}

/// JSON field holding a base64-encoded binary payload, mutually exclusive with
/// `payload`. Mirrors the CloudEvents JSON `data` / `data_base64` split.
pub const PAYLOAD_BASE64_KEY: &str = "payload_base64";

const FIELDS_COMPACT: &[&str] = &["message_id", "payload", "metadata"];
const FIELDS_HUMAN: &[&str] = &["message_id", "payload", PAYLOAD_BASE64_KEY, "metadata"];

/// Text-based formats (JSON) get a UTF-8 payload as a plain string under `payload`
/// and a binary payload as base64 under `payload_base64`. Binary formats (msgpack)
/// already encode bytes compactly, so they keep the native representation.
impl Serialize for CanonicalMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let human_readable = serializer.is_human_readable();
        let with_metadata = !self.metadata.is_empty();
        let len = 2 + usize::from(with_metadata);
        let mut state = serializer.serialize_struct("CanonicalMessage", len)?;
        state.serialize_field("message_id", &MessageId(self.message_id))?;

        if human_readable {
            // from_utf8 bails at the first invalid byte, so binary payloads cost O(1) here.
            match std::str::from_utf8(&self.payload) {
                Ok(text) => state.serialize_field("payload", text)?,
                Err(_) => state.serialize_field(
                    PAYLOAD_BASE64_KEY,
                    &crate::support::base64_engine::encode(&self.payload),
                )?,
            }
        } else {
            state.serialize_field("payload", &self.payload)?;
        }

        if with_metadata {
            state.serialize_field("metadata", &self.metadata)?;
        }
        state.end()
    }
}

/// Wrapper so `print_uuidv7` stays the single source of truth for id formatting.
struct MessageId(u128);

impl Serialize for MessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        print_uuidv7(&self.0, serializer)
    }
}

/// Accepts every historical shape: `payload` as a string, as a byte array, or as
/// native bytes, plus the new `payload_base64`.
impl<'de> Deserialize<'de> for CanonicalMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = if deserializer.is_human_readable() {
            FIELDS_HUMAN
        } else {
            FIELDS_COMPACT
        };
        deserializer.deserialize_struct("CanonicalMessage", fields, CanonicalMessageVisitor)
    }
}

struct CanonicalMessageVisitor;

enum Field {
    MessageId,
    Payload,
    PayloadBase64,
    Metadata,
    Ignore,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FieldVisitor;

        impl serde::de::Visitor<'_> for FieldVisitor {
            type Value = Field;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a CanonicalMessage field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Field, E>
            where
                E: serde::de::Error,
            {
                Ok(match value {
                    "message_id" => Field::MessageId,
                    "payload" => Field::Payload,
                    PAYLOAD_BASE64_KEY => Field::PayloadBase64,
                    "metadata" => Field::Metadata,
                    _ => Field::Ignore,
                })
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// `u128` id newtype reusing [`deserialize_u128`].
struct MessageIdRepr(u128);

impl<'de> Deserialize<'de> for MessageIdRepr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_u128(deserializer).map(MessageIdRepr)
    }
}

impl<'de> serde::de::Visitor<'de> for CanonicalMessageVisitor {
    type Value = CanonicalMessage;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("struct CanonicalMessage")
    }

    // Compact/positional encodings (msgpack writes structs as arrays).
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        use serde::de::Error;
        let message_id: MessageIdRepr = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(0, &self))?;
        let payload: Bytes = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(1, &self))?;
        let metadata = seq.next_element()?.unwrap_or_default();
        Ok(CanonicalMessage {
            message_id: message_id.0,
            payload,
            metadata,
        })
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        use serde::de::Error;
        let mut message_id: Option<u128> = None;
        let mut payload: Option<Bytes> = None;
        let mut payload_base64: Option<String> = None;
        let mut metadata: Option<HashMap<String, String>> = None;

        while let Some(key) = map.next_key::<Field>()? {
            match key {
                Field::MessageId => {
                    if message_id.is_some() {
                        return Err(A::Error::duplicate_field("message_id"));
                    }
                    message_id = Some(map.next_value::<MessageIdRepr>()?.0);
                }
                // `Bytes`' own visitor already accepts a string, a byte array and
                // native bytes, which covers all legacy JSON forms.
                Field::Payload => {
                    if payload.is_some() {
                        return Err(A::Error::duplicate_field("payload"));
                    }
                    payload = Some(map.next_value()?);
                }
                Field::PayloadBase64 => {
                    if payload_base64.is_some() {
                        return Err(A::Error::duplicate_field(PAYLOAD_BASE64_KEY));
                    }
                    payload_base64 = Some(map.next_value()?);
                }
                Field::Metadata => {
                    if metadata.is_some() {
                        return Err(A::Error::duplicate_field("metadata"));
                    }
                    metadata = Some(map.next_value()?);
                }
                Field::Ignore => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        let payload = match (payload, payload_base64) {
            (Some(_), Some(_)) => {
                return Err(A::Error::custom(
                    "payload and payload_base64 are mutually exclusive",
                ));
            }
            (Some(p), None) => p,
            (None, Some(b64)) => Bytes::from(
                crate::support::base64_engine::decode(&b64)
                    .map_err(|err| A::Error::custom(format!("invalid payload_base64: {err}")))?,
            ),
            (None, None) => return Err(A::Error::missing_field("payload")),
        };

        Ok(CanonicalMessage {
            message_id: message_id.ok_or_else(|| A::Error::missing_field("message_id"))?,
            payload,
            metadata: metadata.unwrap_or_default(),
        })
    }
}

/// Reserved prefix for framework-injected **source/provenance** metadata — the
/// per-message position a consumer read from (e.g. `mqb.src.kafka_offset`,
/// `mqb.src.nats_subject`). These keys describe where a message came from on the
/// *current* hop and are deliberately **not** forwarded: every publisher strips
/// keys with this prefix when serializing metadata to the wire/store (via
/// [`CanonicalMessage::strip_source_metadata`] or [`is_source_metadata_key`]), so
/// they do not accumulate across chained endpoints (http → nats → kafka → mongodb).
/// **Any new metadata-serializing publisher must do the same.**
/// Application metadata (user headers, `reply_to`, `correlation_id`, …) is not
/// prefixed and propagates as before.
pub const SOURCE_METADATA_PREFIX: &str = "mqb.src.";

/// Whether `key` is framework-injected source metadata that must not be forwarded.
/// See [`SOURCE_METADATA_PREFIX`].
#[inline]
pub fn is_source_metadata_key(key: &str) -> bool {
    key.starts_with(SOURCE_METADATA_PREFIX)
}

/// Metadata key holding a message's business identity, set by the `id` middleware.
///
/// Deliberately outside [`SOURCE_METADATA_PREFIX`]: an identity describes the *record*, not the
/// hop it was read on, so it propagates downstream instead of being stripped. Unlike
/// `message_id` (a `u128`) it keeps the key in its original string form, so a sink can use it
/// verbatim.
pub const MESSAGE_IDENTITY_KEY: &str = "mqb.id";

/// Whether the deprecated `MQB_SOURCE_METADATA` compatibility fallback is enabled.
///
/// Off by default — the per-message origin (topic/subject/queue, offset, …) is only
/// needed when consuming a wildcard/pattern subscription and you must recover where
/// each message actually came from (e.g. dead-letter routing). New configurations should use
/// `source_metadata: true` on the relevant source configuration. The environment variable remains
/// for one compatibility release; its value is read once and cached. Stripping/anti-spoofing of
/// `mqb.src.*` stays active regardless, so these keys never propagate downstream.
/// See [`SOURCE_METADATA_PREFIX`].
pub fn source_metadata_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = TEST_FORCE_SOURCE_METADATA.with(|c| c.get()) {
            return forced;
        }
    }
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MQB_SOURCE_METADATA")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

/// Resolve endpoint-local provenance with the legacy environment fallback once at consumer
/// construction time, never in a message-processing loop.
#[inline]
pub fn source_metadata_enabled_for_endpoint(explicit: bool) -> bool {
    explicit || source_metadata_enabled()
}

#[cfg(test)]
thread_local! {
    /// Per-thread override for [`source_metadata_enabled`] in unit tests, so tests can
    /// exercise the enabled/disabled paths deterministically without touching the
    /// process-global env var. `None` falls back to the env-derived default.
    static TEST_FORCE_SOURCE_METADATA: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Guard that restores the test-only per-thread source metadata override.
// Only referenced by endpoint test modules (nats/mqtt), so it looks unused under
// feature sets that exclude them.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct SourceMetadataTestOverride {
    previous: Option<bool>,
}

#[cfg(test)]
impl Drop for SourceMetadataTestOverride {
    fn drop(&mut self) {
        TEST_FORCE_SOURCE_METADATA.with(|c| c.set(self.previous));
    }
}

/// Force [`source_metadata_enabled`] to a value on the current thread (test-only).
#[cfg(test)]
#[must_use]
#[allow(dead_code)]
pub(crate) fn force_source_metadata_for_test(value: Option<bool>) -> SourceMetadataTestOverride {
    let previous = TEST_FORCE_SOURCE_METADATA.with(|c| {
        let prev = c.get();
        c.set(value);
        prev
    });
    SourceMetadataTestOverride { previous }
}

pub fn print_uuidv7<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(fast_uuid_v7::format_uuid(*value).as_ref())
}

/// Custom deserializer for u128 that handles UUID strings, hex, and numeric formats.
pub fn deserialize_u128<'de, D>(deserializer: D) -> Result<u128, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    u128_from_json(&val).map_err(serde::de::Error::custom)
}

pub(crate) fn u128_from_json(val: &serde_json::Value) -> Result<u128, String> {
    if let Some(s) = val.as_str() {
        if let Ok(uuid) = Uuid::parse_str(s) {
            return Ok(uuid.as_u128());
        } else if s.starts_with("0x") || s.starts_with("0X") {
            if let Ok(n) =
                u128::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
            {
                return Ok(n);
            }
        } else if let Ok(n) = s.parse::<u128>() {
            return Ok(n);
        }
    } else if let Some(n) = val.as_u64() {
        return Ok(n as u128);
    } else if let Some(n) = val.as_i64() {
        if n < 0 {
            return Err("message_id cannot be negative".to_string());
        }
        return Ok(n as u128);
    } else if val.is_number() {
        // Fallback for large numeric literals that don't fit in u64/i64
        if let Ok(n) = serde_json::from_value::<u128>(val.clone()) {
            return Ok(n);
        }
    } else if let Some(oid) = val.get("$oid").and_then(|v| v.as_str()) {
        if let Ok(n) = u128::from_str_radix(oid, 16) {
            return Ok(n);
        }
    }
    if let Some(s) = val.as_str() {
        // Any other string id (`"j1"`, an application key, a foreign system's id) is
        // folded into a stable u128 instead of failing. Rejecting it used to make a
        // whole JSON line unparseable, and a `file`/`json` source then silently kept
        // the line as an opaque raw payload, discarding its own `metadata`.
        return Ok(fnv1a_128(s.as_bytes()));
    }
    Err("Invalid u128 format".to_string())
}

/// FNV-1a, 128-bit. Deterministic and stable forever (unlike `DefaultHasher`), so the
/// same string id maps to the same message id in every process and every release —
/// which is what deduplication and correlation rely on.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for b in bytes {
        hash ^= *b as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Parse a message id from a string, accepting the same formats as the JSON
/// deserializer: a UUID string, a `0x`-prefixed hex literal, or a decimal
/// integer. Any other string is hashed into a stable id. Used by the language
/// bindings so id parsing stays identical across Rust, Python, and Node.
pub fn message_id_from_str(id: &str) -> Result<u128, String> {
    u128_from_json(&serde_json::Value::String(id.to_string()))
        .map_err(|err| format!("invalid message id '{id}': {err}"))
}

/// Format a u128 message id as a canonical UUID string (the inverse of
/// [`message_id_from_str`] for UUID-shaped ids).
pub fn format_message_id(id: u128) -> String {
    fast_uuid_v7::format_uuid(id).to_string()
}

impl CanonicalMessage {
    pub fn new(payload: Vec<u8>, message_id: Option<u128>) -> Self {
        Self {
            message_id: message_id.unwrap_or_else(fast_uuid_v7::gen_id_with_sub_ms_4),
            payload: Bytes::from(payload),
            metadata: HashMap::new(),
        }
    }

    pub fn new_bytes(payload: Bytes, message_id: Option<u128>) -> Self {
        Self {
            message_id: message_id.unwrap_or_else(fast_uuid_v7::gen_id_with_sub_ms_4),
            payload,
            metadata: HashMap::new(),
        }
    }

    pub fn from_type<T: Serialize>(data: &T) -> Result<Self, serde_json::Error> {
        let bytes = serde_json::to_vec(data)?;
        Ok(Self::new(bytes, None))
    }

    pub fn from_vec(payload: impl Into<Vec<u8>>) -> Self {
        Self::new(payload.into(), None)
    }

    pub fn set_id(&mut self, id: u128) {
        self.message_id = id;
    }

    /// Remove framework-injected source/provenance metadata (`mqb.src.*`) in place.
    /// Call before serializing an outbound message to the wire/store so per-hop
    /// cursor keys don't accumulate across endpoints. See [`SOURCE_METADATA_PREFIX`].
    #[inline]
    pub fn strip_source_metadata(&mut self) {
        self.metadata.retain(|key, _| !is_source_metadata_key(key));
    }

    pub fn from_json(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct IdExtractor {
            #[serde(deserialize_with = "deserialize_u128")]
            id: u128,
        }

        let mut message_id = None;
        for key in ["message_id", "id", "_id"] {
            if let Some(v) = payload.get(key) {
                // Use from_value with a helper struct to leverage deserialize_u128
                // and produce a proper serde_json::Error on failure.
                let mut map = serde_json::Map::new();
                map.insert("id".to_string(), v.clone());
                let extractor: IdExtractor =
                    serde_json::from_value(serde_json::Value::Object(map))?;
                message_id = Some(extractor.id);
                break;
            }
        }

        let bytes = serde_json::to_vec(&payload)?;
        Ok(Self::new(bytes, message_id))
    }

    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// Returns the payload as a UTF-8 lossy string.
    pub fn get_payload_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.payload)
    }

    /// Sets the payload of this message to the given string.
    pub fn set_payload_str(&mut self, payload: impl Into<String>) {
        self.payload = Bytes::from(payload.into());
    }

    pub fn with_metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_metadata_kv(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn with_type_key(mut self, kind: impl Into<String>) -> Self {
        self.metadata.insert(KIND_KEY.into(), kind.into());
        self
    }

    pub fn with_raw_format(mut self) -> Self {
        self.metadata
            .insert("mq_bridge.original_format".to_string(), "raw".to_string());
        self
    }
}

impl From<&str> for CanonicalMessage {
    fn from(s: &str) -> Self {
        Self::new(s.as_bytes().into(), None)
    }
}

impl From<String> for CanonicalMessage {
    fn from(s: String) -> Self {
        Self::new(s.into_bytes(), None)
    }
}

impl From<Vec<u8>> for CanonicalMessage {
    fn from(v: Vec<u8>) -> Self {
        Self::new(v, None)
    }
}

impl From<serde_json::Value> for CanonicalMessage {
    fn from(v: serde_json::Value) -> Self {
        Self::from_json(v).expect("Failed to serialize JSON value")
    }
}

/// A context object that holds metadata and identification for a message,
/// separated from the payload. Useful for typed handlers.
#[derive(Debug, Clone)]
pub struct MessageContext {
    pub message_id: u128,
    pub metadata: HashMap<String, String>,
}

impl From<CanonicalMessage> for MessageContext {
    fn from(msg: CanonicalMessage) -> Self {
        Self {
            message_id: msg.message_id,
            metadata: msg.metadata,
        }
    }
}

#[doc(hidden)]
pub mod tracing_support {
    use super::CanonicalMessage;

    /// A helper struct to lazily format a slice of message IDs for tracing.
    /// The collection and formatting only occurs if the trace is enabled.
    pub struct LazyMessageIds<'a>(pub &'a [CanonicalMessage]);

    impl<'a> std::fmt::Debug for LazyMessageIds<'a> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let ids: Vec<String> = self
                .0
                .iter()
                .map(|m| format!("{:032x}", m.message_id))
                .collect();
            f.debug_list().entries(ids).finish()
        }
    }
}

#[doc(hidden)]
pub mod macro_support {
    use super::CanonicalMessage;
    use serde::Serialize;

    pub trait Fallback {
        fn convert(&self) -> CanonicalMessage;
    }

    impl<T: Serialize> Fallback for Wrap<T> {
        fn convert(&self) -> CanonicalMessage {
            CanonicalMessage::from_type(&self.0).expect("Serialization failed in msg! macro")
        }
    }

    pub struct Wrap<T>(pub T);

    impl<T> Wrap<T>
    where
        T: Into<CanonicalMessage> + Clone,
    {
        pub fn convert(&self) -> CanonicalMessage {
            self.0.clone().into()
        }
    }
}

/// A macro to create a `CanonicalMessage` easily.
///
/// Examples:
/// ```rust
/// use mq_bridge::msg;
///
/// let m1 = msg!("hello");
/// let m2 = msg!("hello", "greeting");
/// let m3 = msg!("hello", "kind" => "greeting");
///
/// #[derive(serde::Serialize, Clone)]
/// struct MyData { val: i32 }
/// let m4 = msg!(MyData { val: 42 }, "my_type");
/// ```
#[macro_export]
macro_rules! msg {
    ($payload:expr $(, $key:expr => $val:expr)* $(,)?) => {
        {
            #[allow(unused_imports)]
            use $crate::canonical_message::macro_support::{Wrap, Fallback};
            #[allow(unused_mut)]
            let mut message = Wrap($payload).convert();
            $(
                message = message.with_metadata_kv($key, $val);
            )*
            message
        }
    };
    ($payload:expr, $kind:expr $(,)?) => {
        {
            #[allow(unused_imports)]
            use $crate::canonical_message::macro_support::{Wrap, Fallback};
            let mut message = Wrap($payload).convert();
            message = message.with_type_key($kind);
            message
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An ordinary string id must not make the whole envelope unparseable — a
    /// `file`/`json` source used to keep such a line as an opaque raw payload and throw
    /// away its `metadata`.
    #[test]
    fn arbitrary_string_message_id_is_hashed_not_rejected() {
        let line = r#"{"message_id":"j1","payload":"hello","metadata":{"k":"v"}}"#;
        let msg: CanonicalMessage = serde_json::from_str(line).unwrap();
        assert_eq!(msg.payload, Bytes::from_static(b"hello"));
        assert_eq!(msg.metadata.get("k").unwrap(), "v");

        // Stable: the same string always maps to the same id, and different ones differ.
        let again: CanonicalMessage = serde_json::from_str(line).unwrap();
        assert_eq!(msg.message_id, again.message_id);
        assert_ne!(msg.message_id, message_id_from_str("j2").unwrap());

        // The documented forms keep their exact value.
        assert_eq!(message_id_from_str("42").unwrap(), 42);
        assert_eq!(
            message_id_from_str("019fd574-0000-7000-8000-000000000001").unwrap(),
            Uuid::parse_str("019fd574-0000-7000-8000-000000000001")
                .unwrap()
                .as_u128()
        );
    }

    /// A UTF-8 payload becomes a plain JSON string, not a byte array.
    #[test]
    fn json_utf8_payload_is_a_string() {
        let msg = CanonicalMessage::new(b"{\"a\":1}".to_vec(), Some(42));
        let json = String::from_utf8(serde_json::to_vec(&msg).unwrap()).unwrap();
        assert!(json.contains(r#""payload":"{\"a\":1}""#), "{json}");
        assert!(!json.contains('['), "{json}");
        assert!(!json.contains(PAYLOAD_BASE64_KEY), "{json}");

        let back: CanonicalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload, msg.payload);
        assert_eq!(back.message_id, 42);
    }

    #[test]
    fn json_binary_payload_is_base64() {
        let msg = CanonicalMessage::new(vec![0xFF, 0x00, 0xFE], Some(7));
        let json = String::from_utf8(serde_json::to_vec(&msg).unwrap()).unwrap();
        assert!(json.contains(r#""payload_base64":"/wD+""#), "{json}");
        assert!(!json.contains(r#""payload""#), "{json}");

        let back: CanonicalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload.as_ref(), &[0xFF, 0x00, 0xFE]);
        assert_eq!(back.message_id, 7);
    }

    /// Metadata still round-trips and is still omitted when empty.
    #[test]
    fn json_metadata_round_trip() {
        let mut msg = CanonicalMessage::new(b"hi".to_vec(), Some(1));
        assert!(!String::from_utf8(serde_json::to_vec(&msg).unwrap())
            .unwrap()
            .contains("metadata"));

        msg.metadata.insert("kind".into(), "Order".into());
        let back: CanonicalMessage =
            serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap();
        assert_eq!(back.metadata.get("kind").map(String::as_str), Some("Order"));
    }

    /// The legacy verbose byte-array form is still read.
    #[test]
    fn json_reads_legacy_byte_array() {
        let msg: CanonicalMessage =
            serde_json::from_str(r#"{"message_id":"1","payload":[104,105]}"#).unwrap();
        assert_eq!(msg.payload.as_ref(), b"hi");
        assert_eq!(msg.message_id, 1);
    }

    /// A string payload already deserialized to its raw UTF-8 bytes before this
    /// change; that must not shift to base64 decoding.
    #[test]
    fn json_reads_string_payload_verbatim() {
        let msg: CanonicalMessage =
            serde_json::from_str(r#"{"message_id":"1","payload":"hi"}"#).unwrap();
        assert_eq!(msg.payload.as_ref(), b"hi");
    }

    #[test]
    fn json_rejects_both_payload_fields() {
        let err = serde_json::from_str::<CanonicalMessage>(
            r#"{"message_id":"1","payload":"hi","payload_base64":"aGk="}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn json_rejects_invalid_base64() {
        let err = serde_json::from_str::<CanonicalMessage>(
            r#"{"message_id":"1","payload_base64":"!!!!"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid payload_base64"), "{err}");
    }

    /// The msgpack IPC transport must keep native bytes — no base64, no array.
    #[test]
    fn msgpack_keeps_native_bytes() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let msg = CanonicalMessage::new(payload.clone(), Some(9));
        let encoded = rmp_serde::to_vec(&msg).unwrap();
        assert!(
            encoded.len() < payload.len() + 64,
            "msgpack payload was not native bytes: {} bytes",
            encoded.len()
        );

        let back: CanonicalMessage = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(back.payload.as_ref(), payload.as_slice());
        assert_eq!(back.message_id, 9);
    }

    #[test]
    fn source_metadata_key_detection() {
        assert!(is_source_metadata_key("mqb.src.kafka_offset"));
        assert!(is_source_metadata_key("mqb.src.nats_subject"));
        assert!(!is_source_metadata_key("kind"));
        assert!(!is_source_metadata_key("reply_to"));
        assert!(!is_source_metadata_key("correlation_id"));
        // The reserved prefix itself is the boundary.
        assert_eq!(SOURCE_METADATA_PREFIX, "mqb.src.");
    }

    #[test]
    fn explicit_source_metadata_does_not_depend_on_the_legacy_environment() {
        let _legacy_disabled = force_source_metadata_for_test(Some(false));
        assert!(source_metadata_enabled_for_endpoint(true));
        assert!(!source_metadata_enabled_for_endpoint(false));
    }

    #[test]
    fn test_message_id_parsing() {
        // String UUID
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let msg = CanonicalMessage::from_json(json!({ "id": uuid })).unwrap();
        assert_eq!(msg.message_id, 113059749145936325402354257176981405696);

        // Hex string
        let msg = CanonicalMessage::from_json(json!({ "id": "0xFF" })).unwrap();
        assert_eq!(msg.message_id, 255);

        // Numeric
        let msg = CanonicalMessage::from_json(json!({ "id": 100 })).unwrap();
        assert_eq!(msg.message_id, 100);

        // Negative numeric
        let msg_err = CanonicalMessage::from_json(json!({ "id": -1 }));
        assert!(msg_err.is_err());

        // Mongo OID
        let oid = "507f1f77bcf86cd799439011";
        let msg = CanonicalMessage::from_json(json!({ "_id": { "$oid": oid } })).unwrap();
        let expected = u128::from_str_radix(oid, 16).unwrap();
        assert_eq!(msg.message_id, expected);
    }

    #[test]
    fn test_message_id_from_str_helper() {
        // The string helper the bindings call accepts the same formats as the
        // JSON path: UUID, 0x-hex, and decimal.
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(
            message_id_from_str(uuid).unwrap(),
            113059749145936325402354257176981405696
        );
        assert_eq!(message_id_from_str("0xFF").unwrap(), 255);
        assert_eq!(message_id_from_str("100").unwrap(), 100);
        // Anything else is hashed rather than rejected, and stably so.
        assert_eq!(
            message_id_from_str("not-an-id").unwrap(),
            message_id_from_str("not-an-id").unwrap()
        );
        assert_ne!(
            message_id_from_str("not-an-id").unwrap(),
            message_id_from_str("also-not-an-id").unwrap()
        );

        // A UUID id round-trips through format_message_id unchanged.
        let id = message_id_from_str(uuid).unwrap();
        assert_eq!(format_message_id(id), uuid);
    }

    #[test]
    fn test_metadata_builder() {
        let msg = CanonicalMessage::new(b"payload".to_vec(), None)
            .with_metadata_kv("key1", "val1")
            .with_type_key("my_type");

        assert_eq!(msg.metadata.get("key1").map(|s| s.as_str()), Some("val1"));
        assert_eq!(
            msg.metadata.get("kind").map(|s| s.as_str()),
            Some("my_type")
        );
    }
}
