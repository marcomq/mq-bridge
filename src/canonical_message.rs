//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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
            message_id: message_id.unwrap_or_else(|| Uuid::now_v7().as_u128()),
            payload: Bytes::from(payload),
            metadata: HashMap::new(),
        }
    }

    pub fn from_vec(payload: impl Into<Vec<u8>>) -> Self {
        Self::new(payload.into(), None)
    }

    pub fn from_str(payload: &str) -> Self {
        Self::new(payload.as_bytes().into(), None)
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

    pub fn from_struct<T: Serialize>(data: &T) -> Result<Self, serde_json::Error> {
        let bytes = serde_json::to_vec(data)?;
        Ok(Self::new(bytes, None))
    }

    pub fn get_struct<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
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
}

impl From<&str> for CanonicalMessage {
    fn from(s: &str) -> Self {
        Self::from_str(s)
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
