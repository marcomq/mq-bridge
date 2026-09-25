//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The services a 1.2 plugin receives through `plugin_init`: its logs and
//! metrics are re-emitted here, into the host's subscriber and recorder.
//!
//! Every plugin event uses the target [`PLUGIN_LOG_TARGET`], because a
//! `tracing` target must be known at compile time; the plugin's own module path
//! travels in the `module` field.

use std::borrow::Cow;
use std::panic::AssertUnwindSafe;

use crate::support::plugin_abi::{
    MqbHostVTable, MqbKeyValue, MqbSlice, MQB_LOG_DEBUG, MQB_LOG_ERROR, MQB_LOG_INFO,
    MQB_LOG_TRACE, MQB_LOG_WARN,
};

/// The `tracing` target of every event a plugin logs.
pub const PLUGIN_LOG_TARGET: &str = "mq_bridge::plugin";

pub(super) static HOST_VTABLE: MqbHostVTable = MqbHostVTable {
    struct_size: std::mem::size_of::<MqbHostVTable>(),
    log_enabled,
    log,
    metric,
};

unsafe extern "C" fn log_enabled(level: u8) -> u8 {
    let enabled = match level {
        MQB_LOG_ERROR => tracing::enabled!(target: PLUGIN_LOG_TARGET, tracing::Level::ERROR),
        MQB_LOG_WARN => tracing::enabled!(target: PLUGIN_LOG_TARGET, tracing::Level::WARN),
        MQB_LOG_INFO => tracing::enabled!(target: PLUGIN_LOG_TARGET, tracing::Level::INFO),
        MQB_LOG_DEBUG => tracing::enabled!(target: PLUGIN_LOG_TARGET, tracing::Level::DEBUG),
        MQB_LOG_TRACE => tracing::enabled!(target: PLUGIN_LOG_TARGET, tracing::Level::TRACE),
        _ => false,
    };
    u8::from(enabled)
}

unsafe extern "C" fn log(level: u8, module: MqbSlice, message: MqbSlice) {
    // A host subscriber that panics must not unwind into the plugin.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let module = unsafe { text(module) };
        let message = unsafe { text(message) };
        match level {
            MQB_LOG_ERROR => tracing::error!(target: PLUGIN_LOG_TARGET, %module, "{message}"),
            MQB_LOG_WARN => tracing::warn!(target: PLUGIN_LOG_TARGET, %module, "{message}"),
            MQB_LOG_INFO => tracing::info!(target: PLUGIN_LOG_TARGET, %module, "{message}"),
            MQB_LOG_DEBUG => tracing::debug!(target: PLUGIN_LOG_TARGET, %module, "{message}"),
            _ => tracing::trace!(target: PLUGIN_LOG_TARGET, %module, "{message}"),
        }
    }));
}

unsafe fn text<'a>(slice: MqbSlice) -> Cow<'a, str> {
    String::from_utf8_lossy(unsafe { slice.as_bytes() })
}

#[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
unsafe extern "C" fn metric(
    kind: u8,
    name: MqbSlice,
    labels: *const MqbKeyValue,
    labels_len: usize,
    value: f64,
) {
    #[cfg(feature = "metrics")]
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        record(kind, name, labels, labels_len, value)
    }));
}

#[cfg(feature = "metrics")]
unsafe fn record(kind: u8, name: MqbSlice, labels: *const MqbKeyValue, len: usize, value: f64) {
    use crate::support::plugin_abi::{
        MQB_METRIC_COUNTER, MQB_METRIC_COUNTER_ABSOLUTE, MQB_METRIC_GAUGE_ADD,
        MQB_METRIC_GAUGE_SET, MQB_METRIC_HISTOGRAM,
    };
    let name = unsafe { text(name) }.into_owned();
    let pairs = if labels.is_null() || len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(labels, len) }
    };
    let labels: Vec<metrics::Label> = pairs
        .iter()
        .map(|pair| unsafe {
            metrics::Label::new(text(pair.key).into_owned(), text(pair.value).into_owned())
        })
        .collect();
    match kind {
        MQB_METRIC_COUNTER => metrics::counter!(name, labels).increment(value as u64),
        MQB_METRIC_COUNTER_ABSOLUTE => metrics::counter!(name, labels).absolute(value as u64),
        MQB_METRIC_GAUGE_SET => metrics::gauge!(name, labels).set(value),
        MQB_METRIC_GAUGE_ADD => metrics::gauge!(name, labels).increment(value),
        MQB_METRIC_HISTOGRAM => metrics::histogram!(name, labels).record(value),
        _ => {}
    }
}
