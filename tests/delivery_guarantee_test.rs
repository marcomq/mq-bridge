// Fault injection for the `deduplication` middleware: transient sink failures, producer
// duplicates and competing instances. No Docker.
#![cfg(feature = "dedup")]

use mq_bridge::models::{
    DeduplicationMiddleware, Endpoint, EndpointType, FaultMode, MemoryConfig, Middleware,
    RandomPanicMiddleware,
};
use mq_bridge::{CanonicalMessage, Route};
use std::collections::HashMap;
use std::time::Duration;

const MESSAGES: usize = 200;

fn input(topic: &str, store: &str) -> Endpoint {
    Endpoint::new(EndpointType::Memory(
        MemoryConfig::new(topic, Some(4 * MESSAGES)).with_enable_nack(true),
    ))
    .add_middleware(Middleware::Deduplication(DeduplicationMiddleware {
        store: Some(store.to_string()),
        sled_path: None,
        ttl_seconds: 3600,
        key: Some("${payload:id}".to_string()),
        replay_response: false,
    }))
}

/// A sink that fails transiently on the given call numbers. Each failure nacks its batch
/// and makes the route reconnect; the memory source then redelivers it.
fn flaky_output(topic: &str, fail_on: &[usize]) -> Endpoint {
    fail_on
        .iter()
        .fold(Endpoint::new_memory(topic, 4 * MESSAGES), |endpoint, &n| {
            endpoint.add_middleware(Middleware::RandomPanic(RandomPanicMiddleware {
                mode: FaultMode::Disconnect,
                trigger_on_message: Some(n),
                // `Default` leaves the fault disabled; only serde defaults it on.
                enabled: true,
                ..Default::default()
            }))
        })
}

/// Every key twice, as a producer that retried every send would deliver it.
async fn send_with_producer_duplicates(topic: &str) {
    let channel = Endpoint::new_memory(topic, 4 * MESSAGES).channel().unwrap();
    for i in 0..MESSAGES {
        for copy in 0..2u128 {
            let body = format!(r#"{{"id":"k{i}"}}"#);
            let id = (i as u128) << 1 | copy;
            channel
                .send_message(CanonicalMessage::new(body.into_bytes(), Some(id)))
                .await
                .unwrap();
        }
    }
}

/// Collects the sink until every key arrived, then a little longer to catch late duplicates.
async fn collect(topic: &str) -> HashMap<String, usize> {
    let channel = Endpoint::new_memory(topic, 4 * MESSAGES).channel().unwrap();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut settled_at = None;
    loop {
        for msg in channel.drain_messages() {
            let body: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
            *seen
                .entry(body["id"].as_str().unwrap().to_string())
                .or_default() += 1;
        }
        if seen.len() == MESSAGES {
            let at = *settled_at.get_or_insert_with(tokio::time::Instant::now);
            if at.elapsed() > Duration::from_secs(1) {
                return seen;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lost messages: only {} of {MESSAGES} keys arrived",
            seen.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_exactly_once(seen: &HashMap<String, usize>) {
    let duplicated: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert_eq!(seen.len(), MESSAGES, "every key must arrive");
    assert!(duplicated.is_empty(), "duplicated keys: {duplicated:?}");
}

/// Transient sink failures and producer duplicates on one instance: every key lands once.
/// The in-process nack path (claims surviving into the redelivery) is pinned by the unit
/// test `a_nacked_message_is_redelivered_immediately`; here a failure reconnects the route.
#[tokio::test(flavor = "multi_thread")]
async fn transient_failures_and_producer_duplicates_land_exactly_once() {
    for concurrency in [1, 4] {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("sled://{}", dir.path().join("dedup").display());
        let (in_topic, out_topic) = (
            format!("dg_in_{concurrency}"),
            format!("dg_out_{concurrency}"),
        );
        let route_name = format!("dg_route_{concurrency}");

        Route::new(
            input(&in_topic, &store),
            flaky_output(&out_topic, &[3, 11, 29, 47]),
        )
        .with_concurrency(concurrency)
        .with_batch_size(8)
        .with_fault_injection(true)
        .with_reconnect_interval_ms(50)
        .deploy(&route_name)
        .await
        .unwrap();

        send_with_producer_duplicates(&in_topic).await;
        let seen = collect(&out_topic).await;
        Route::stop(&route_name).await;
        assert_exactly_once(&seen);
    }
}

/// Two instances of one route compete for the same source and share a SQL dedup store, while
/// both sinks fail now and then; a failing instance reconnects and its claims outlive it in
/// the store. Before the fix the peer acked those redeliveries as duplicates and lost them.
/// They share the route name, as replicas do: the default table is named after it.
#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
async fn competing_instances_on_a_shared_store_land_exactly_once() {
    // `MQB_DEDUP_TEST_STORE` points the same test at a server (postgres://…, mysql://…).
    let dir = tempfile::tempdir().unwrap();
    let store = std::env::var("MQB_DEDUP_TEST_STORE").unwrap_or_else(|_| {
        let path = dir.path().join("dedup.db");
        std::fs::File::create(&path).unwrap();
        format!("sqlite://{}", path.display())
    });

    let mut instances = Vec::new();
    for fail_on in [[5, 17, 41], [7, 23, 37]] {
        let handle = Route::new(
            input("dg_shared_in", &store),
            flaky_output("dg_shared_out", &fail_on),
        )
        .with_concurrency(2)
        .with_batch_size(8)
        .with_fault_injection(true)
        .with_reconnect_interval_ms(50)
        .run(&format!("dg_shared_{}", std::process::id()))
        .await
        .unwrap();
        instances.push(handle);
    }

    send_with_producer_duplicates("dg_shared_in").await;
    let seen = collect("dg_shared_out").await;
    for instance in instances {
        instance.stop().await;
    }
    assert_exactly_once(&seen);
}
