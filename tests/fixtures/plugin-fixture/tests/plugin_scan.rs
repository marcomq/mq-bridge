//! A route's lookup opens only the file named for the endpoint; listing every
//! `libmq_bridge_*` on the search path takes an explicit call.
//!
//! Its own test binary: a successful load is permanent for the process.

use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::plugin::{
    discover_all_endpoint_plugins_in, discover_endpoint_plugin_in, library_file_name,
    test_support::build_plugin_cdylib,
};

#[test]
fn only_an_explicit_scan_loads_a_library_no_route_named() {
    let built = build_plugin_cdylib(env!("CARGO_MANIFEST_DIR"), "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));
    let dir = std::env::temp_dir().join(format!("mqb-scan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&built, dir.join(library_file_name("bundle"))).unwrap();
    // A helper library next to a plugin, and a file that is not a library at all.
    std::fs::write(dir.join(library_file_name("bundle_go")), b"not a library").unwrap();
    std::fs::write(dir.join("README.txt"), b"ignored").unwrap();
    let dirs = [dir];

    assert!(discover_endpoint_plugin_in(&dirs, "fixture-sink")
        .unwrap()
        .is_none());
    assert!(
        get_endpoint_factory("fixture").is_none(),
        "a route's lookup loaded a library it did not name"
    );

    let infos = discover_all_endpoint_plugins_in(&dirs);
    assert!(
        infos.iter().any(|info| info.name == "fixture-sink"),
        "{infos:?}"
    );
    assert!(get_endpoint_factory("fixture").is_some());
}
