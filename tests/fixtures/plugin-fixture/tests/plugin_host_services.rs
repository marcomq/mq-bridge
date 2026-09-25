//! A loaded plugin's logs and metrics reach the host's subscriber and recorder.
//!
//! Its own file, because both are process-wide and installed once.

use std::sync::{Arc, Mutex};

use metrics::{
    Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use mq_bridge::plugin::{
    load_endpoint_plugin, test_support::build_plugin_cdylib, PLUGIN_LOG_TARGET,
};
use mq_bridge::CanonicalMessage;
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<(String, String)>>>);

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        let target = event.metadata().target().to_string();
        self.0.lock().unwrap().push((target, fields.0));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[derive(Clone, Default)]
struct Counters(Arc<Mutex<Vec<Increment>>>);

/// Name, labels and amount of one counter increment.
type Increment = (String, Vec<(String, String)>, u64);

struct CounterSlot {
    counters: Counters,
    key: Key,
}

impl CounterFn for CounterSlot {
    fn increment(&self, value: u64) {
        let labels = self
            .key
            .labels()
            .map(|label| (label.key().to_string(), label.value().to_string()))
            .collect();
        let name = self.key.name().to_string();
        self.counters.0.lock().unwrap().push((name, labels, value));
    }
    fn absolute(&self, _: u64) {}
}

impl Recorder for Counters {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(CounterSlot {
            counters: self.clone(),
            key: key.clone(),
        }))
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_logs_and_metrics_reach_the_host() {
    let logs = Logs::default();
    let counters = Counters::default();
    tracing::subscriber::set_global_default(logs.clone()).unwrap();
    metrics::set_global_recorder(counters.clone()).unwrap();

    let library = build_plugin_cdylib(env!("CARGO_MANIFEST_DIR"), "mq-bridge-plugin-fixture")
        .expect("build the fixture plugin");
    let info = load_endpoint_plugin(library).expect("the fixture plugin should load");
    let factory = mq_bridge::extensions::get_endpoint_factory(&info.name).unwrap();
    let publisher = factory
        .create_publisher("test", &json!({ "queue": "host-services" }))
        .await
        .expect("create publisher");
    publisher
        .send_batch(vec![
            CanonicalMessage::from("a"),
            CanonicalMessage::from("b"),
        ])
        .await
        .expect("send batch");

    let logs = logs.0.lock().unwrap().clone();
    assert!(
        logs.iter()
            .any(|(target, fields)| target == PLUGIN_LOG_TARGET
                && fields.contains("fixture opened a publisher")
                && fields.contains("queue=host-services")),
        "{logs:?}"
    );
    let counters = counters.0.lock().unwrap().clone();
    assert!(
        counters.contains(&(
            "fixture_published_total".to_string(),
            vec![("queue".to_string(), "host-services".to_string())],
            2
        )),
        "{counters:?}"
    );
}
