//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::type_handler::KIND_KEY;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CanonicalMessage {
    pub message_id: u128,
    pub payload: Bytes,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl CanonicalMessage {
    pub fn new(payload: Vec<u8>, message_id: Option<u128>) -> Self {
        Self {
            message_id: message_id.unwrap_or_else(fast_uuid_v7::gen_id),
            payload: Bytes::from(payload),
            metadata: HashMap::new(),
        }
    }

    pub fn new_bytes(payload: Bytes, message_id: Option<u128>) -> Self {
        Self {
            message_id: message_id.unwrap_or_else(fast_uuid_v7::gen_id),
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

    pub fn from_json(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        let mut message_id = None;
        if let Some(val) = payload
            .get("message_id")
            .or(payload.get("id"))
            .or(payload.get("_id"))
        {
            if let Some(s) = val.as_str() {
                if let Ok(uuid) = Uuid::parse_str(s) {
                    message_id = Some(uuid.as_u128());
                } else if let Ok(n) = u128::from_str_radix(s.trim_start_matches("0x"), 16) {
                    message_id = Some(n);
                } else if let Ok(n) = s.parse::<u128>() {
                    message_id = Some(n);
                }
            } else if let Some(n) = val.as_i64() {
                message_id = Some(n as u128);
            } else if let Some(n) = val.as_u64() {
                message_id = Some(n as u128);
            } else if let Some(oid) = val.get("$oid").and_then(|v| v.as_str()) {
                if let Ok(n) = u128::from_str_radix(oid, 16) {
                    message_id = Some(n);
                }
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
