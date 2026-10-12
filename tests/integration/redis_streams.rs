#![allow(dead_code)]
use std::sync::Arc;

use mq_bridge::endpoints::redis_streams::{RedisStreamsConsumer, RedisStreamsPublisher};
use mq_bridge::test_utils::{
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, verify_subscriber_logic,
    PERF_TEST_MESSAGE_COUNT,
};

const COMPOSE: &str = "tests/integration/docker-compose/redis.yml";
const URL: &str = "redis://localhost:6379";

const CONFIG_YAML: &str = r#"
routes:
  memory_to_redis_streams:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-redis" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      redis_streams: { url: "redis://localhost:6379", stream: "test-redis-pipeline" }

  redis_streams_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      redis_streams: { url: "redis://localhost:6379", stream: "test-redis-pipeline", group: "itest-pipeline", reader_connections: 4 }
    output:
      memory: { topic: "test-out-redis", capacity: {out_capacity} }
"#;

pub async fn test_redis_pipeline() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("redis_streams", &config_yaml).await;
    })
    .await;
}

pub async fn test_redis_subscriber_logic() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let stream = format!("sub_logic_redis_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some(stream),
            subscriber_mode: true,
            ..Default::default()
        };

        let publisher = Arc::new(RedisStreamsPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(
            RedisStreamsConsumer::new(&config).await.unwrap(),
        ));
        let sub2 = Arc::new(tokio::sync::Mutex::new(
            RedisStreamsConsumer::new(&config).await.unwrap(),
        ));
        // Give the subscriber read tasks time to issue their first blocking XREAD
        // from "$" so they don't miss the messages published below.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        verify_subscriber_logic(publisher, sub1, sub2).await;
    })
    .await;
}

pub async fn test_redis_performance_pipeline() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("redis_streams", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_redis_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(COMPOSE, |controller| async move {
        let config = mq_bridge::models::RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some("status_redis".to_string()),
            group: Some("itest-status".to_string()),
            ..Default::default()
        };

        let publisher = RedisStreamsPublisher::new(&config).await.unwrap();
        let consumer = RedisStreamsConsumer::new(&config).await.unwrap();

        println!("[Redis] Checking initial status...");
        sleep(Duration::from_secs(2)).await;
        assert!(
            publisher.status().await.healthy,
            "Publisher should be healthy initially"
        );
        assert!(
            consumer.status().await.healthy,
            "Consumer should be healthy initially"
        );
        println!("[Redis] Initial status check OK.");

        controller.stop_service("redis");
        println!("[Redis] Service 'redis' stopped. Waiting for disconnect detection...");
        let start = std::time::Instant::now();
        loop {
            if !publisher.status().await.healthy && !consumer.status().await.healthy {
                println!("[Redis] Disconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[Redis] Timeout waiting for disconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }

        controller.start_service("redis");
        println!("[Redis] Service 'redis' started. Waiting for reconnect...");
        let start = std::time::Instant::now();
        loop {
            if publisher.status().await.healthy && consumer.status().await.healthy {
                println!("[Redis] Reconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[Redis] Timeout waiting for reconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }
        println!("[Redis] Status test successful.");
    })
    .await;
}

// Issue 1 (regression): reading FROM a Redis stream with `exit_on_empty` must drain
// every existing entry and then terminate. Before the fix the consumer blocked on the
// first `recv` forever, so `--drain` never fired and a broker batch job hung.
pub async fn test_redis_drain_exits_on_empty() {
    use mq_bridge::models::{Endpoint, EndpointType, RedisStreamsConfig};
    use mq_bridge::route::RouteOutcome;
    use mq_bridge::traits::{MessageDisposition, MessagePublisher};
    use mq_bridge::{CanonicalMessage, Route};

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        const N: usize = 300;
        let stream = format!("drain_redis_{}", fast_uuid_v7::gen_id());

        // Publish N entries.
        let pub_config = RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some(stream.clone()),
            ..Default::default()
        };
        let publisher = RedisStreamsPublisher::new(&pub_config).await.unwrap();
        let batch: Vec<_> = (0..N)
            .map(|i| CanonicalMessage::new(format!("msg-{i}").into_bytes(), None))
            .collect();
        publisher.send_batch(batch).await.unwrap();

        // Read them with a group from the start of the stream, draining to memory.
        let in_config = RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some(stream.clone()),
            group: Some(format!("g_{}", fast_uuid_v7::gen_id())),
            read_from_start: true,
            ..Default::default()
        };
        let out_topic = format!("drain_redis_out_{}", fast_uuid_v7::gen_id());
        let input = Endpoint::new(EndpointType::RedisStreams(in_config));
        let output = Endpoint::new_memory(&out_topic, N + 100);
        let route = Route::new(input, output)
            .with_batch_size(128)
            .with_exit_on_empty(true);

        let mut verifier = route
            .connect_to_output("redis_drain_verifier")
            .await
            .unwrap();
        let collector = tokio::spawn(async move {
            let mut received = 0usize;
            while received < N {
                let item =
                    tokio::time::timeout(std::time::Duration::from_secs(15), verifier.receive())
                        .await
                        .expect("timed out draining output")
                        .expect("output stream closed early");
                received += 1;
                (item.commit)(MessageDisposition::Ack).await.unwrap();
            }
            received
        });

        let handle = route.run("redis_drain_test").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("redis route did not exit on empty after draining");
        assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));
        handle.join().await.expect("route task panicked");

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), collector)
            .await
            .expect("timed out collecting output")
            .expect("collector task panicked");
        assert_eq!(received, N);
        println!("[Redis] Drain exit_on_empty test successful.");
    })
    .await;
}

/// A replacement consumer must reclaim a backlog left by dead consumers even when the
/// `>` side of the stream is already fully delivered (`lag: 0`).
///
/// Regression for a permanent wedge: the reclaiming reader issued `XAUTOCLAIM` on the same
/// multiplexed connection it then parked in `XREAD BLOCK`. With nothing new arriving the
/// read really blocks, so the claim reply could time out client-side *after* the server had
/// applied it — the entries moved into the new consumer's own PEL and were never delivered,
/// and every retry re-claimed them and reset their idle time. The route stayed `healthy`.
///
/// The shape matters: two dead consumers holding the backlog, a drained `>` side, several
/// reader connections, `batch_size` larger than the backlog, and `block_ms` equal to
/// `redelivery_timeout_ms` so the reclaim fires on every blocked read.
pub async fn test_redis_reclaims_backlog_with_drained_stream() {
    use mq_bridge::models::{Endpoint, EndpointType, RedisStreamsConfig};
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use mq_bridge::{CanonicalMessage, Route};

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        const N: usize = 200;
        let stream = format!("reclaim_redis_{}", fast_uuid_v7::gen_id());
        let group = format!("g_reclaim_{}", fast_uuid_v7::gen_id());

        let base = |consumer_name: Option<String>| RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some(stream.clone()),
            group: Some(group.clone()),
            read_from_start: true,
            consumer_name,
            redelivery_timeout_ms: Some(1000),
            block_ms: Some(1000),
            ..Default::default()
        };

        RedisStreamsPublisher::new(&base(None))
            .await
            .unwrap()
            .send_batch(
                (0..N)
                    .map(|i| CanonicalMessage::new(format!("msg-{i}").into_bytes(), None))
                    .collect(),
            )
            .await
            .unwrap();

        // Two consumers read the whole stream and are dropped without acking: the backlog
        // ends up pending under two now-dead names and `>` has nothing left to deliver.
        // Counted as distinct payloads: the second dead consumer may reclaim entries that
        // are already pending under `dead-0`, so deliveries overcount the backlog.
        let mut drained_ids = std::collections::HashSet::new();
        for i in 0..2 {
            let mut dead = RedisStreamsConsumer::new(&base(Some(format!("dead-{i}"))))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut received = 0usize;
            while drained_ids.len() < N * (i + 1) / 2 && std::time::Instant::now() < deadline {
                let batch =
                    tokio::time::timeout(std::time::Duration::from_secs(5), dead.receive_batch(16))
                        .await;
                match batch {
                    Ok(Ok(b)) => {
                        received += b.messages.len();
                        drained_ids.extend(b.messages.iter().map(|m| m.payload.clone()));
                    }
                    Ok(Err(e)) => {
                        eprintln!("dead-{i}: receive_batch failed: {e:?}");
                        break;
                    }
                    Err(e) => {
                        eprintln!("dead-{i}: receive_batch timed out: {e}");
                        break;
                    }
                }
            }
            assert!(received > 0, "dead-{i} received no messages");
            drop(dead);
        }
        let drained = drained_ids.len();
        assert!(drained > 0, "seeding left no pending entries");

        // The replacement: batch_size well above the backlog, several reader connections.
        let out_topic = format!("reclaim_out_{}", fast_uuid_v7::gen_id());
        let output = Endpoint::new_memory(&out_topic, N + 100);
        let mut in_config = base(None);
        in_config.reader_connections = Some(4);
        let route = Route::new(
            Endpoint::new(EndpointType::RedisStreams(in_config)),
            output.clone(),
        )
        .with_batch_size(512)
        .with_concurrency(4);

        let mut sink = output.create_consumer("reclaim_sink").await.unwrap();
        let handle = route.run("redis_reclaim_test").await.unwrap();

        let mut received_ids = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while received_ids.len() < drained {
            let received = received_ids.len();
            let batch =
                tokio::time::timeout(std::time::Duration::from_secs(20), sink.receive_batch(512))
                    .await
                    .unwrap_or_else(|_| {
                        panic!("reclaim wedged: {received}/{drained} entries redelivered")
                    })
                    .unwrap();
            received_ids.extend(batch.messages.iter().map(|m| m.payload.clone()));
            (batch.commit)(vec![
                mq_bridge::traits::MessageDisposition::Ack;
                batch.messages.len()
            ])
            .await
            .unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "reclaim wedged: {}/{drained} entries redelivered",
                received_ids.len()
            );
        }
        handle.stop().await;
        println!(
            "[Redis] Reclaim-with-drained-stream test successful ({} entries).",
            received_ids.len()
        );
    })
    .await;
}

/// A partial batch must be delivered immediately, not withheld until `batch_size` entries
/// have accumulated. `receive_batch` blocks for the *first* entry, then drains whatever else
/// is already pending — so two entries answer a `receive_batch(1024)` at once.
///
/// Reported as a bug against v0.3.8 (a route stalling at the app's default batch_size of
/// 1024); it did not reproduce at either level, so these pin the behaviour instead.
pub async fn test_redis_partial_batch_is_delivered() {
    use mq_bridge::models::{Endpoint, EndpointType, RedisStreamsConfig};
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use mq_bridge::{CanonicalMessage, Route};

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let config = |suffix: &str| RedisStreamsConfig {
            url: URL.to_string(),
            stream: Some(format!("partial_{suffix}_{}", fast_uuid_v7::gen_id())),
            group: Some(format!("g_partial_{suffix}")),
            read_from_start: true,
            ..Default::default()
        };
        let two = || {
            vec![
                CanonicalMessage::new(b"v1".to_vec(), None),
                CanonicalMessage::new(b"v2".to_vec(), None),
            ]
        };

        // Consumer level. Entries are published *before* the group exists, matching the report.
        let cfg = config("consumer");
        RedisStreamsPublisher::new(&cfg)
            .await
            .unwrap()
            .send_batch(two())
            .await
            .unwrap();
        let mut consumer = RedisStreamsConsumer::new(&cfg).await.unwrap();
        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            consumer.receive_batch(1024),
        )
        .await
        .expect("receive_batch never returned: a partial batch was withheld")
        .unwrap();
        assert_eq!(
            batch.messages.len(),
            2,
            "both pending entries must arrive in one under-full batch"
        );

        // Route level, at the app's default batch_size.
        let cfg = config("route");
        RedisStreamsPublisher::new(&cfg)
            .await
            .unwrap()
            .send_batch(two())
            .await
            .unwrap();
        let out_topic = format!("partial_out_{}", fast_uuid_v7::gen_id());
        let output = Endpoint::new_memory(&out_topic, 100);
        let route = Route::new(
            Endpoint::new(EndpointType::RedisStreams(cfg)),
            output.clone(),
        )
        .with_batch_size(1024);
        let handle = route.run("redis_partial_batch").await.unwrap();

        let mut sink = output.create_consumer("partial_sink").await.unwrap();
        let batch =
            tokio::time::timeout(std::time::Duration::from_secs(10), sink.receive_batch(1024))
                .await
                .expect("nothing reached the sink: the route withheld a partial batch")
                .unwrap();
        assert!(!batch.messages.is_empty());
        handle.stop().await;
    })
    .await;
}

fn idle_config(name: &str) -> mq_bridge::models::RedisStreamsConfig {
    mq_bridge::models::RedisStreamsConfig {
        url: URL.to_string(),
        stream: Some(format!("{name}_{}", fast_uuid_v7::gen_id())),
        group: Some(format!("g_{name}")),
        ..Default::default()
    }
}

/// Reads until `expected` messages arrived or `within` passed; returns the count.
async fn receive_within(
    consumer: &mut RedisStreamsConsumer,
    expected: usize,
    within: std::time::Duration,
) -> usize {
    use mq_bridge::traits::{MessageConsumer, MessageDisposition};
    let deadline = tokio::time::Instant::now() + within;
    let mut received = 0;
    while received < expected {
        let Ok(batch) = tokio::time::timeout_at(deadline, consumer.receive_batch(64)).await else {
            break;
        };
        let batch = batch.unwrap();
        received += batch.messages.len();
        (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
            .await
            .unwrap();
    }
    received
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_message_on_an_idle_stream_arrives_at_once() {
    if !mq_bridge::test_utils::should_run("redis_streams") {
        return;
    }
    use mq_bridge::traits::MessagePublisher;
    use std::time::Duration;

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let config = idle_config("idle");
        let mut consumer = RedisStreamsConsumer::new(&config).await.unwrap();
        let publisher = RedisStreamsPublisher::new(&config).await.unwrap();
        // Long enough for several client-side response timeouts of the blocking read.
        tokio::time::sleep(Duration::from_secs(3)).await;
        for i in 0..5 {
            publisher
                .send(mq_bridge::CanonicalMessage::new(vec![i], None))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        let received = receive_within(&mut consumer, 5, Duration::from_secs(8)).await;
        assert_eq!(received, 5, "messages on an idle stream are delayed");
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_dropped_consumer_takes_no_entries() {
    if !mq_bridge::test_utils::should_run("redis_streams") {
        return;
    }
    use mq_bridge::traits::MessagePublisher;
    use std::time::Duration;

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let config = idle_config("dropped");
        // What a route does after a connection error: drop the consumer, build another.
        for _ in 0..3 {
            drop(RedisStreamsConsumer::new(&config).await.unwrap());
        }
        let mut consumer = RedisStreamsConsumer::new(&config).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let publisher = RedisStreamsPublisher::new(&config).await.unwrap();
        for i in 0..20 {
            publisher
                .send(mq_bridge::CanonicalMessage::new(vec![i], None))
                .await
                .unwrap();
        }
        let received = receive_within(&mut consumer, 20, Duration::from_secs(8)).await;
        assert_eq!(received, 20, "a dropped consumer's reader took entries");
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_entry_in_flight_is_not_delivered_twice() {
    if !mq_bridge::test_utils::should_run("redis_streams") {
        return;
    }
    use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher};
    use std::time::Duration;

    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let mut config = idle_config("inflight");
        config.read_from_start = true;
        config.redelivery_timeout_ms = Some(1000);
        config.block_ms = Some(1000);
        RedisStreamsPublisher::new(&config)
            .await
            .unwrap()
            .send(mq_bridge::CanonicalMessage::new(b"slow".to_vec(), None))
            .await
            .unwrap();
        let mut consumer = RedisStreamsConsumer::new(&config).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(10), consumer.receive_batch(8))
            .await
            .expect("the message arrives")
            .unwrap();
        assert_eq!(first.messages.len(), 1);

        // A sink slower than `redelivery_timeout_ms` still holds the entry.
        let again = tokio::time::timeout(Duration::from_secs(4), consumer.receive_batch(8)).await;
        assert!(
            again.is_err(),
            "an entry still in flight was delivered again"
        );

        // A nack hands it back to the reclaim pass.
        (first.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();
        let received = receive_within(&mut consumer, 1, Duration::from_secs(8)).await;
        assert_eq!(received, 1, "a nacked entry is delivered again");
    })
    .await;
}
