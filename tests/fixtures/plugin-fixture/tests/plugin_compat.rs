//! Released plugins built against older ABI minors, driven by this host.
//!
//! Skipped unless the library is named in the env, since neither is built here:
//!
//! - `MQB_COMPAT_PULSAR_LIB`: `mq-bridge-pulsar` built against mq-bridge 0.4.12
//!   (ABI 1.0), with a broker on `MQB_COMPAT_PULSAR_URL` (default
//!   `pulsar://localhost:6650`, see mq-bridge-pulsar's `tests/docker-compose.yml`).
//! - `MQB_COMPAT_CONNECT_LIB`: an `mq-bridge-connect` release (ABI 1.1), its Go
//!   sibling next to it. Needs no broker.

use std::time::Duration;

use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::plugin::conformance::{self, ConformanceOptions};
use mq_bridge::plugin::load_endpoint_plugin;
use mq_bridge::CanonicalMessage;
use serde_json::json;

fn library(var: &str) -> Option<String> {
    let path = std::env::var(var).ok().filter(|path| !path.is_empty());
    if path.is_none() {
        eprintln!("skipped: set {var} to run it");
    }
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn a_1_0_plugin_passes_conformance_under_this_host() {
    let Some(path) = library("MQB_COMPAT_PULSAR_LIB") else {
        return;
    };
    let url = std::env::var("MQB_COMPAT_PULSAR_URL")
        .unwrap_or_else(|_| "pulsar://localhost:6650".to_string());
    let info = load_endpoint_plugin(&path).expect("load the 1.0 plugin");
    assert_eq!(
        (info.name.as_str(), info.abi_major, info.abi_minor),
        ("pulsar", 1, 0)
    );

    let topic = format!("compat-{}", std::process::id());
    let mut options = ConformanceOptions::new(
        &topic,
        json!({
            "url": url,
            "topic": format!("persistent://public/default/{topic}"),
            "subscription": format!("compat-{topic}"),
        }),
    );
    options.messages = 4;
    options.receive_timeout = Duration::from_secs(30);
    // The broker delays a negative ack by a minute.
    options.expect_redelivery = false;
    let factory = get_endpoint_factory("pulsar").expect("registered by the load");
    conformance::run(factory.as_ref(), options)
        .await
        .expect("a 1.0 plugin behaves as it did under a 1.0 host");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_1_1_plugin_round_trips_under_this_host() {
    let Some(path) = library("MQB_COMPAT_CONNECT_LIB") else {
        return;
    };
    let info = load_endpoint_plugin(&path).expect("load the 1.1 plugin");
    assert_eq!(
        (info.name.as_str(), info.abi_major, info.abi_minor),
        ("connect", 1, 1)
    );
    let factory = get_endpoint_factory("connect").expect("registered by the load");

    let source = json!({
        "connector": "generate",
        "count": 10,
        "interval": "",
        "mapping": "root.id = counter()",
    });
    let mut consumer = factory.create_consumer("compat", &source).await.unwrap();
    let mut received = Vec::new();
    while received.len() < 10 {
        let batch = tokio::time::timeout(Duration::from_secs(30), consumer.receive_batch(16))
            .await
            .expect("generate produces within the timeout")
            .expect("receive");
        let acks = vec![Default::default(); batch.messages.len()];
        received.extend(batch.messages);
        (batch.commit)(acks).await.expect("commit");
    }
    assert!(consumer.status().await.healthy);

    let sink = json!({ "connector": "drop" });
    let publisher = factory.create_publisher("compat", &sink).await.unwrap();
    let batch: Vec<CanonicalMessage> = received.into_iter().take(4).collect();
    publisher.send_batch(batch).await.expect("send");
    publisher.flush().await.expect("flush");
}
