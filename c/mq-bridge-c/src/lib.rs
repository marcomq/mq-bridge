//! The C API of mq-bridge. `include/mq_bridge.h` is generated from this crate.
//!
//! Every call blocks the calling thread. A fallible call returns an `MqbStatus`
//! (or a null handle) and leaves its text in [`mqb_last_error`].

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

mod consumer;
mod message;
mod publisher;
mod route;

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use anyhow::{anyhow, Context};
use mq_bridge_bindings_common as common;
use mqb::errors::{ConsumerError, ProcessingError};
use mqb::support::plugin_abi::{
    MqbPluginVTable, MqbStatus, MQB_ERR_CONNECTION, MQB_ERR_PANIC, MQB_ERR_PERMANENT,
    MQB_ERR_RETRYABLE, MQB_OK,
};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Bumped when a change to `mq_bridge.h` can break a program built against the
/// old one; compare with `mqb_api_version()`. `tests/c_header.rs` asks on each change.
pub const MQB_API_VERSION: u32 = 1;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

pub(crate) fn set_error(text: impl std::fmt::Display) {
    let text = text.to_string().replace('\0', " ");
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::new(text).unwrap_or_default());
}

/// `MQB_ERR_RETRYABLE` / `MQB_ERR_CONNECTION` when the engine classified the failure so.
fn status_of(err: &anyhow::Error) -> MqbStatus {
    err.chain()
        .find_map(|cause| {
            if let Some(err) = cause.downcast_ref::<ProcessingError>() {
                return Some(match err {
                    ProcessingError::Retryable(_) => MQB_ERR_RETRYABLE,
                    ProcessingError::Connection(_) => MQB_ERR_CONNECTION,
                    ProcessingError::NonRetryable(_) => MQB_ERR_PERMANENT,
                });
            }
            match cause.downcast_ref::<ConsumerError>()? {
                ConsumerError::Connection(_) => Some(MQB_ERR_CONNECTION),
                _ => Some(MQB_ERR_PERMANENT),
            }
        })
        .unwrap_or(MQB_ERR_PERMANENT)
}

/// Runs `call`, turning an error or a panic into a status plus [`mqb_last_error`].
pub(crate) fn guard(call: impl FnOnce() -> anyhow::Result<()>) -> MqbStatus {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(Ok(())) => MQB_OK,
        Ok(Err(err)) => {
            set_error(format!("{err:#}"));
            status_of(&err)
        }
        Err(_) => {
            set_error("mq-bridge panicked");
            MQB_ERR_PANIC
        }
    }
}

/// Like [`guard`] for constructors: a boxed handle, or null on failure.
pub(crate) fn guard_new<T>(call: impl FnOnce() -> anyhow::Result<T>) -> *mut T {
    let mut created = std::ptr::null_mut();
    guard(|| {
        created = Box::into_raw(Box::new(call()?));
        Ok(())
    });
    created
}

pub(crate) unsafe fn handle<'a, T>(ptr: *const T) -> anyhow::Result<&'a T> {
    unsafe { ptr.as_ref() }.ok_or_else(|| anyhow!("null handle"))
}

pub(crate) unsafe fn text<'a>(ptr: *const c_char, what: &str) -> anyhow::Result<&'a str> {
    if ptr.is_null() {
        return Err(anyhow!("{what} is null"));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .with_context(|| format!("{what} is not UTF-8"))
}

/// A null or empty name means "the document holds a single unnamed entry".
pub(crate) unsafe fn optional_name<'a>(ptr: *const c_char) -> anyhow::Result<Option<&'a str>> {
    if ptr.is_null() {
        return Ok(None);
    }
    Ok(common::normalize_name(Some(unsafe { text(ptr, "name") }?)))
}

pub(crate) fn parse_config(config: &str) -> anyhow::Result<serde_yaml_ng::Value> {
    serde_yaml_ng::from_str(config).context("failed to parse YAML config")
}

pub(crate) fn into_c_string(text: String) -> *mut c_char {
    CString::new(text.replace('\0', " "))
        .unwrap_or_default()
        .into_raw()
}

/// Version of the library, e.g. `"0.4.20"`. Static; do not free.
#[no_mangle]
pub extern "C" fn mqb_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The `MQB_API_VERSION` this library was built with. A mismatch with the header's
/// value means header and library come from different releases.
#[no_mangle]
pub extern "C" fn mqb_api_version() -> u32 {
    MQB_API_VERSION
}

/// Text of the last failed call on this thread. Valid until the next failing call
/// on the same thread; do not free.
#[no_mangle]
pub extern "C" fn mqb_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Frees a string returned by this library (`mqb_config_schema`, `*_status_json`).
#[no_mangle]
pub unsafe extern "C" fn mqb_string_free(text: *mut c_char) {
    if !text.is_null() {
        drop(unsafe { CString::from_raw(text) });
    }
}

/// Requests a graceful shutdown of every route. Returns true only for the first
/// request; it cannot be undone.
#[no_mangle]
pub extern "C" fn mqb_request_shutdown() -> bool {
    mqb::shutdown::request_shutdown()
}

#[no_mangle]
pub extern "C" fn mqb_is_shutdown_requested() -> bool {
    mqb::shutdown::is_shutdown_requested()
}

/// JSON Schema of the config document, or null if the library was built without
/// the `schema` feature. Free with `mqb_string_free`.
#[no_mangle]
pub extern "C" fn mqb_config_schema() -> *mut c_char {
    #[cfg(feature = "schema")]
    {
        let mut out = std::ptr::null_mut();
        guard(|| {
            let schema = schemars::schema_for!(mqb::models::Config);
            out = into_c_string(serde_json::to_string(&schema)?);
            Ok(())
        });
        out
    }
    #[cfg(not(feature = "schema"))]
    {
        set_error("this mq-bridge build has no `schema` feature");
        std::ptr::null_mut()
    }
}

/// Loads a native plugin library and registers the endpoints and middleware it
/// exports. Call before starting a route that names them.
#[no_mangle]
pub unsafe extern "C" fn mqb_load_plugin(path: *const c_char) -> MqbStatus {
    guard(|| {
        let path = unsafe { text(path, "path") }?;
        mqb::plugin::load_endpoint_plugins(Path::new(path)).map(drop)
    })
}

/// Registers a custom endpoint or middleware implemented in this program. `table`
/// is the one a plugin library would export (see `mq_bridge_plugin.h`) and must
/// stay valid for the life of the process.
#[no_mangle]
pub unsafe extern "C" fn mqb_register_plugin(table: *const MqbPluginVTable) -> MqbStatus {
    guard(|| unsafe { mqb::plugin::register_plugin_table(table) }.map(drop))
}

/// Receives one library log event. `level` is `error`, `warn`, `info`, `debug` or
/// `trace`; the strings are valid for the call only.
pub type mqb_log_fn = Option<
    unsafe extern "C" fn(
        level: *const c_char,
        target: *const c_char,
        message: *const c_char,
        user_data: *mut c_void,
    ),
>;

struct CLogLayer {
    callback: unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *mut c_void),
    user_data: usize,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let record = common::logging::record_from_event(event);
        let c_text = |text: &str| CString::new(text.replace('\0', " ")).unwrap_or_default();
        let level = c_text(record.level_str());
        let target = c_text(&record.target);
        let message = c_text(&record.message);
        unsafe {
            (self.callback)(
                level.as_ptr(),
                target.as_ptr(),
                message.as_ptr(),
                self.user_data as *mut c_void,
            )
        };
    }
}

/// Routes the library's log events into `callback`, which may be called from any
/// thread. `level` (null for `warn`) seeds the filter; `MQ_BRIDGE_LOG` / `RUST_LOG`
/// override it. Fails if logging was already initialized.
#[no_mangle]
pub unsafe extern "C" fn mqb_init_logging(
    callback: mqb_log_fn,
    user_data: *mut c_void,
    level: *const c_char,
) -> MqbStatus {
    guard(|| {
        let callback = callback.ok_or_else(|| anyhow!("callback is null"))?;
        let level = unsafe { optional_name(level) }?;
        tracing_subscriber::registry()
            .with(common::logging::env_filter(level))
            .with(CLogLayer {
                callback,
                user_data: user_data as usize,
            })
            .try_init()
            .map_err(|err| anyhow!("mq-bridge logging is already initialized: {err}"))
    })
}
