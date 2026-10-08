use std::ffi::c_char;
use std::sync::OnceLock;

use anyhow::anyhow;
use mqb::canonical_message::{format_message_id, message_id_from_str};
use mqb::support::plugin_abi::{MqbSlice, MqbStatus};
use mqb::CanonicalMessage;

use crate::{guard, guard_new, handle, text};

/// A message: payload bytes, string metadata and an id.
pub struct mqb_message_t {
    pub(crate) inner: CanonicalMessage,
    id_text: OnceLock<String>,
}

impl mqb_message_t {
    pub(crate) fn wrap(inner: CanonicalMessage) -> Self {
        Self {
            inner,
            id_text: OnceLock::new(),
        }
    }
}

const ABSENT: MqbSlice = MqbSlice {
    ptr: std::ptr::null(),
    len: 0,
};

/// Creates a message with a copy of `payload` and a generated id. Free it with
/// `mqb_message_free`; sending does not consume it.
#[no_mangle]
pub unsafe extern "C" fn mqb_message_new(payload: *const u8, len: usize) -> *mut mqb_message_t {
    guard_new(|| {
        if payload.is_null() && len > 0 {
            return Err(anyhow!("payload is null"));
        }
        let bytes = unsafe { MqbSlice { ptr: payload, len }.as_bytes() };
        Ok(mqb_message_t::wrap(CanonicalMessage::new(
            bytes.to_vec(),
            None,
        )))
    })
}

/// Sets the id: a UUID, a `0x` hex or decimal integer, or any text (hashed).
#[no_mangle]
pub unsafe extern "C" fn mqb_message_set_id(
    message: *mut mqb_message_t,
    id: *const c_char,
) -> MqbStatus {
    guard(|| {
        let message = unsafe { message.as_mut() }.ok_or_else(|| anyhow!("null handle"))?;
        let id = unsafe { text(id, "id") }?;
        message.inner.message_id = message_id_from_str(id).map_err(|err| anyhow!(err))?;
        message.id_text = OnceLock::new();
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mqb_message_set_metadata(
    message: *mut mqb_message_t,
    key: *const c_char,
    value: *const c_char,
) -> MqbStatus {
    guard(|| {
        let message = unsafe { message.as_mut() }.ok_or_else(|| anyhow!("null handle"))?;
        let key = unsafe { text(key, "key") }?;
        let value = unsafe { text(value, "value") }?;
        message
            .inner
            .metadata
            .insert(key.to_string(), value.to_string());
        Ok(())
    })
}

/// The payload. Like every slice a message returns, it is not NUL-terminated and
/// stays valid until the message is changed or freed.
#[no_mangle]
pub unsafe extern "C" fn mqb_message_payload(message: *const mqb_message_t) -> MqbSlice {
    match unsafe { handle(message) } {
        Ok(message) => MqbSlice::from_bytes(&message.inner.payload),
        Err(_) => ABSENT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn mqb_message_id(message: *const mqb_message_t) -> MqbSlice {
    match unsafe { handle(message) } {
        Ok(message) => MqbSlice::from_str(
            message
                .id_text
                .get_or_init(|| format_message_id(message.inner.message_id)),
        ),
        Err(_) => ABSENT,
    }
}

/// The metadata value of `key`; `ptr` is null when there is none.
#[no_mangle]
pub unsafe extern "C" fn mqb_message_metadata(
    message: *const mqb_message_t,
    key: *const c_char,
) -> MqbSlice {
    let found = unsafe { handle(message) }
        .and_then(|message| Ok(message.inner.metadata.get(unsafe { text(key, "key") }?)));
    match found {
        Ok(Some(value)) => MqbSlice::from_str(value),
        _ => ABSENT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn mqb_message_metadata_count(message: *const mqb_message_t) -> usize {
    unsafe { handle(message) }.map_or(0, |message| message.inner.metadata.len())
}

/// The metadata entry at `index` (below `mqb_message_metadata_count`), in no
/// particular order.
#[no_mangle]
pub unsafe extern "C" fn mqb_message_metadata_at(
    message: *const mqb_message_t,
    index: usize,
    key: *mut MqbSlice,
    value: *mut MqbSlice,
) -> MqbStatus {
    guard(|| {
        let message = unsafe { handle(message) }?;
        let (found_key, found_value) = message
            .inner
            .metadata
            .iter()
            .nth(index)
            .ok_or_else(|| anyhow!("metadata index {index} is out of range"))?;
        if key.is_null() || value.is_null() {
            return Err(anyhow!("null output pointer"));
        }
        unsafe {
            *key = MqbSlice::from_str(found_key);
            *value = MqbSlice::from_str(found_value);
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mqb_message_free(message: *mut mqb_message_t) {
    if !message.is_null() {
        drop(unsafe { Box::from_raw(message) });
    }
}
