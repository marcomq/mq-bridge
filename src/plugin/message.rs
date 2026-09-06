//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Message conversion for both sides of the plugin ABI: the host uses it to
//! publish and to read received batches, the SDK for the mirror image.

use std::collections::HashMap;

use crate::support::plugin_abi::{MqbKeyValue, MqbMessage, MqbSlice};
use crate::CanonicalMessage;

/// Copies borrowed ABI messages into owned ones.
///
/// # Safety
/// `ptr` must point at `len` initialised messages whose slices are still valid,
/// i.e. the call is in progress or the batch has not been released.
pub(crate) unsafe fn from_abi(ptr: *const MqbMessage, len: usize) -> Vec<CanonicalMessage> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(ptr, len) }
        .iter()
        .map(|message| {
            let id = u128::from_be_bytes(message.message_id);
            let mut canonical = CanonicalMessage::new(
                unsafe { message.payload.as_bytes() }.to_vec(),
                (id != 0).then_some(id),
            );
            if !message.metadata.is_null() && message.metadata_len > 0 {
                let entries =
                    unsafe { std::slice::from_raw_parts(message.metadata, message.metadata_len) };
                canonical.metadata = HashMap::with_capacity(entries.len());
                for entry in entries {
                    canonical.metadata.insert(
                        String::from_utf8_lossy(unsafe { entry.key.as_bytes() }).into_owned(),
                        String::from_utf8_lossy(unsafe { entry.value.as_bytes() }).into_owned(),
                    );
                }
            }
            canonical
        })
        .collect()
}

/// Messages in ABI form, together with the storage their slices point into.
///
/// The entries borrow each message's payload `Bytes` and metadata `String`s,
/// which live in their own heap allocations — so moving this struct keeps the
/// pointers valid. Dropping it invalidates them.
pub(crate) struct AbiMessages {
    messages: Vec<CanonicalMessage>,
    /// Metadata for every message; `abi` points into it.
    _metadata: Vec<MqbKeyValue>,
    abi: Vec<MqbMessage>,
}

impl AbiMessages {
    pub(crate) fn new(messages: Vec<CanonicalMessage>) -> Self {
        let total = messages.iter().map(|message| message.metadata.len()).sum();
        // Exact capacity: a reallocation would dangle the pointers taken below.
        let mut metadata: Vec<MqbKeyValue> = Vec::with_capacity(total);
        let mut ranges = Vec::with_capacity(messages.len());
        for message in &messages {
            let start = metadata.len();
            for (key, value) in &message.metadata {
                metadata.push(MqbKeyValue {
                    key: MqbSlice::from_str(key),
                    value: MqbSlice::from_str(value),
                });
            }
            ranges.push((start, metadata.len() - start));
        }
        let base = metadata.as_ptr();
        let abi = messages
            .iter()
            .zip(ranges)
            .map(|(message, (start, len))| MqbMessage {
                message_id: message.message_id.to_be_bytes(),
                payload: MqbSlice::from_bytes(&message.payload),
                // Safety: `start` indexes `metadata`, which is never grown again.
                metadata: if len == 0 {
                    std::ptr::null()
                } else {
                    unsafe { base.add(start) }
                },
                metadata_len: len,
            })
            .collect();
        Self {
            messages,
            _metadata: metadata,
            abi,
        }
    }

    pub(crate) fn as_ptr(&self) -> *const MqbMessage {
        self.abi.as_ptr()
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_the_round_trip() {
        let mut original = CanonicalMessage::from("payload");
        original.metadata.insert("kind".into(), "order".into());
        let owned = AbiMessages::new(vec![original.clone(), CanonicalMessage::from("plain")]);

        let restored = unsafe { from_abi(owned.as_ptr(), owned.len()) };
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].payload, original.payload);
        assert_eq!(restored[0].message_id, original.message_id);
        assert_eq!(
            restored[0].metadata.get("kind").map(String::as_str),
            Some("order")
        );
        assert!(restored[1].metadata.is_empty());
    }

    #[test]
    fn moving_the_storage_keeps_the_slices_valid() {
        let owned = Box::new(AbiMessages::new(vec![CanonicalMessage::from("stable")]));
        let restored = unsafe { from_abi(owned.as_ptr(), owned.len()) };
        assert_eq!(restored[0].get_payload_str(), "stable");
    }

    #[test]
    fn a_message_without_an_id_gets_a_fresh_one() {
        let abi = [MqbMessage {
            message_id: [0u8; 16],
            payload: MqbSlice::from_str("body"),
            metadata: std::ptr::null(),
            metadata_len: 0,
        }];
        assert_ne!(unsafe { from_abi(abi.as_ptr(), 1) }[0].message_id, 0);
    }

    #[test]
    fn an_empty_array_is_read_without_dereferencing() {
        assert!(unsafe { from_abi(std::ptr::null(), 0) }.is_empty());
    }
}
