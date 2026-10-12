//! Discovery resolves a middleware name the way it resolves an endpoint name.
//!
//! Its own test binary: the error suite loads the same fixture under the same
//! name, and a library is never unloaded.

use mq_bridge::plugin::{
    discover_middleware_plugin_in, library_file_name, test_support::build_plugin_cdylib,
};

const WORKSPACE: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn a_middleware_only_plugin_answers_its_middleware_name() {
    let name = "middleware-only-fixture";
    let built = build_plugin_cdylib(WORKSPACE, "mq-bridge-plugin-fixture-middleware")
        .unwrap_or_else(|err| panic!("could not build the middleware fixture cdylib: {err:#}"));
    let dir = std::env::temp_dir().join(format!("mqb-mw-discovery-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the plugin directory");
    std::fs::copy(&built, dir.join(library_file_name(name))).expect("install the cdylib");

    assert!(mq_bridge::extensions::get_middleware_factory(name).is_none());
    let info = discover_middleware_plugin_in(std::slice::from_ref(&dir), name)
        .expect("the middleware plugin loads")
        .expect("the installed library answers the name");

    assert_eq!(info.name, name);
    assert!(info.supports_middleware);
    assert!(mq_bridge::extensions::get_middleware_factory(name).is_some());

    std::fs::remove_dir_all(&dir).ok();
}
