//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CanonicalMessage {
    pub message_id: Option<u64>,
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

    pub fn from_json(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        let bytes = serde_json::to_vec(&payload)?;
        Ok(Self::new(bytes))
    }

    pub fn with_metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}
