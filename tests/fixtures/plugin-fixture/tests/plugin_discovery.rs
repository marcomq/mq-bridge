//! Finding a plugin from the endpoint name a route asks for.
//!
//! Its own test binary on purpose, holding this one test: discovery only runs
//! while the name is still unregistered, the sibling suites load this same
//! fixture, and the search path has to come from the process env to prove the
//! route reaches it — so nothing else may be running while it is set.

use std::path::PathBuf;

use mq_bridge::endpoints::create_consumer_from_route;
use mq_bridge::models::{Endpoint, EndpointType};
use mq_bridge::plugin::{library_file_name, test_support::build_plugin_cdylib};
use serde_json::json;

const WORKSPACE: &str = env!("CARGO_MANIFEST_DIR");

/// Installs the fixture cdylib under the file name discovery looks for.
fn install_plugin() -> PathBuf {
    let built = build_plugin_cdylib(WORKSPACE, "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));
    let dir = std::env::temp_dir().join(format!("mqb-discovery-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the plugin directory");
    std::fs::copy(&built, dir.join(library_file_name("fixture"))).expect("install the cdylib");
    dir
}

fn custom_endpoint(name: &str) -> Endpoint {
    Endpoint {
        endpoint_type: EndpointType::Custom {
            name: name.to_string(),
            config: json!({ "queue": "discovery" }),
        },
        middlewares: vec![],
        handler: None,
    }
}

#[tokio::test]
async fn a_route_finds_an_installed_plugin_by_the_endpoint_name_it_asks_for() {
    let dir = install_plugin();
    std::env::set_var(mq_bridge::plugin::discovery::SEARCH_PATH_VAR, &dir);

    assert!(
        mq_bridge::extensions::get_endpoint_factory("fixture").is_none(),
        "nothing may be registered before the route asks for the name"
    );

    create_consumer_from_route("discovery", &custom_endpoint("fixture"))
        .await
        .expect("the route should resolve `fixture` from the search path");

    assert!(
        mq_bridge::extensions::get_endpoint_factory("fixture").is_some(),
        "discovery registers the endpoint, so a second route needs no search"
    );

    // Runs last: an unknown name scans the whole search path, build tree included.
    let error = create_consumer_from_route("discovery", &custom_endpoint("not-installed"))
        .await
        .map(|_| ())
        .expect_err("an endpoint with no factory and no library cannot resolve");
    let message = format!("{error:#}");
    assert!(
        message.contains(&library_file_name("not-installed")),
        "{message}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
