//! What the plugin boundary must preserve.
//!
//! Every check here runs the *same* fixture endpoint twice where it matters:
//! linked directly as Rust code, and loaded as a compiled plugin. A difference
//! between the two is a defect in the ABI, the loader or the SDK.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mq_bridge::errors::{ConsumerError, PublisherError};
use mq_bridge::models::{Endpoint, EndpointType, Middleware};
use mq_bridge::plugin::conformance::{self, ConformanceOptions};
use mq_bridge::plugin::load_endpoint_plugin;
use mq_bridge::plugin::test_support::{
    build_plugin_cdylib, payload_texts, receive_at_least, receive_one_batch,
};
use mq_bridge::route::Route;
use mq_bridge::traits::{
    CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher,
};
use mq_bridge::{CanonicalMessage, SentBatch};
use mq_bridge_plugin_fixture::{commit_log_queue, drop_log_queue, FixtureFactory};
use serde_json::{json, Value};

const WORKSPACE: &str = env!("CARGO_MANIFEST_DIR");
const WAIT: Duration = Duration::from_secs(5);

/// Builds each fixture library once, however many tests ask for it.
fn library(package: &str) -> PathBuf {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    static BAD_ABI: OnceLock<PathBuf> = OnceLock::new();
    let slot = match package {
        "mq-bridge-plugin-fixture" => &FIXTURE,
        _ => &BAD_ABI,
    };
    slot.get_or_init(|| {
        build_plugin_cdylib(WORKSPACE, package)
            .unwrap_or_else(|err| panic!("could not build `{package}`: {err:#}"))
    })
    .clone()
}

/// The factory the host built from the loaded plugin.
fn plugin_factory() -> Arc<dyn CustomEndpointFactory> {
    let info = load_endpoint_plugin(library("mq-bridge-plugin-fixture"))
        .expect("the fixture plugin should load");
    assert_eq!(info.name, "fixture");
    assert!(info.supports_consumer && info.supports_publisher);
    mq_bridge::extensions::get_endpoint_factory(&info.name)
        .expect("loading a plugin registers its endpoint")
}

async fn consumer(factory: &dyn CustomEndpointFactory, config: Value) -> Box<dyn MessageConsumer> {
    factory
        .create_consumer("test", &config)
        .await
        .expect("create consumer")
}

async fn publisher(
    factory: &dyn CustomEndpointFactory,
    config: Value,
) -> Box<dyn MessagePublisher> {
    factory
        .create_publisher("test", &config)
        .await
        .expect("create publisher")
}

fn messages(payloads: &[&str]) -> Vec<CanonicalMessage> {
    payloads
        .iter()
        .map(|p| CanonicalMessage::from(*p))
        .collect()
}

async fn publish(factory: &dyn CustomEndpointFactory, queue: &str, payloads: &[&str]) {
    let publisher = publisher(factory, json!({ "queue": queue })).await;
    publisher
        .send_batch(messages(payloads))
        .await
        .expect("send batch");
    publisher.flush().await.expect("flush");
}

/// Consumer of the queue the fixture logs every commit of `queue` to.
async fn commit_log(factory: &dyn CustomEndpointFactory, queue: &str) -> Box<dyn MessageConsumer> {
    consumer(factory, json!({ "queue": commit_log_queue(queue) })).await
}

fn meta<'a>(message: &'a CanonicalMessage, key: &str) -> Option<&'a str> {
    message.metadata.get(key).map(String::as_str)
}

/// Sorted payloads of the first `expected` messages on `queue`.
async fn drain(factory: &dyn CustomEndpointFactory, queue: &str, expected: usize) -> Vec<String> {
    let mut consumer = consumer(factory, json!({ "queue": queue })).await;
    let mut payloads = payload_texts(&receive_at_least(&mut *consumer, expected, WAIT).await);
    payloads.sort();
    payloads
}

/// A route between two fixture queues, the plugin middleware on `middleware`'s side.
fn fixture_route(input: &str, output: &str, middleware: Option<(&str, Value)>) -> Route {
    let endpoint = |queue: &str, side: &str| {
        let mut endpoint = Endpoint::new(EndpointType::Custom {
            name: "fixture".to_string(),
            config: json!({ "queue": queue }),
        });
        if let Some((_, config)) = middleware.as_ref().filter(|(at, _)| *at == side) {
            endpoint.middlewares = vec![Middleware::Custom {
                name: "fixture".to_string(),
                config: config.clone(),
            }];
        }
        endpoint
    };
    Route::new(endpoint(input, "input"), endpoint(output, "output"))
}

/// The fixture never ends its stream, so a route runs until cancelled.
async fn run_route(route: Route, name: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        route.run_until_err(name, None, None),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_endpoint_conforms_the_same_linked_and_loaded() {
    let run = |factory: Arc<dyn CustomEndpointFactory>, queue: &'static str| async move {
        conformance::run(
            factory.as_ref(),
            ConformanceOptions::new(queue, json!({ "queue": queue })),
        )
        .await
        .unwrap_or_else(|err| panic!("{queue} should pass conformance: {err:#}"))
    };
    let direct = run(Arc::new(FixtureFactory), "conformance-direct").await;
    let loaded = run(plugin_factory(), "conformance-plugin").await;
    assert!(direct.contains(&"round_trip"));
    assert_eq!(direct, loaded);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_moves_messages_through_plugin_endpoints() {
    let factory = plugin_factory();
    publish(&*factory, "route-in", &["a", "b", "c"]).await;
    run_route(fixture_route("route-in", "route-out", None), "plugin_route").await;
    assert_eq!(drain(&*factory, "route-out", 3).await, ["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledgement_happens_only_when_the_batch_is_committed() {
    let factory = plugin_factory();
    publish(&*factory, "ack-timing", &["one"]).await;
    let mut consumer = consumer(&*factory, json!({ "queue": "ack-timing" })).await;
    let batch = receive_one_batch(&mut *consumer, WAIT).await;

    let mut commits = commit_log(&*factory, "ack-timing").await;
    let early = commits.receive_batch(8).await.unwrap().messages;
    assert!(
        early.is_empty(),
        "receiving a batch must not acknowledge it"
    );

    (batch.commit)(vec![MessageDisposition::Ack])
        .await
        .expect("commit");
    let logged = receive_one_batch(&mut *commits, WAIT).await.messages;
    assert_eq!(payload_texts(&logged), ["one"]);
    assert_eq!(meta(&logged[0], "disposition"), Some("ack"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nacked_batch_is_redelivered_and_reported_as_nacked() {
    let factory = plugin_factory();
    publish(&*factory, "nack-timing", &["retry-me"]).await;
    let mut consumer = consumer(&*factory, json!({ "queue": "nack-timing" })).await;
    let batch = receive_one_batch(&mut *consumer, WAIT).await;
    (batch.commit)(vec![MessageDisposition::Nack])
        .await
        .expect("commit");

    let again = receive_one_batch(&mut *consumer, WAIT).await.messages;
    assert_eq!(payload_texts(&again), ["retry-me"]);
    let logged = receive_one_batch(&mut *commit_log(&*factory, "nack-timing").await, WAIT).await;
    assert_eq!(meta(&logged.messages[0], "disposition"), Some("nack"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_dropped_without_committing_acknowledges_nothing() {
    let factory = plugin_factory();
    publish(&*factory, "dropped", &["not-acked"]).await;
    let mut consumer = consumer(&*factory, json!({ "queue": "dropped" })).await;
    drop(receive_one_batch(&mut *consumer, WAIT).await);

    let logged = commit_log(&*factory, "dropped")
        .await
        .receive_batch(8)
        .await
        .unwrap();
    assert!(
        logged.messages.is_empty(),
        "dropping a batch must not acknowledge it"
    );
}

/// An endpoint's `Drop` may spawn, so the plugin frees it inside its runtime.
#[tokio::test(flavor = "multi_thread")]
async fn a_freed_publisher_is_dropped_inside_the_plugin_runtime() {
    let factory = plugin_factory();
    drop(publisher(&*factory, json!({ "queue": "freed" })).await);
    let mut drops = consumer(&*factory, json!({ "queue": drop_log_queue("freed") })).await;
    let logged = receive_one_batch(&mut *drops, WAIT).await.messages;
    assert_eq!(payload_texts(&logged), ["in-runtime"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn consumer_error_classes_survive_the_abi() {
    let factory = plugin_factory();
    let error = |fail: &'static str| {
        let factory = Arc::clone(&factory);
        async move {
            let config = json!({ "queue": "errors", "fail_receive": fail });
            let mut consumer = consumer(&*factory, config).await;
            consumer
                .receive_batch(1)
                .await
                .expect_err("the fixture was asked to fail")
        }
    };
    assert!(matches!(
        error("retryable").await,
        ConsumerError::Connection(_)
    ));
    assert!(matches!(
        error("end_of_stream").await,
        ConsumerError::EndOfStream
    ));
    // The plugin's own message has to reach the host, not just "the plugin failed".
    let permanent = error("permanent").await;
    assert!(matches!(permanent, ConsumerError::Permanent(_)));
    assert!(
        permanent
            .to_string()
            .contains("fixture injected a permanent"),
        "{permanent}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn publisher_error_classes_survive_the_abi() {
    let factory = plugin_factory();
    for (fail, expect_retryable) in [("retryable", true), ("permanent", false)] {
        let publisher = publisher(&*factory, json!({ "queue": "errors", "fail_send": fail })).await;
        let error = publisher
            .send_batch(messages(&["x"]))
            .await
            .expect_err("the fixture was asked to fail");
        match (&error, expect_retryable) {
            (PublisherError::Retryable(_), true) | (PublisherError::NonRetryable(_), false) => {}
            _ => panic!("`fail_send: {fail}` produced the wrong error class: {error}"),
        }
    }
}

/// ABI 1.1. Before it, an order-sensitive plugin sink was published to in
/// parallel whenever the route ran with `concurrency > 1`.
#[tokio::test(flavor = "multi_thread")]
async fn publisher_ordering_requirement_is_the_same_linked_and_loaded() {
    for ordered in [true, false] {
        let config = json!({ "queue": "ordering", "requires_ordered_publish": ordered });
        let direct = publisher(&FixtureFactory, config.clone()).await;
        let loaded = publisher(&*plugin_factory(), config).await;
        assert_eq!(
            (
                direct.requires_ordered_publish(),
                loaded.requires_ordered_publish()
            ),
            (ordered, ordered)
        );
    }
}

/// Payloads of the messages a publish reported as failed.
fn failed_texts(sent: &SentBatch) -> Vec<String> {
    let SentBatch::Partial { failed, .. } = sent else {
        panic!("expected a partial batch, got {sent:?}");
    };
    payload_texts(failed.iter().map(|(message, _)| message))
}

/// ABI 1.1. A batch that half landed comes back as `Partial`, naming exactly the
/// messages at the failed indices. Under 1.0 a retry duplicated the rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_publish_survives_the_abi() {
    let factory = plugin_factory();
    let config = json!({ "queue": "partial", "fail_send_at": [1, 3] });
    let payloads = ["a", "b", "c", "d", "e"];
    let loaded = publisher(&*factory, config.clone()).await;
    let sent = loaded
        .send_batch(messages(&payloads))
        .await
        .expect("partial is not an error");
    assert_eq!(failed_texts(&sent), ["b", "d"]);
    let SentBatch::Partial { failed, .. } = &sent else {
        unreachable!()
    };
    assert!(failed
        .iter()
        .all(|(_, error)| matches!(error, PublisherError::Retryable(_))));

    let direct = publisher(&FixtureFactory, config.clone()).await;
    assert_eq!(
        failed_texts(&direct.send_batch(messages(&payloads)).await.unwrap()),
        ["b", "d"]
    );

    // The route acks the rest on the strength of this, so they must have landed.
    assert_eq!(drain(&*factory, "partial", 3).await, ["a", "c", "e"]);
}

/// A permanent per-message failure must not arrive as retryable: the route
/// would nack it forever.
#[tokio::test(flavor = "multi_thread")]
async fn partial_failure_classes_survive_the_abi() {
    let config = json!({ "queue": "partial-class", "fail_send_at": [0], "fail_send": "permanent" });
    let publisher = publisher(&*plugin_factory(), config).await;
    let sent = publisher
        .send_batch(messages(&["x", "y"]))
        .await
        .expect("partial");
    let SentBatch::Partial { failed, .. } = sent else {
        panic!("expected a partial batch, got {sent:?}");
    };
    assert_eq!(failed.len(), 1);
    assert!(
        matches!(failed[0].1, PublisherError::NonRetryable(_)),
        "{}",
        failed[0].1
    );
}

/// A batch where nothing landed stays a batch error, which keeps `Connection`
/// able to mean "reconnect the endpoint".
#[tokio::test(flavor = "multi_thread")]
async fn a_wholly_failed_batch_is_still_an_error() {
    let config = json!({ "queue": "partial-none", "fail_send_at": [0, 1] });
    let publisher = publisher(&*plugin_factory(), config).await;
    let error = publisher
        .send_batch(messages(&["x", "y"]))
        .await
        .expect_err("every message failed, so the batch failed");
    assert!(matches!(error, PublisherError::Retryable(_)), "{error}");
    assert!(
        error.to_string().contains("2 of 2 messages failed"),
        "{error}"
    );
}

/// Response payloads and failed payloads of one publish.
async fn publish_with_responses(
    factory: &dyn CustomEndpointFactory,
    config: &Value,
    payloads: &[&str],
) -> (Vec<String>, Vec<String>) {
    let publisher = publisher(factory, config.clone()).await;
    let sent = publisher
        .send_batch(messages(payloads))
        .await
        .expect("send batch");
    let failed = failed_texts(&sent);
    let SentBatch::Partial { responses, .. } = sent else {
        unreachable!()
    };
    (payload_texts(&responses.unwrap_or_default()), failed)
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_responses_and_failures_survive_the_abi() {
    let cases: [(Value, &[&str], &[&str]); 2] = [
        (
            json!({ "queue": "responses", "respond": true }),
            &["re:a", "re:b", "re:c"],
            &[],
        ),
        (
            json!({ "queue": "responses-partial", "respond": true, "fail_send_at": [1] }),
            &["re:a", "re:c"],
            &["b"],
        ),
    ];
    for (config, responses, failed) in cases {
        let direct = publish_with_responses(&FixtureFactory, &config, &["a", "b", "c"]).await;
        let loaded = publish_with_responses(&*plugin_factory(), &config, &["a", "b", "c"]).await;
        assert_eq!(direct.0, responses);
        assert_eq!(direct.1, failed);
        assert_eq!(direct, loaded);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_disposition_reaches_the_plugin() {
    let factory = plugin_factory();
    publish(&*factory, "reply-commit", &["ask", "plain"]).await;
    let mut consumer = consumer(&*factory, json!({ "queue": "reply-commit" })).await;
    let batch = receive_one_batch(&mut *consumer, WAIT).await;
    assert_eq!(batch.messages.len(), 2);
    let reply = MessageDisposition::Reply(CanonicalMessage::from("answer"));
    (batch.commit)(vec![reply, MessageDisposition::Ack])
        .await
        .expect("commit");

    let mut commits = commit_log(&*factory, "reply-commit").await;
    let logged = receive_at_least(&mut *commits, 2, WAIT).await;
    let field = |payload: &str, key: &str| {
        let message = logged
            .iter()
            .find(|m| m.get_payload_str() == payload)
            .unwrap();
        meta(message, key).map(str::to_owned)
    };
    assert_eq!(field("ask", "disposition").as_deref(), Some("reply"));
    assert_eq!(field("ask", "reply").as_deref(), Some("answer"));
    assert_eq!(field("plain", "disposition").as_deref(), Some("ack"));
    assert_eq!(field("plain", "reply"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_plugin_of_a_library_is_registered() {
    let factory = plugin_factory();
    let infos = mq_bridge::plugin::load_endpoint_plugins(library("mq-bridge-plugin-fixture"))
        .expect("loading again returns the original registration");
    let names: Vec<_> = infos.iter().map(|info| info.name.as_str()).collect();
    assert_eq!(names, ["fixture", "fixture-sink"]);
    assert!(infos[1].supports_publisher && !infos[1].supports_consumer);
    assert!(infos[0].supports_middleware && !infos[1].supports_middleware);

    let sink = mq_bridge::extensions::get_endpoint_factory("fixture-sink")
        .expect("the second plugin is registered too");
    publish(&*sink, "second-plugin", &["one"]).await;
    assert_eq!(drain(&*factory, "second-plugin", 1).await, ["one"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoint_status_survives_the_abi() {
    async fn statuses(factory: &dyn CustomEndpointFactory) -> (Value, Value) {
        let config = json!({ "queue": "status" });
        let publisher = publisher(factory, config.clone()).await;
        let consumer = consumer(factory, config).await;
        (
            serde_json::to_value(publisher.status().await).unwrap(),
            serde_json::to_value(consumer.status().await).unwrap(),
        )
    }
    let direct = statuses(&FixtureFactory).await;
    assert_eq!(direct.0["target"], "status");
    assert_eq!(direct.0["details"], json!({ "fixture": true }));
    assert_eq!(direct, statuses(&*plugin_factory()).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_inside_the_plugin_becomes_an_error() {
    let config = json!({ "queue": "panic", "panic_on_receive": true });
    let mut consumer = consumer(&*plugin_factory(), config).await;
    let error = consumer
        .receive_batch(1)
        .await
        .expect_err("a panicking plugin must not unwind into the host");
    assert!(matches!(error, ConsumerError::Permanent(_)), "{error}");
    assert!(error.to_string().contains("panicked"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_configuration_is_rejected_at_creation() {
    let config = json!({ "queue": "x", "unknown_field": 1 });
    let Err(error) = plugin_factory().create_consumer("bad", &config).await else {
        panic!("unknown configuration fields must be rejected");
    };
    let text = format!("{error:#}");
    assert!(
        text.contains("invalid fixture endpoint configuration"),
        "{text}"
    );
    assert!(
        matches!(
            error.downcast_ref::<ConsumerError>(),
            Some(ConsumerError::Permanent(_))
        ),
        "a rejected config must stop the route: {text}"
    );
}

#[test]
fn a_plugin_built_against_another_abi_major_is_rejected() {
    let error = load_endpoint_plugin(library("mq-bridge-plugin-fixture-bad-abi"))
        .expect_err("an incompatible plugin must not load");
    let text = format!("{error:#}");
    assert!(
        text.contains("incompatible with host ABI major version"),
        "{text}"
    );
    assert!(text.contains("rebuild the plugin"), "{text}");
}

#[test]
fn loading_the_same_library_twice_is_idempotent() {
    let first = load_endpoint_plugin(library("mq-bridge-plugin-fixture")).unwrap();
    let second = load_endpoint_plugin(library("mq-bridge-plugin-fixture")).unwrap();
    assert_eq!(first, second);
}

/// Same plugin, different file: the loader cannot dedupe by path, so the name
/// conflict must be caught instead of silently replacing the endpoint.
#[test]
fn a_second_library_claiming_the_same_endpoint_name_is_rejected() {
    let source = library("mq-bridge-plugin-fixture");
    load_endpoint_plugin(&source).expect("first load");
    let name = source.file_name().unwrap().to_string_lossy();
    let copy = source.with_file_name(format!("copy_of_{name}"));
    std::fs::copy(&source, &copy).expect("copy the plugin");

    let loaded = load_endpoint_plugin(&copy);
    let _ = std::fs::remove_file(&copy);
    let text = format!(
        "{:#}",
        loaded.expect_err("a duplicate endpoint name must be rejected")
    );
    assert!(text.contains("already registered"), "{text}");
}

#[test]
fn a_file_that_is_not_a_plugin_is_rejected_with_its_path() {
    let error = load_endpoint_plugin("/nonexistent/libmissing.so").unwrap_err();
    assert!(
        format!("{error:#}").contains("plugin library not found"),
        "{error:#}"
    );
}

// ---------------------------------------------------------- config schema

/// The schema is read *about* the plugin rather than through it, so a
/// difference is invisible until a form or a URI is wrong.
#[test]
fn the_configuration_schema_is_the_same_linked_and_loaded() {
    let info = load_endpoint_plugin(library("mq-bridge-plugin-fixture")).unwrap();
    let loaded = info
        .endpoint_schema()
        .expect("the fixture describes itself");
    assert_eq!(loaded, FixtureFactory.config_schema().unwrap());
    assert_eq!(
        info.middleware_schema().expect("and its middleware"),
        mq_bridge::plugin::sdk::MiddlewareFactory::config_schema(
            &mq_bridge_plugin_fixture::FixtureMiddlewareFactory
        )
        .unwrap()
    );
    assert_eq!(loaded["properties"]["queue"]["x-mqb-uri"], json!("path"));
}

/// ABI 1.2 asks the plugin per config, so a guarantee can depend on a field.
#[test]
fn delivery_guarantees_follow_the_config_across_the_boundary() {
    let loaded = plugin_factory();
    for config in [
        json!({ "queue": "delivery" }),
        json!({ "queue": "delivery", "idempotent": true, "acknowledges": false }),
    ] {
        let answers = |factory: &dyn CustomEndpointFactory| {
            (
                factory.idempotent_sink(&config),
                factory.acknowledges(&config),
            )
        };
        assert_eq!(answers(&*loaded), answers(&FixtureFactory), "{config}");
    }
    assert!(loaded.idempotent_sink(&json!({ "idempotent": true })));
    assert!(!loaded.acknowledges(&json!({ "acknowledges": false })));
}

/// The fixture's config has no `url` field and denies unknown ones, so only the
/// declared schema can map a URI onto it, and a bool arrives from a URI as text.
/// The mapped config must then open the endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_uri_maps_onto_the_schema_the_plugin_declared() {
    let factory = plugin_factory();
    let config = mq_bridge::plugin::endpoint_uri_schema("fixture")
        .config_from_uri("fixture://_/orders?commit_requires_order=false&fail_send_at=1,3")
        .expect("map the uri");
    assert_eq!(config["queue"], json!("orders"));
    assert_eq!(config["commit_requires_order"], json!(false));
    assert_eq!(config["fail_send_at"], json!([1, 3]));
    assert!(!config.contains_key("url"), "{config:?}");

    let consumer = consumer(&*factory, Value::Object(config)).await;
    assert!(!consumer.commit_requires_order());
}

// ------------------------------------------------------------- middleware

/// The library also exports a middleware named `fixture`; loading registers both.
#[test]
fn loading_a_plugin_registers_its_middleware_too() {
    let info = load_endpoint_plugin(library("mq-bridge-plugin-fixture")).unwrap();
    assert!(info.supports_middleware);
    assert!(mq_bridge::extensions::get_middleware_factory("fixture").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_middleware_rewrites_and_drops_on_either_side() {
    let factory = plugin_factory();
    for side in ["input", "output"] {
        let (input, output) = (format!("mw-{side}"), format!("mw-{side}-out"));
        publish(&*factory, &input, &["keep-one", "skip-two", "keep-three"]).await;
        let config = json!({ "drop_prefix": "skip-", "suffix": format!("-{side}") });
        run_route(fixture_route(&input, &output, Some((side, config))), &input).await;
        assert_eq!(
            drain(&*factory, &output, 2).await,
            [format!("keep-one-{side}"), format!("keep-three-{side}")]
        );
    }
}

/// A message the middleware drops must be acknowledged, or the source hands it
/// back forever.
#[tokio::test(flavor = "multi_thread")]
async fn messages_dropped_by_a_plugin_middleware_are_acknowledged() {
    let factory = plugin_factory();
    publish(&*factory, "mw-drop", &["skip-me", "keep-me"]).await;
    let middleware = Some(("input", json!({ "drop_prefix": "skip-" })));
    run_route(
        fixture_route("mw-drop", "mw-drop-out", middleware),
        "mw_drop",
    )
    .await;
    assert_eq!(drain(&*factory, "mw-drop-out", 1).await, ["keep-me"]);

    let logged = receive_one_batch(&mut *commit_log(&*factory, "mw-drop").await, WAIT).await;
    let acked = logged
        .messages
        .iter()
        .filter(|m| meta(m, "disposition") == Some("ack"));
    let mut acked = payload_texts(acked);
    acked.sort();
    assert_eq!(acked, ["keep-me", "skip-me"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_plugin_middleware_surfaces_its_message() {
    let factory = plugin_factory();
    let middleware = mq_bridge::extensions::get_middleware_factory("fixture")
        .expect("the fixture plugin registers a middleware");
    let consumer = consumer(&*factory, json!({ "queue": "mw-fail" })).await;
    let mut wrapped = middleware
        .apply_consumer(consumer, "mw-fail", &json!({ "fail": true }))
        .await
        .expect("apply middleware");

    publish(&*factory, "mw-fail", &["anything"]).await;
    let error = wrapped
        .receive_batch(4)
        .await
        .expect_err("the middleware was asked to fail");
    assert!(error.to_string().contains("configured to fail"), "{error}");
}

/// A cancelled route drops its endpoints while an ABI call may still be inside
/// the plugin. Freeing the handle there segfaulted before handles were
/// refcounted, and only sometimes, so this hammers the window.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_route_mid_call_does_not_free_a_handle_still_in_use() {
    let factory = plugin_factory();
    publish(&*factory, "cancel-in", &["a", "b", "c"]).await;
    for round in 0..25 {
        let middleware = Some(("input", json!({ "drop_prefix": "skip-" })));
        let route = fixture_route("cancel-in", "cancel-out", middleware);
        // The fixture returns empty batches, so the consumer is almost always
        // parked inside the plugin when the timeout fires.
        let name = format!("cancel_{round}");
        let _ = tokio::time::timeout(
            Duration::from_millis(15),
            route.run_until_err(&name, None, None),
        )
        .await;
    }
}
