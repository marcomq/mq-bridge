//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Message conversion for both sides of the plugin ABI: the host uses it to
//! publish and to read received batches, the SDK for the mirror image.

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
    // SAFETY: the caller guarantees `len` initialised messages at the non-null `ptr`.
    unsafe { std::slice::from_raw_parts(ptr, len) }
        .iter()
        // SAFETY: each message comes from that batch, so its slices are still valid.
        .map(|message| unsafe { read_message(message, true, true) })
        .collect()
}

/// Copies one borrowed message; a part not copied is left empty for the caller to fill.
///
/// # Safety
/// As for [`from_abi`].
pub(crate) unsafe fn read_message(
    message: &MqbMessage,
    copy_payload: bool,
    copy_metadata: bool,
) -> CanonicalMessage {
    let id = u128::from_be_bytes(message.message_id);
    let payload = if copy_payload {
        // SAFETY: the caller keeps the batch alive for this call; `to_vec` copies out before it ends.
        unsafe { message.payload.as_bytes() }.to_vec()
    } else {
        Vec::new()
    };
    let mut canonical = CanonicalMessage::new(payload, (id != 0).then_some(id));
    if copy_metadata && !message.metadata.is_null() && message.metadata_len > 0 {
        // SAFETY: non-null and, per the caller's contract, `metadata_len` initialised entries.
        let entries = unsafe { std::slice::from_raw_parts(message.metadata, message.metadata_len) };
        let metadata = canonical.metadata_map_mut();
        metadata.reserve(entries.len());
        // SAFETY (key / value below): same batch, still alive; both are copied into owned Strings.
        for entry in entries {
            metadata.insert(
                String::from_utf8_lossy(unsafe { entry.key.as_bytes() }).into_owned(),
                String::from_utf8_lossy(unsafe { entry.value.as_bytes() }).into_owned(),
            );
        }
    }
    canonical
}

/// Messages in ABI form, together with the storage their slices point into.
///
/// The entries borrow each message's payload `Bytes` and metadata `String`s,
/// which live in their own heap allocations — so moving this struct keeps the
/// pointers valid. Dropping it invalidates them.
pub(crate) struct AbiMessages {
    messages: Vec<CanonicalMessage>,
    /// Metadata for every message; `abi` points into it.
    metadata: Vec<MqbKeyValue>,
    abi: Vec<MqbMessage>,
}

impl AbiMessages {
    pub(crate) fn new(messages: Vec<CanonicalMessage>) -> Self {
        let mut abi = Self {
            messages: Vec::new(),
            metadata: Vec::new(),
            abi: Vec::new(),
        };
        abi.reload(messages);
        abi
    }

    /// Replaces the messages, reusing the ABI arrays' capacity.
    pub(crate) fn reload(&mut self, messages: Vec<CanonicalMessage>) {
        let total = messages.iter().map(CanonicalMessage::metadata_len).sum();
        self.abi.clear();
        self.metadata.clear();
        // Reserved up front: a reallocation would dangle the pointers taken below.
        self.metadata.reserve(total);
        for message in &messages {
            for (key, value) in message.metadata_iter() {
                self.metadata.push(MqbKeyValue {
                    key: MqbSlice::from_str(key),
                    value: MqbSlice::from_str(value),
                });
            }
        }
        let base = self.metadata.as_ptr();
        let mut start = 0;
        self.abi.extend(messages.iter().map(|message| {
            let len = message.metadata_len();
            // Safety: `start` indexes `metadata`, which is not grown again.
            let entries = if len == 0 {
                std::ptr::null()
            } else {
                unsafe { base.add(start) }
            };
            start += len;
            MqbMessage {
                message_id: message.message_id.to_be_bytes(),
                payload: MqbSlice::from_bytes(&message.payload),
                metadata: entries,
                metadata_len: len,
            }
        }));
        self.messages = messages;
    }

    /// The ABI entries beside the messages they point into.
    pub(crate) fn split(&mut self) -> (&[MqbMessage], &mut [CanonicalMessage]) {
        (&self.abi, &mut self.messages)
    }

    /// Takes the messages out, leaving the arrays empty for the next [`Self::reload`].
    pub(crate) fn take_messages(&mut self) -> Vec<CanonicalMessage> {
        self.abi.clear();
        self.metadata.clear();
        std::mem::take(&mut self.messages)
    }

    pub(crate) fn as_ptr(&self) -> *const MqbMessage {
        self.abi.as_ptr()
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn abi(&self) -> &[MqbMessage] {
        &self.abi
    }

    /// Recovers the messages, consuming the ABI view of them. Lets a publish
    /// outcome be paired back with the message it belongs to.
    pub(crate) fn into_messages(self) -> Vec<CanonicalMessage> {
        self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_the_round_trip() {
        let mut original = CanonicalMessage::from("payload");
        original.set_metadata("kind", "order");
        let owned = AbiMessages::new(vec![original.clone(), CanonicalMessage::from("plain")]);

        let restored = unsafe { from_abi(owned.as_ptr(), owned.len()) };
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].payload, original.payload);
        assert_eq!(restored[0].message_id, original.message_id);
        assert_eq!(restored[0].get_metadata("kind"), Some("order"));
        assert_eq!(restored[1].metadata_len(), 0);
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
