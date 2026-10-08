//! `disable_plugin_loading` is process-wide and one-way, so it gets a test binary of its own.
#![cfg(feature = "plugin")]

use mq_bridge::plugin::{
    disable_plugin_loading, discover_all_endpoint_plugins, discover_endpoint_plugin,
    discovery_enabled, load_endpoint_plugin, plugin_loading_disabled, search_path_hint,
};

#[test]
fn disabling_plugin_loading_refuses_every_later_load() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("libmq_bridge_latch.so");
    std::fs::write(&library, b"not a library").unwrap();

    assert!(!plugin_loading_disabled());
    let before = load_endpoint_plugin(&library).unwrap_err().to_string();
    assert!(before.contains("failed to load plugin library"), "{before}");

    disable_plugin_loading();

    assert!(plugin_loading_disabled());
    assert!(!discovery_enabled());
    let after = load_endpoint_plugin(&library).unwrap_err().to_string();
    assert!(after.contains("plugin loading is disabled"), "{after}");
    assert!(discover_endpoint_plugin("latch").unwrap().is_none());
    assert!(discover_all_endpoint_plugins().is_empty());
    assert!(search_path_hint("latch").contains("disabled"));
}
