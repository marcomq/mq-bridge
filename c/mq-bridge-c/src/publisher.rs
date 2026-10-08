use std::ffi::c_char;
use std::path::Path;

use anyhow::anyhow;
use mq_bridge_bindings_common as common;
use mqb::models::Endpoint;
use mqb::support::plugin_abi::MqbStatus;
use mqb::{Publisher, SentBatch};
use tokio::runtime::Runtime;

use crate::message::mqb_message_t;
use crate::{guard, guard_new, handle, optional_name, parse_config, text};

/// Publishes to one output endpoint. Safe to use from several threads.
pub struct mqb_publisher_t {
    runtime: Runtime,
    publisher: Publisher,
}

fn build(endpoint: Endpoint) -> anyhow::Result<mqb_publisher_t> {
    let runtime = common::build_runtime()?;
    let publisher = runtime.block_on(Publisher::new(endpoint))?;
    Ok(mqb_publisher_t { runtime, publisher })
}

/// Builds a publisher from a YAML or JSON config file. `name` selects an entry of
/// a `publishers:` document; null or `""` for a single bare endpoint. Null on error.
#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_from_file(
    path: *const c_char,
    name: *const c_char,
) -> *mut mqb_publisher_t {
    guard_new(|| {
        let path = unsafe { text(path, "path") }?;
        let name = unsafe { optional_name(name) }?;
        build(common::load_named_publisher(Path::new(path), name)?)
    })
}

/// Like `mqb_publisher_from_file`, from YAML or JSON text.
#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_from_str(
    config: *const c_char,
    name: *const c_char,
) -> *mut mqb_publisher_t {
    guard_new(|| {
        let value = parse_config(unsafe { text(config, "config") }?)?;
        let name = unsafe { optional_name(name) }?;
        build(common::named_publisher_from_value(value, name)?)
    })
}

/// Publishes one message and waits for the endpoint to accept it.
#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_send(
    publisher: *const mqb_publisher_t,
    message: *const mqb_message_t,
) -> MqbStatus {
    guard(|| {
        let publisher = unsafe { handle(publisher) }?;
        let message = unsafe { handle(message) }?.inner.clone();
        publisher
            .runtime
            .block_on(publisher.publisher.send(message))
            .map(drop)
    })
}

/// Publishes `count` messages in one call. Fails if any of them was rejected.
#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_send_batch(
    publisher: *const mqb_publisher_t,
    messages: *const *const mqb_message_t,
    count: usize,
) -> MqbStatus {
    guard(|| {
        let publisher = unsafe { handle(publisher) }?;
        if messages.is_null() && count > 0 {
            return Err(anyhow!("messages is null"));
        }
        let batch = (0..count)
            .map(|index| Ok(unsafe { handle(*messages.add(index)) }?.inner.clone()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        match publisher
            .runtime
            .block_on(publisher.publisher.send_batch(batch))?
        {
            SentBatch::Partial { failed, .. } if !failed.is_empty() => Err(anyhow!(
                "{} of {count} message(s) failed to publish. First error: {}",
                failed.len(),
                failed[0].1
            )),
            _ => Ok(()),
        }
    })
}

/// Sends a request and stores the reply in `*response` (free it with
/// `mqb_message_free`; null on failure). Needs an endpoint that supports request-reply.
#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_request(
    publisher: *const mqb_publisher_t,
    message: *const mqb_message_t,
    response: *mut *mut mqb_message_t,
) -> MqbStatus {
    guard(|| {
        if response.is_null() {
            return Err(anyhow!("null output pointer"));
        }
        unsafe { *response = std::ptr::null_mut() };
        let publisher = unsafe { handle(publisher) }?;
        let message = unsafe { handle(message) }?.inner.clone();
        let reply = publisher
            .runtime
            .block_on(publisher.publisher.request(message))?;
        unsafe { *response = Box::into_raw(Box::new(mqb_message_t::wrap(reply))) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mqb_publisher_free(publisher: *mut mqb_publisher_t) {
    if !publisher.is_null() {
        drop(unsafe { Box::from_raw(publisher) });
    }
}
