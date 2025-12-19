//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CanonicalMessage {
    pub message_id: Option<u128>,
    pub payload: Bytes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

impl CanonicalMessage {
    pub fn new(payload: Vec<u8>) -> Self {
        Self {
            message_id: None,
            payload: Bytes::from(payload),
            metadata: None,
        }
    }

    pub fn set_id(&mut self, id: u128) {
        self.message_id = Some(id);
    }

    pub fn gen_id(&mut self) {
        self.set_id(Uuid::new_v4().as_u128());
    }

    pub fn with_gen_id(mut self) -> Self {
        self.gen_id();
        self
    }

    pub fn from_json(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        let mut message_id = None;
        if let Some(val) = payload.get("message_id").or(payload.get("id")).or(payload.get("_id")) {
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
        let mut msg = Self::new(bytes);
        msg.message_id = message_id;
        Ok(msg)
    }

    pub fn from_struct<T: Serialize>(data: &T) -> Result<Self, serde_json::Error> {
        let bytes = serde_json::to_vec(data)?;
        Ok(Self::new(bytes))
    }

    pub fn get_struct<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    pub fn with_metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}
