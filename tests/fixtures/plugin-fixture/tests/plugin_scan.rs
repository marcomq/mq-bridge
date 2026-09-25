//! An endpoint whose name matches no library file is found by scanning every
//! `libmq_bridge_*` on the search path.
//!
//! Its own test binary: a successful load is permanent for the process.

use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::plugin::{
    discover_endpoint_plugin_in, library_file_name, test_support::build_plugin_cdylib,
};

#[test]
fn an_endpoint_named_unlike_its_library_is_found_by_the_scan() {
    let built = build_plugin_cdylib(env!("CARGO_MANIFEST_DIR"), "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));
    let dir = std::env::temp_dir().join(format!("mqb-scan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&built, dir.join(library_file_name("bundle"))).unwrap();
    // A helper library next to a plugin, and a file that is not a library at all.
    std::fs::write(dir.join(library_file_name("bundle_go")), b"not a library").unwrap();
    std::fs::write(dir.join("README.txt"), b"ignored").unwrap();
    let dirs = [dir];

    assert!(discover_endpoint_plugin_in(&dirs, "no-such-endpoint")
        .unwrap()
        .is_none());
    assert!(
        get_endpoint_factory("fixture").is_some(),
        "the scan loaded the whole library"
    );

    let info = discover_endpoint_plugin_in(&dirs, "fixture-sink")
        .unwrap()
        .expect("a library loaded by an earlier scan still answers");
    assert_eq!(info.name, "fixture-sink");
}
