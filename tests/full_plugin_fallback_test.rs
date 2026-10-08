//! A host without the `sled` feature serves `sled:` from the full plugin library.
//!
//! Ignored by default: it needs the plugin built first, and the host built without `sled`.
//!
//! ```text
//! cargo build -p mq-bridge-full-plugin --no-default-features --features mq-bridge/sled
//! MQB_FULL_PLUGIN=target/debug/libmq_bridge_full.so \
//!   cargo test --features plugin --test full_plugin_fallback_test -- --ignored
//! ```
#![cfg(all(feature = "plugin", not(feature = "sled")))]

use std::time::Duration;

use mq_bridge::endpoints::{create_consumer_from_route, create_publisher_from_route};
use mq_bridge::models::Endpoint;
use mq_bridge::plugin::test_support::{payload_texts, receive_one_batch};
use mq_bridge::CanonicalMessage;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MQB_FULL_PLUGIN, see the module docs"]
async fn a_disabled_built_in_runs_from_the_full_plugin() {
    let library = std::env::var("MQB_FULL_PLUGIN").expect("MQB_FULL_PLUGIN is not set");
    let dir = tempfile::tempdir().unwrap();
    let endpoint: Endpoint = serde_json::from_value(json!({
        "sled": { "path": dir.path().to_str().unwrap(), "tree": "q", "read_from_start": true }
    }))
    .unwrap();

    let before = create_publisher_from_route("r", &endpoint).await;
    assert!(
        before.is_err(),
        "this host must not have `sled` compiled in"
    );

    let infos = mq_bridge::plugin::load_endpoint_plugins(&library).unwrap();
    assert!(infos.iter().any(|info| info.name == "sled"));

    let publisher = create_publisher_from_route("r", &endpoint).await.unwrap();
    publisher
        .send(CanonicalMessage::new(b"through the plugin".to_vec(), None))
        .await
        .unwrap();
    publisher.flush().await.unwrap();

    let mut consumer = create_consumer_from_route("r", &endpoint).await.unwrap();
    let batch = receive_one_batch(consumer.as_mut(), Duration::from_secs(5)).await;
    assert_eq!(payload_texts(&batch.messages), ["through the plugin"]);
}
