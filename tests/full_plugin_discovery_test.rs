//! A host without the `sled` feature finds `sled:` in a full plugin library by discovery alone.
//!
//! Ignored by default: it needs the plugin built first, and the host built without `sled`.
//!
//! ```text
//! cargo build -p mq-bridge-full-plugin --no-default-features --features mq-bridge/sled
//! MQB_PLUGIN_DIR=target/debug MQB_PLUGIN_DISCOVERY=1 \
//!   cargo test --features plugin --test full_plugin_discovery_test -- --ignored
//! ```
#![cfg(all(feature = "plugin", not(feature = "sled")))]

use mq_bridge::endpoints::check_publisher;
use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::models::Endpoint;
use serde_json::json;

#[test]
#[ignore = "needs MQB_PLUGIN_DIR and MQB_PLUGIN_DISCOVERY=1, see the module docs"]
fn a_disabled_built_in_is_found_by_discovery() {
    assert!(
        mq_bridge::plugin::discovery_enabled(),
        "MQB_PLUGIN_DISCOVERY=1 is not set"
    );
    assert!(get_endpoint_factory("sled").is_none());
    let dir = tempfile::tempdir().unwrap();
    let endpoint: Endpoint = serde_json::from_value(json!({
        "sled": { "path": dir.path().to_str().unwrap(), "tree": "q" }
    }))
    .unwrap();

    check_publisher("r", &endpoint, None).unwrap();
    assert!(get_endpoint_factory("sled").is_some());
}
