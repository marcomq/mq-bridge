//! What discovery does with a candidate it must not accept.
//!
//! Its own test binary: the mismatch case loads the fixture, and the sibling
//! discovery suite asserts nothing is registered before its route asks. These
//! cases pass their directory in, so none of them touches the process env.

use std::path::PathBuf;

use mq_bridge::endpoints::create_consumer_from_route;
use mq_bridge::models::{Endpoint, EndpointType};
use mq_bridge::plugin::{
    discover_endpoint_plugin_in, library_file_name, test_support::build_plugin_cdylib,
};
use serde_json::json;

const WORKSPACE: &str = env!("CARGO_MANIFEST_DIR");

/// One directory per case, so no case's file can answer another's lookup.
fn plugin_dir(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mqb-discovery-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the plugin directory");
    dir
}

/// The file name is a convention the library itself never sees, so the two can
/// disagree — and the library is already in the process by the time we know.
#[test]
fn a_library_under_the_wrong_name_is_refused_and_says_what_it_provides() {
    let built = build_plugin_cdylib(WORKSPACE, "mq-bridge-plugin-fixture")
        .unwrap_or_else(|err| panic!("could not build the fixture cdylib: {err:#}"));
    let dir = plugin_dir("mismatch");
    std::fs::copy(&built, dir.join(library_file_name("not-the-fixture")))
        .expect("install the cdylib under a name it does not provide");

    let error = discover_endpoint_plugin_in(&[dir.clone()], "not-the-fixture")
        .map(|_| ())
        .expect_err("a library providing another endpoint cannot answer this name");
    let message = format!("{error:#}");

    assert!(message.contains("not-the-fixture"), "{message}");
    // The name it really provides, spelled as the file it should have been.
    assert!(message.contains(&library_file_name("fixture")), "{message}");
    // Nothing is ever unloaded, so it is in the process under its real name.
    assert!(
        mq_bridge::extensions::get_endpoint_factory("fixture").is_some(),
        "the refused library stays loaded: {message}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A file that cannot be loaded must not read as "no such endpoint": the name was
/// found, and hiding the loader's reason sends the reader looking in the wrong place.
#[test]
fn a_file_that_is_not_a_library_is_a_load_failure_not_a_missing_endpoint() {
    let dir = plugin_dir("unloadable");
    let name = "not-a-library";
    std::fs::write(
        dir.join(library_file_name(name)),
        b"this is not a shared library",
    )
    .expect("install the file");

    let error = discover_endpoint_plugin_in(&[dir.clone()], name)
        .map(|_| ())
        .expect_err("a candidate that fails to load is an error, not a miss");
    let message = format!("{error:#}");

    assert!(message.contains(name), "{message}");
    assert!(message.contains(&dir.display().to_string()), "{message}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Discovery resolves an endpoint name, so a plugin that provides only a
/// middleware cannot answer one — and says that, rather than leaving the route
/// to fail later on a factory that was never registered.
#[test]
fn a_plugin_with_only_a_middleware_cannot_answer_an_endpoint_name() {
    let name = "middleware-only-fixture";
    let built = build_plugin_cdylib(WORKSPACE, "mq-bridge-plugin-fixture-middleware")
        .unwrap_or_else(|err| panic!("could not build the middleware fixture cdylib: {err:#}"));
    let dir = plugin_dir("middleware-only");
    std::fs::copy(&built, dir.join(library_file_name(name))).expect("install the cdylib");

    let error = discover_endpoint_plugin_in(&[dir.clone()], name)
        .map(|_| ())
        .expect_err("a middleware is not an endpoint");
    let message = format!("{error:#}");

    assert!(message.contains(name), "{message}");
    assert!(message.contains("middleware"), "{message}");
    // Loading happens before the capability check, so the middleware it does
    // provide is registered and stays that way.
    assert!(
        mq_bridge::extensions::get_middleware_factory(name).is_some(),
        "the middleware half is loaded and keeps working: {message}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A name with no file at all is the ordinary case, and must stay a plain miss.
#[test]
fn a_name_with_no_file_is_not_an_error() {
    let dir = plugin_dir("empty");

    let found = discover_endpoint_plugin_in(&[dir.clone()], "nothing-is-installed-for-this")
        .expect("a miss is not an error");

    assert!(found.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// The name it looked for and where is the whole value of the route's error.
#[tokio::test]
async fn an_endpoint_no_plugin_provides_reports_what_it_looked_for() {
    let endpoint = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: "not-installed".to_string(),
            config: json!({ "queue": "discovery" }),
        },
        middlewares: vec![],
        handler: None,
    };

    let error = create_consumer_from_route("discovery", &endpoint)
        .await
        .map(|_| ())
        .expect_err("an endpoint with no factory and no library cannot resolve");
    let message = format!("{error:#}");

    assert!(message.contains("not-installed"), "{message}");
    assert!(
        message.contains(&library_file_name("not-installed")),
        "{message}"
    );
}
