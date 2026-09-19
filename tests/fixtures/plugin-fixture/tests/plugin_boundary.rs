//! What the plugin boundary must preserve.
//!
//! Every check here runs the *same* fixture endpoint twice where it matters:
//! linked directly as Rust code, and loaded as a compiled plugin. A difference
//! between the two is a defect in the ABI, the loader or the SDK.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mq_bridge::errors::{ConsumerError, PublisherError};
use mq_bridge::plugin::conformance::{self, ConformanceOptions};
use mq_bridge::plugin::{load_endpoint_plugin, test_support::build_plugin_cdylib};
use mq_bridge::traits::{CustomEndpointFactory, MessageDisposition};
use mq_bridge::{CanonicalMessage, ReceivedBatch, SentBatch};
use mq_bridge_plugin_fixture::FixtureFactory;
use serde_json::json;

const WORKSPACE: &str = env!("CARGO_MANIFEST_DIR");

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

async fn publish(factory: &dyn CustomEndpointFactory, queue: &str, payloads: &[&str]) {
    let publisher = factory
        .create_publisher("test", &json!({ "queue": queue }))
        .await
        .expect("create publisher");
    let messages = payloads
        .iter()
        .map(|payload| CanonicalMessage::from(*payload))
        .collect();
    publisher.send_batch(messages).await.expect("send batch");
    publisher.flush().await.expect("flush");
}

/// Receives one non-empty batch, or panics after `timeout`.
async fn receive_one_batch(
    consumer: &mut dyn mq_bridge::traits::MessageConsumer,
    timeout: Duration,
) -> ReceivedBatch {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let batch = consumer.receive_batch(16).await.expect("receive batch");
        if !batch.messages.is_empty() {
            return batch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no message arrived within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Receives at least `expected` messages, or panics after `timeout`.
async fn receive_at_least(
    consumer: &mut dyn mq_bridge::traits::MessageConsumer,
    expected: usize,
    timeout: Duration,
) -> Vec<CanonicalMessage> {
    let deadline = std::time::Instant::now() + timeout;
    let mut messages = Vec::new();
    while messages.len() < expected {
        let batch = consumer.receive_batch(16).await.expect("receive batch");
        messages.extend(batch.messages);
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of {expected} messages arrived within {timeout:?}",
            messages.len()
        );
        if messages.len() < expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    messages
}

#[tokio::test(flavor = "multi_thread")]
async fn the_endpoint_conforms_when_linked_directly() {
    let report = conformance::run(
        &FixtureFactory,
        ConformanceOptions::new("direct", json!({ "queue": "conformance-direct" })),
    )
    .await
    .expect("direct-linked fixture should pass conformance");
    assert!(report.contains(&"round_trip"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_endpoint_conforms_when_loaded_as_a_plugin() {
    let factory = plugin_factory();
    let report = conformance::run(
        factory.as_ref(),
        ConformanceOptions::new("plugin", json!({ "queue": "conformance-plugin" })),
    )
    .await
    .expect("plugin-loaded fixture should pass the same conformance suite");

    // Same checks, same outcome: the ABI round trip changed no semantics.
    let direct = conformance::run(
        &FixtureFactory,
        ConformanceOptions::new("direct", json!({ "queue": "conformance-direct-2" })),
    )
    .await
    .unwrap();
    assert_eq!(report, direct);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_moves_messages_through_plugin_endpoints() {
    use mq_bridge::models::{Endpoint, EndpointType};
    use mq_bridge::route::Route;

    let factory = plugin_factory();
    publish(factory.as_ref(), "route-in", &["a", "b", "c"]).await;

    let endpoint = |queue: &str| {
        Endpoint::new(EndpointType::Custom {
            name: "fixture".to_string(),
            config: json!({ "queue": queue }),
        })
    };
    let route = Route::new(endpoint("route-in"), endpoint("route-out"));

    // The fixture never ends its stream, so the route runs until cancelled.
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        route.run_until_err("plugin_route", None, None),
    )
    .await;

    let mut consumer = factory
        .create_consumer("drain", &json!({ "queue": "route-out" }))
        .await
        .expect("create consumer");
    let messages = receive_at_least(&mut *consumer, 3, Duration::from_secs(5)).await;
    let mut payloads: Vec<String> = messages
        .iter()
        .map(|message| message.get_payload_str().to_string())
        .collect();
    payloads.sort();
    assert_eq!(payloads, vec!["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledgement_happens_only_when_the_batch_is_committed() {
    let factory = plugin_factory();
    publish(factory.as_ref(), "ack-timing", &["one"]).await;

    let mut consumer = factory
        .create_consumer("ack", &json!({ "queue": "ack-timing" }))
        .await
        .expect("create consumer");
    let batch = receive_one_batch(&mut *consumer, Duration::from_secs(5)).await;

    // The plugin records every commit in a side queue, so the host can observe
    // that receiving alone acknowledged nothing.
    let mut commits = factory
        .create_consumer("log", &json!({ "queue": "ack-timing#committed" }))
        .await
        .expect("create commit-log consumer");
    assert!(
        commits.receive_batch(8).await.unwrap().messages.is_empty(),
        "receiving a batch must not acknowledge it"
    );

    (batch.commit)(vec![MessageDisposition::Ack])
        .await
        .expect("commit");

    let logged = receive_one_batch(&mut *commits, Duration::from_secs(5)).await;
    assert_eq!(logged.messages.len(), 1);
    assert_eq!(logged.messages[0].get_payload_str(), "one");
    assert_eq!(
        logged.messages[0]
            .metadata
            .get("disposition")
            .map(String::as_str),
        Some("ack")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nacked_batch_is_redelivered_and_reported_as_nacked() {
    let factory = plugin_factory();
    publish(factory.as_ref(), "nack-timing", &["retry-me"]).await;

    let mut consumer = factory
        .create_consumer("nack", &json!({ "queue": "nack-timing" }))
        .await
        .expect("create consumer");
    let batch = receive_one_batch(&mut *consumer, Duration::from_secs(5)).await;
    (batch.commit)(vec![MessageDisposition::Nack])
        .await
        .expect("commit");

    let again = receive_one_batch(&mut *consumer, Duration::from_secs(5)).await;
    assert_eq!(again.messages[0].get_payload_str(), "retry-me");

    let mut commits = factory
        .create_consumer("log", &json!({ "queue": "nack-timing#committed" }))
        .await
        .expect("create commit-log consumer");
    let logged = receive_one_batch(&mut *commits, Duration::from_secs(5)).await;
    assert_eq!(
        logged.messages[0]
            .metadata
            .get("disposition")
            .map(String::as_str),
        Some("nack")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_dropped_without_committing_acknowledges_nothing() {
    let factory = plugin_factory();
    publish(factory.as_ref(), "dropped", &["not-acked"]).await;

    let mut consumer = factory
        .create_consumer("drop", &json!({ "queue": "dropped" }))
        .await
        .expect("create consumer");
    drop(receive_one_batch(&mut *consumer, Duration::from_secs(5)).await);

    let mut commits = factory
        .create_consumer("log", &json!({ "queue": "dropped#committed" }))
        .await
        .expect("create commit-log consumer");
    assert!(
        commits.receive_batch(8).await.unwrap().messages.is_empty(),
        "dropping a batch must not acknowledge it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn consumer_error_classes_survive_the_abi() {
    let factory = plugin_factory();
    let consumer_error = |fail: &'static str| {
        let factory = Arc::clone(&factory);
        async move {
            let mut consumer = factory
                .create_consumer(
                    "errors",
                    &json!({ "queue": "errors", "fail_receive": fail }),
                )
                .await
                .expect("create consumer");
            consumer
                .receive_batch(1)
                .await
                .expect_err("the fixture was asked to fail")
        }
    };

    assert!(matches!(
        consumer_error("retryable").await,
        ConsumerError::Connection(_)
    ));
    assert!(matches!(
        consumer_error("permanent").await,
        ConsumerError::Permanent(_)
    ));
    assert!(matches!(
        consumer_error("end_of_stream").await,
        ConsumerError::EndOfStream
    ));

    // The message from inside the plugin has to reach the host, or an operator
    // sees only "the plugin failed".
    let error = consumer_error("permanent").await;
    assert!(
        error.to_string().contains("fixture injected a permanent"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn publisher_error_classes_survive_the_abi() {
    let factory = plugin_factory();
    for (fail, expect_retryable) in [("retryable", true), ("permanent", false)] {
        let publisher = factory
            .create_publisher("errors", &json!({ "queue": "errors", "fail_send": fail }))
            .await
            .expect("create publisher");
        let error = publisher
            .send_batch(vec![CanonicalMessage::from("x")])
            .await
            .expect_err("the fixture was asked to fail");
        match (&error, expect_retryable) {
            (PublisherError::Retryable(_), true) | (PublisherError::NonRetryable(_), false) => {}
            _ => panic!("`fail_send: {fail}` produced the wrong error class: {error}"),
        }
    }
}

/// ABI 1.1. Before it, the host had no way to ask, so an order-sensitive sink
/// loaded as a plugin was silently published to in parallel whenever the route
/// ran with `concurrency > 1`.
#[tokio::test(flavor = "multi_thread")]
async fn publisher_ordering_requirement_survives_the_abi() {
    let factory = plugin_factory();
    for ordered in [true, false] {
        let publisher = factory
            .create_publisher(
                "ordering",
                &json!({ "queue": "ordering", "requires_ordered_publish": ordered }),
            )
            .await
            .expect("create publisher");
        assert_eq!(
            publisher.requires_ordered_publish(),
            ordered,
            "`requires_ordered_publish: {ordered}` did not survive the ABI"
        );
    }
}

/// The directly linked endpoint is the reference: the plugin-loaded one has to
/// give the same answer, or the ABI is reporting something the endpoint didn't
/// say.
#[tokio::test(flavor = "multi_thread")]
async fn ordering_is_reported_the_same_linked_and_loaded() {
    let config = json!({ "queue": "ordering-parity", "requires_ordered_publish": true });
    let direct = FixtureFactory
        .create_publisher("ordering-parity", &config)
        .await
        .expect("create the directly linked publisher");
    let loaded = plugin_factory()
        .create_publisher("ordering-parity", &config)
        .await
        .expect("create the plugin-loaded publisher");

    assert!(direct.requires_ordered_publish());
    assert_eq!(
        direct.requires_ordered_publish(),
        loaded.requires_ordered_publish()
    );
}

/// ABI 1.1. The point of the per-message outcome array: a batch that half
/// landed must come back as `Partial`, naming the half that did not. Under 1.0
/// the whole batch was reported as the first failure's class, so a retry
/// duplicated everything that had already been published.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_publish_survives_the_abi() {
    let factory = plugin_factory();
    let config = json!({ "queue": "partial", "fail_send_at": [1, 3] });
    let publisher = factory
        .create_publisher("partial", &config)
        .await
        .expect("create publisher");

    let payloads = ["a", "b", "c", "d", "e"];
    let sent = publisher
        .send_batch(payloads.iter().map(|p| CanonicalMessage::from(*p)).collect())
        .await
        .expect("a partial batch is a success, not an error");

    let SentBatch::Partial { failed, .. } = sent else {
        panic!("expected a partial batch, got {sent:?}");
    };
    // Pairing is positional, so the host must name exactly the messages it put
    // at indices 1 and 3 — not merely the right number of them.
    let names: Vec<String> = failed
        .iter()
        .map(|(message, _)| message.get_payload_str().into_owned())
        .collect();
    assert_eq!(names, ["b", "d"]);
    for (message, error) in &failed {
        assert!(
            matches!(error, PublisherError::Retryable(_)),
            "{} came back as {error}",
            message.get_payload_str()
        );
    }

    // And the other three really were published: the route acknowledges them on
    // the strength of this, so a lost message here is a silently dropped one.
    let mut consumer = factory
        .create_consumer("partial", &config)
        .await
        .expect("create consumer");
    let mut delivered: Vec<String> = receive_at_least(&mut *consumer, 3, Duration::from_secs(5))
        .await
        .iter()
        .map(|message| message.get_payload_str().into_owned())
        .collect();
    delivered.sort();
    assert_eq!(delivered, ["a", "c", "e"]);
}

/// The outcome byte carries the class, so a permanent per-message failure must
/// not arrive as a retryable one: the route would nack it forever.
#[tokio::test(flavor = "multi_thread")]
async fn partial_failure_classes_survive_the_abi() {
    let factory = plugin_factory();
    let publisher = factory
        .create_publisher(
            "partial-class",
            &json!({
                "queue": "partial-class",
                "fail_send_at": [0],
                "fail_send": "permanent",
            }),
        )
        .await
        .expect("create publisher");

    let sent = publisher
        .send_batch(vec![CanonicalMessage::from("x"), CanonicalMessage::from("y")])
        .await
        .expect("a partial batch is a success, not an error");
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

/// A batch where nothing landed stays a batch error rather than becoming a
/// `Partial` listing every message. That is what keeps `Connection` able to mean
/// "reconnect the endpoint", which no per-message byte can say.
#[tokio::test(flavor = "multi_thread")]
async fn a_wholly_failed_batch_is_still_an_error() {
    let publisher = plugin_factory()
        .create_publisher(
            "partial-none",
            &json!({ "queue": "partial-none", "fail_send_at": [0, 1] }),
        )
        .await
        .expect("create publisher");

    let error = publisher
        .send_batch(vec![CanonicalMessage::from("x"), CanonicalMessage::from("y")])
        .await
        .expect_err("every message failed, so the batch failed");
    assert!(matches!(error, PublisherError::Retryable(_)), "{error}");
    assert!(
        error.to_string().contains("2 of 2 messages failed"),
        "the plugin's own summary should reach the host: {error}"
    );
}

/// The directly linked endpoint is the reference for which messages failed too.
#[tokio::test(flavor = "multi_thread")]
async fn partial_publishes_agree_linked_and_loaded() {
    let config = json!({ "queue": "partial-parity", "fail_send_at": [2] });
    let payloads = ["p", "q", "r", "s"];

    async fn failed_payloads(
        factory: &dyn CustomEndpointFactory,
        config: &serde_json::Value,
        payloads: &[&str],
    ) -> Vec<String> {
        let publisher = factory
            .create_publisher("partial-parity", config)
            .await
            .expect("create publisher");
        let sent = publisher
            .send_batch(payloads.iter().map(|p| CanonicalMessage::from(*p)).collect())
            .await
            .expect("send batch");
        let SentBatch::Partial { failed, .. } = sent else {
            panic!("expected a partial batch, got {sent:?}");
        };
        failed
            .iter()
            .map(|(message, _)| message.get_payload_str().into_owned())
            .collect()
    }

    let direct = failed_payloads(&FixtureFactory, &config, &payloads).await;
    let loaded = failed_payloads(&*plugin_factory(), &config, &payloads).await;
    assert_eq!(direct, ["r"]);
    assert_eq!(direct, loaded);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panic_inside_the_plugin_becomes_an_error() {
    let factory = plugin_factory();
    let mut consumer = factory
        .create_consumer(
            "panic",
            &json!({ "queue": "panic", "panic_on_receive": true }),
        )
        .await
        .expect("create consumer");

    let error = consumer
        .receive_batch(1)
        .await
        .expect_err("a panicking plugin must not unwind into the host");
    assert!(matches!(error, ConsumerError::Permanent(_)), "{error}");
    assert!(error.to_string().contains("panicked"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_configuration_is_rejected_at_creation() {
    let factory = plugin_factory();
    let error = match factory
        .create_consumer("bad", &json!({ "queue": "x", "unknown_field": 1 }))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("unknown configuration fields must be rejected"),
    };
    assert!(
        format!("{error:#}").contains("invalid fixture endpoint configuration"),
        "{error:#}"
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

#[test]
fn a_second_library_claiming_the_same_endpoint_name_is_rejected() {
    let source = library("mq-bridge-plugin-fixture");
    load_endpoint_plugin(&source).expect("first load");

    // Same plugin, different file: the loader cannot dedupe by path, so the
    // name conflict must be caught instead of silently replacing the endpoint.
    let copy = source.with_file_name(format!(
        "copy_of_{}",
        source.file_name().unwrap().to_string_lossy()
    ));
    std::fs::copy(&source, &copy).expect("copy the plugin");

    // Remove the copy before asserting, so a failure does not leave it behind.
    let loaded = load_endpoint_plugin(&copy);
    let _ = std::fs::remove_file(&copy);
    let error = loaded.expect_err("a duplicate endpoint name must be rejected");
    let text = format!("{error:#}");
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

// ------------------------------------------------------------- middleware

/// The same library also exports a middleware under the name `fixture`, so
/// loading it registers both.
#[tokio::test(flavor = "multi_thread")]
async fn loading_a_plugin_registers_its_middleware_too() {
    let info = load_endpoint_plugin(library("mq-bridge-plugin-fixture")).unwrap();
    assert!(info.supports_middleware);
    assert!(
        mq_bridge::extensions::get_middleware_factory("fixture").is_some(),
        "a plugin with the middleware capability must register one"
    );
}

/// Builds a route whose input and output are plugin endpoints, with the plugin
/// middleware applied to `side`.
fn route_with_middleware(
    input: &str,
    output: &str,
    side: &str,
    middleware_config: serde_json::Value,
) -> mq_bridge::route::Route {
    use mq_bridge::models::{Endpoint, EndpointType, Middleware};

    let middleware = Middleware::Custom {
        name: "fixture".to_string(),
        config: middleware_config,
    };
    let endpoint = |queue: &str, with_middleware: bool| {
        let mut endpoint = Endpoint::new(EndpointType::Custom {
            name: "fixture".to_string(),
            config: json!({ "queue": queue }),
        });
        if with_middleware {
            endpoint.middlewares = vec![middleware.clone()];
        }
        endpoint
    };
    mq_bridge::route::Route::new(
        endpoint(input, side == "input"),
        endpoint(output, side == "output"),
    )
}

async fn run_route(route: mq_bridge::route::Route, name: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        route.run_until_err(name, None, None),
    )
    .await;
}

async fn drain(factory: &dyn CustomEndpointFactory, queue: &str, expected: usize) -> Vec<String> {
    let mut consumer = factory
        .create_consumer("drain", &json!({ "queue": queue }))
        .await
        .expect("create consumer");
    let messages = receive_at_least(&mut *consumer, expected, Duration::from_secs(5)).await;
    let mut payloads: Vec<String> = messages
        .iter()
        .map(|message| message.get_payload_str().to_string())
        .collect();
    payloads.sort();
    payloads
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_middleware_rewrites_and_drops_on_the_input_side() {
    let factory = plugin_factory();
    publish(
        factory.as_ref(),
        "mw-in",
        &["keep-one", "skip-two", "keep-three"],
    )
    .await;

    run_route(
        route_with_middleware(
            "mw-in",
            "mw-in-out",
            "input",
            json!({ "drop_prefix": "skip-", "suffix": "-seen" }),
        ),
        "mw_input",
    )
    .await;

    assert_eq!(
        drain(factory.as_ref(), "mw-in-out", 2).await,
        vec!["keep-one-seen", "keep-three-seen"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_middleware_rewrites_and_drops_on_the_output_side() {
    let factory = plugin_factory();
    publish(
        factory.as_ref(),
        "mw-out",
        &["keep-one", "skip-two", "keep-three"],
    )
    .await;

    run_route(
        route_with_middleware(
            "mw-out",
            "mw-out-out",
            "output",
            json!({ "drop_prefix": "skip-", "suffix": "-sent" }),
        ),
        "mw_output",
    )
    .await;

    assert_eq!(
        drain(factory.as_ref(), "mw-out-out", 2).await,
        vec!["keep-one-sent", "keep-three-sent"]
    );
}

/// A message the middleware drops must be acknowledged, or the source hands it
/// back forever.
#[tokio::test(flavor = "multi_thread")]
async fn messages_dropped_by_a_plugin_middleware_are_acknowledged() {
    let factory = plugin_factory();
    publish(factory.as_ref(), "mw-drop", &["skip-me", "keep-me"]).await;

    run_route(
        route_with_middleware(
            "mw-drop",
            "mw-drop-out",
            "input",
            json!({ "drop_prefix": "skip-" }),
        ),
        "mw_drop",
    )
    .await;

    assert_eq!(
        drain(factory.as_ref(), "mw-drop-out", 1).await,
        vec!["keep-me"]
    );

    let mut commits = factory
        .create_consumer("log", &json!({ "queue": "mw-drop#committed" }))
        .await
        .expect("create commit-log consumer");
    let logged = receive_one_batch(&mut *commits, Duration::from_secs(5)).await;
    let mut acked: Vec<String> = logged
        .messages
        .iter()
        .filter(|message| message.metadata.get("disposition").map(String::as_str) == Some("ack"))
        .map(|message| message.get_payload_str().to_string())
        .collect();
    acked.sort();
    assert_eq!(
        acked,
        vec!["keep-me", "skip-me"],
        "the dropped message must be acked on the source, not left for redelivery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_plugin_middleware_surfaces_its_message() {
    let factory = plugin_factory();
    let middleware = mq_bridge::extensions::get_middleware_factory("fixture")
        .expect("the fixture plugin registers a middleware");
    let consumer = factory
        .create_consumer("mw-fail", &json!({ "queue": "mw-fail" }))
        .await
        .expect("create consumer");
    let mut wrapped = middleware
        .apply_consumer(consumer, "mw-fail", &json!({ "fail": true }))
        .await
        .expect("apply middleware");

    publish(factory.as_ref(), "mw-fail", &["anything"]).await;
    let error = wrapped
        .receive_batch(4)
        .await
        .expect_err("the middleware was asked to fail");
    assert!(error.to_string().contains("configured to fail"), "{error}");
}

/// A cancelled route drops its endpoints while a blocking ABI call may still be
/// inside the plugin. Freeing the handle there is a use-after-free — it
/// segfaulted before the handles were refcounted, and only sometimes, so this
/// hammers the window.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_route_mid_call_does_not_free_a_handle_still_in_use() {
    let factory = plugin_factory();
    publish(factory.as_ref(), "cancel-in", &["a", "b", "c"]).await;

    for round in 0..25 {
        let route = route_with_middleware(
            "cancel-in",
            "cancel-out",
            "input",
            json!({ "drop_prefix": "skip-" }),
        );
        let name = format!("cancel_{round}");
        // Cancel while the consumer is almost certainly parked inside the
        // plugin: the fixture returns empty batches, so the route spins.
        let _ = tokio::time::timeout(
            Duration::from_millis(15),
            route.run_until_err(&name, None, None),
        )
        .await;
    }
}
