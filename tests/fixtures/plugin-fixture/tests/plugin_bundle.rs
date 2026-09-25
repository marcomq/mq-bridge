//! A library exporting several plugins registers them together or not at all.
//!
//! Its own test binary: a successful load is permanent for the process.

use std::sync::Arc;

use mq_bridge::extensions::{
    get_endpoint_factory, get_middleware_factory, register_endpoint_factory,
    unregister_endpoint_factory,
};
use mq_bridge::plugin::{
    discover_endpoint_plugin_in, library_file_name, load_endpoint_plugins,
    test_support::build_plugin_cdylib,
};
use mq_bridge_plugin_fixture::FixtureFactory;

#[test]
fn a_conflict_in_one_plugin_registers_none_and_discovery_finds_the_second() {
    let built = build_plugin_cdylib(env!("CARGO_MANIFEST_DIR"), "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));

    register_endpoint_factory("fixture-sink", Arc::new(FixtureFactory)).unwrap();
    let error = load_endpoint_plugins(&built).expect_err("`fixture-sink` is taken");
    assert!(format!("{error:#}").contains("fixture-sink"), "{error:#}");
    assert!(get_endpoint_factory("fixture").is_none());
    assert!(get_middleware_factory("fixture").is_none());
    unregister_endpoint_factory("fixture-sink");

    let dir = std::env::temp_dir().join(format!("mqb-bundle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&built, dir.join(library_file_name("fixture-sink"))).unwrap();
    let info = discover_endpoint_plugin_in(&[dir], "fixture-sink")
        .expect("the library provides `fixture-sink`")
        .expect("the file exists");
    assert_eq!(info.name, "fixture-sink");
    assert!(get_endpoint_factory("fixture").is_some());
}
