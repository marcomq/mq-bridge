//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The plugin half of the 1.2 host services: a `tracing` subscriber and, with
//! the `metrics` feature, a `metrics` recorder, both forwarding to the host.
//!
//! Installed as the plugin library's globals. A plugin that already installed
//! its own keeps it; the host's is then simply not used.

use std::fmt::Write as _;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing::span;

use crate::support::plugin_abi::{
    MqbHostVTable, MqbSlice, MQB_HOST_VTABLE_SIZE_V1_2, MQB_LOG_DEBUG, MQB_LOG_ERROR, MQB_LOG_INFO,
    MQB_LOG_TRACE, MQB_LOG_WARN,
};

#[derive(Clone, Copy)]
struct Host(&'static MqbHostVTable);

// The host table is immutable, process-lifetime data whose functions may be
// called from any thread.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

static HOST: OnceLock<Host> = OnceLock::new();

/// `plugin_init`: remembers the host and installs the forwarding globals.
pub(super) unsafe extern "C" fn plugin_init(host: *const MqbHostVTable) {
    let _ = std::panic::catch_unwind(|| {
        if host.is_null() || unsafe { (*host).struct_size } < MQB_HOST_VTABLE_SIZE_V1_2 {
            return;
        }
        let host = Host(unsafe { &*host });
        if HOST.set(host).is_err() {
            return;
        }
        let _ = tracing::subscriber::set_global_default(Forwarder(host));
        #[cfg(feature = "metrics")]
        let _ = metrics::set_global_recorder(MetricsForwarder(host));
    });
}

fn level_code(level: tracing::Level) -> u8 {
    match level {
        tracing::Level::ERROR => MQB_LOG_ERROR,
        tracing::Level::WARN => MQB_LOG_WARN,
        tracing::Level::INFO => MQB_LOG_INFO,
        tracing::Level::DEBUG => MQB_LOG_DEBUG,
        _ => MQB_LOG_TRACE,
    }
}

/// Forwards events; spans are not forwarded, so they are never enabled.
struct Forwarder(Host);

impl tracing::Subscriber for Forwarder {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // The host's filter can change at runtime, so it is asked per event.
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_event()
            && unsafe { (self.0 .0.log_enabled)(level_code(*metadata.level())) } != 0
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut text = Rendered::default();
        event.record(&mut text);
        if !text.fields.is_empty() {
            text.message.push_str(&text.fields);
        }
        let metadata = event.metadata();
        unsafe {
            (self.0 .0.log)(
                level_code(*metadata.level()),
                MqbSlice::from_str(metadata.target()),
                MqbSlice::from_str(&text.message),
            )
        };
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

/// The message, then every other field as ` name=value`.
#[derive(Default)]
struct Rendered {
    message: String,
    fields: String,
}

impl Visit for Rendered {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

#[cfg(feature = "metrics")]
use metrics_forwarding::MetricsForwarder;

#[cfg(feature = "metrics")]
mod metrics_forwarding {
    use std::sync::Arc;

    use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, SharedString, Unit};

    use super::Host;
    use crate::support::plugin_abi::{
        MqbKeyValue, MqbSlice, MQB_METRIC_COUNTER, MQB_METRIC_COUNTER_ABSOLUTE,
        MQB_METRIC_GAUGE_ADD, MQB_METRIC_GAUGE_SET, MQB_METRIC_HISTOGRAM,
    };

    pub(super) struct MetricsForwarder(pub(super) Host);

    /// One registered metric; every sample crosses to the host.
    struct Handle {
        host: Host,
        name: String,
        labels: Vec<(String, String)>,
    }

    impl Handle {
        fn new(host: Host, key: &Key) -> Arc<Self> {
            Arc::new(Self {
                host,
                name: key.name().to_owned(),
                labels: key
                    .labels()
                    .map(|label| (label.key().to_owned(), label.value().to_owned()))
                    .collect(),
            })
        }

        fn send(&self, kind: u8, value: f64) {
            let labels: Vec<MqbKeyValue> = self
                .labels
                .iter()
                .map(|(key, value)| MqbKeyValue {
                    key: MqbSlice::from_str(key),
                    value: MqbSlice::from_str(value),
                })
                .collect();
            unsafe {
                (self.host.0.metric)(
                    kind,
                    MqbSlice::from_str(&self.name),
                    labels.as_ptr(),
                    labels.len(),
                    value,
                )
            };
        }
    }

    impl metrics::CounterFn for Handle {
        fn increment(&self, value: u64) {
            self.send(MQB_METRIC_COUNTER, value as f64);
        }

        fn absolute(&self, value: u64) {
            self.send(MQB_METRIC_COUNTER_ABSOLUTE, value as f64);
        }
    }

    impl metrics::GaugeFn for Handle {
        fn increment(&self, value: f64) {
            self.send(MQB_METRIC_GAUGE_ADD, value);
        }

        fn decrement(&self, value: f64) {
            self.send(MQB_METRIC_GAUGE_ADD, -value);
        }

        fn set(&self, value: f64) {
            self.send(MQB_METRIC_GAUGE_SET, value);
        }
    }

    impl metrics::HistogramFn for Handle {
        fn record(&self, value: f64) {
            self.send(MQB_METRIC_HISTOGRAM, value);
        }
    }

    impl metrics::Recorder for MetricsForwarder {
        fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

        fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

        fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

        fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
            Counter::from_arc(Handle::new(self.0, key))
        }

        fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
            Gauge::from_arc(Handle::new(self.0, key))
        }

        fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
            Histogram::from_arc(Handle::new(self.0, key))
        }
    }
}
