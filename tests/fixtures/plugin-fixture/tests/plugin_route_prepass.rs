//! A route loads the plugins it names before its startup checks ask the registry,
//! including a name nested in a fanout whose library file is named for another.
//!
//! Its own test binary: it sets the search path and discovery switch in the process env.

use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::models::{Endpoint, EndpointType};
use mq_bridge::plugin::discovery::{DISCOVERY_VAR, SEARCH_PATH_VAR};
use mq_bridge::plugin::{library_file_name, test_support::build_plugin_cdylib};
use mq_bridge::Route;
use serde_json::json;

fn route_naming_fixture_sink() -> Route {
    let sink = Endpoint::new(EndpointType::Custom {
        name: "fixture-sink".to_string(),
        config: json!({ "queue": "prepass" }),
    });
    Route::new(
        Endpoint::new_memory("prepass", 10),
        Endpoint::new(EndpointType::Fanout(vec![sink])),
    )
}

#[test]
fn a_route_loads_the_plugins_it_names_before_its_checks() {
    let built = build_plugin_cdylib(env!("CARGO_MANIFEST_DIR"), "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));
    let dir = std::env::temp_dir().join(format!("mqb-prepass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&built, dir.join(library_file_name("bundle"))).unwrap();
    unsafe { std::env::set_var(SEARCH_PATH_VAR, &dir) };

    unsafe { std::env::set_var(DISCOVERY_VAR, "0") };
    route_naming_fixture_sink().check("prepass", None).unwrap();
    assert!(
        get_endpoint_factory("fixture-sink").is_none(),
        "discovery is off, so nothing may load"
    );

    unsafe { std::env::remove_var(DISCOVERY_VAR) };
    route_naming_fixture_sink().check("prepass", None).unwrap();
    assert!(
        get_endpoint_factory("fixture-sink").is_some(),
        "the check should load the library that provides the nested name"
    );

    std::fs::remove_dir_all(&dir).ok();
}
