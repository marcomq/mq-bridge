//! A host without the `compression` and `dedup` features applies them from the full plugin,
//! around its own `memory` endpoints.
//!
//! Ignored by default: it needs the plugin built first, and the host built without them.
//!
//! ```text
//! cargo build -p mq-bridge-full-plugin --no-default-features --features mq-bridge/compression,mq-bridge/dedup
//! MQB_FULL_PLUGIN=target/debug/libmq_bridge_full.so \
//!   cargo test --features plugin --test full_plugin_middleware_test -- --ignored
//! ```
#![cfg(all(
    feature = "plugin",
    not(feature = "compression"),
    not(feature = "dedup")
))]

use std::time::Duration;

use mq_bridge::endpoints::{create_consumer_from_route, create_publisher_from_route};
use mq_bridge::models::Endpoint;
use mq_bridge::plugin::test_support::{payload_texts, receive_one_batch};
use mq_bridge::traits::MessageDisposition;
use mq_bridge::CanonicalMessage;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MQB_FULL_PLUGIN, see the module docs"]
async fn disabled_built_in_middleware_runs_from_the_full_plugin() {
    let library = std::env::var("MQB_FULL_PLUGIN").expect("MQB_FULL_PLUGIN is not set");
    let output: Endpoint = serde_json::from_value(json!({
        "memory": { "topic": "full_plugin_middleware" },
        "middlewares": [{ "compression": { "algorithm": "zstd" } }]
    }))
    .unwrap();
    // The first entry is outermost: dedup sees what compression decompressed.
    let input: Endpoint = serde_json::from_value(json!({
        "memory": { "topic": "full_plugin_middleware" },
        "middlewares": [
            { "deduplication": { "store": "memory://full_plugin_middleware", "ttl_seconds": 60, "key": "${payload:id}" } },
            { "compression": { "algorithm": "zstd" } }
        ]
    }))
    .unwrap();

    let before = create_publisher_from_route("r", &output).await;
    assert!(
        before.is_err(),
        "this host must not have `compression` compiled in"
    );

    mq_bridge::plugin::load_endpoint_plugins(&library).unwrap();

    let publisher = create_publisher_from_route("r", &output).await.unwrap();
    let mut consumer = create_consumer_from_route("r", &input).await.unwrap();
    let mut round = async |payloads: &[&str]| {
        for payload in payloads {
            publisher
                .send(CanonicalMessage::new(payload.as_bytes().to_vec(), None))
                .await
                .unwrap();
        }
        publisher.flush().await.unwrap();
        let batch = receive_one_batch(consumer.as_mut(), Duration::from_secs(5)).await;
        let texts = payload_texts(&batch.messages);
        (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
            .await
            .unwrap();
        texts
    };

    let once = r#"{"id":"once"}"#;
    let twice = r#"{"id":"twice"}"#;
    assert_eq!(round(&[once]).await, [once]);
    assert_eq!(round(&[once, twice]).await, [twice]);
}
