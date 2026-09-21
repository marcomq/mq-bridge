#![allow(unused_imports, dead_code)]
use std::sync::Arc;

use mq_bridge::test_utils::PERF_TEST_MESSAGE_COUNT;

use mq_bridge::endpoints::mongodb::{
    MongoDbChangeStreamReader, MongoDbConsumer, MongoDbIdReader, MongoDbPublisher,
};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_performance_pipeline_test_at_least_once_named,
    run_performance_pipeline_test_named, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, should_run, verify_subscriber_logic,
    PerformanceResult,
};
// Queue pipeline: `consume: consumer` is explicit because the default (`capture_all`) needs a
// replica set and otherwise degrades to an `_id`-ordered reader, which cannot tail a collection
// written by 4 concurrent workers — a batch landing out of `_id` order is skipped for good.
const CONFIG_YAML: &str = r#"
routes:
  memory_to_mongodb:
    concurrency: 4
    batch_size: 1024
    input:
      memory: { topic: "test-in-mongodb" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      mongodb: { url: "mongodb://localhost:27017", database: "mq_bridge_test", collection: "test_collection" }

  mongodb_to_memory:
    concurrency: 4
    batch_size: 1024
    input:
      mongodb: { url: "mongodb://localhost:27017", database: "mq_bridge_test", collection: "test_collection", consume: consumer }
    output:
      memory: { topic: "test-out-mongodb", capacity: {out_capacity} }
"#;

/// `consume: snapshot` on a standalone mongod: seed the collection, then read it non-destructively
/// in one pass. Asserts the two properties that define the mode — every seeded document arrives,
/// and the route ends itself on drain instead of polling on as a (lossy) tail.
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_snapshot_reads_all_and_ends() {
    if !should_run("mongodb") {
        return;
    }
    use mq_bridge::models::{Endpoint, EndpointType, MongoConsume, MongoDbConfig, Route};
    use mq_bridge::traits::MessagePublisher;

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let collection = format!("snapshot_{}", fast_uuid_v7::gen_id());
        let config = MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: "mq_bridge_test".to_string(),
            collection: Some(collection.clone()),
            consume: Some(MongoConsume::Snapshot),
            ..Default::default()
        };

        let publisher = MongoDbPublisher::new(&config).await.unwrap();
        let seeded = 250usize;
        for i in 0..seeded {
            publisher
                .send(format!("snapshot-{}", i).as_str().into())
                .await
                .unwrap();
        }

        let route_name = format!("snapshot_route_{}", fast_uuid_v7::gen_id());
        let out = Endpoint::new_memory(&route_name, seeded + 100);
        let out_channel = out.channel().unwrap();
        let route = Route::new(Endpoint::new(EndpointType::MongoDb(config)), out);
        route.deploy(&route_name).await.unwrap();

        // Track the seeded payloads rather than a message count: the snapshot also emits the
        // publisher's internal `<collection>:sequencer` document, so counting would both overshoot
        // and race (250 messages can be 249 real ones plus the sequencer).
        let mut seen = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while seen.len() < seeded && std::time::Instant::now() < deadline {
            for msg in out_channel.drain_messages() {
                seen.insert(msg.get_payload_str().to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let missing: Vec<usize> = (0..seeded)
            .filter(|i| !seen.contains(&format!("snapshot-{}", i)))
            .collect();
        assert!(
            missing.is_empty(),
            "snapshot dropped {} of {} documents: {:?}",
            missing.len(),
            seeded,
            missing
        );

        // Drained -> EndOfStream -> the route ends itself. It stays in the registry (only `stop()`
        // removes it), so the terminal outcome is what proves it finished rather than tailed on.
        let ended = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while mq_bridge::route_outcome(&route_name).is_none() && std::time::Instant::now() < ended {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            mq_bridge::route_outcome(&route_name),
            Some(mq_bridge::RouteOutcome::Completed),
            "snapshot route must complete on drain instead of tailing"
        );
        mq_bridge::stop_route(&route_name).await;
    })
    .await;
}

pub async fn test_mongodb_standalone_mode_boundaries() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let base = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: "mq_bridge_test".to_string(),
            collection: Some(format!("mode_boundaries_{}", fast_uuid_v7::gen_id())),
            ..Default::default()
        };

        MongoDbIdReader::new(&base)
            .await
            .expect("snapshot must work on standalone MongoDB");
        assert!(
            MongoDbChangeStreamReader::new(&base, false).await.is_err(),
            "capture_new must reject standalone MongoDB"
        );
        assert!(
            MongoDbChangeStreamReader::new(&base, true).await.is_err(),
            "capture_all must reject standalone MongoDB"
        );
    })
    .await;
}

pub async fn test_mongodb_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("mongodb", &config_yaml).await;
    })
    .await;
}

pub async fn test_mongodb_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/mongodb.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("mongodb", &config_yaml, controller, "mongodb").await;
        },
    )
    .await;
}

// Queue mode (`consume: consumer`) is the standalone-safe reader that still round-trips the
// wrapped envelope, so the handler sees the `kind` metadata. It replaces the removed subscriber
// mode this test used to exercise.
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_consumer_no_duplicates() {
    if !should_run("mongodb") {
        return;
    }
    use mq_bridge::models::{Endpoint, Route};
    use mq_bridge::traits::MessagePublisher;
    use mq_bridge::type_handler::TypeHandler;
    use mq_bridge::Handled;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    setup_logging();
    let collection_name = "test_no_dupes_route";
    let db_name = "mq_bridge_test_dupes_route";
    let url = "mongodb://localhost:27017";

    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct TestMsg {
        id: u32,
        data: String,
    }

    run_test_with_docker(
        "tests/integration/docker-compose/mongodb.yml",
        || async move {
            // Clean setup
            let client = mongodb::Client::with_uri_str(url).await.unwrap();
            client
                .database(db_name)
                .collection::<mongodb::bson::Document>(collection_name)
                .drop()
                .await
                .ok();

            let input_config = mq_bridge::models::MongoDbConfig {
                url: url.to_string(),
                database: db_name.to_string(),
                collection: Some(collection_name.to_string()),
                consume: Some(mq_bridge::models::MongoConsume::Consumer),
                polling_interval_ms: Some(10),
                format: mq_bridge::models::MongoDbFormat::Json,
                ..Default::default()
            };
            let input = Endpoint::new(mq_bridge::models::EndpointType::MongoDb(
                input_config.clone(),
            ));

            // Setup Output Endpoint (Memory to verify)
            let output = Endpoint::new_memory("out_no_dupes", 20);

            let counter = Arc::new(AtomicUsize::new(0));
            let counter_clone = counter.clone();

            let type_handler = TypeHandler::new().add("test_msg", move |msg: TestMsg| {
                let counter = counter_clone.clone();
                async move {
                    assert!(msg.id == 1 || msg.id == 2);
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Handled::Ack)
                }
            });

            let route = Route::new(input, output).with_handler(type_handler);

            route.deploy("test_no_dupes_route").await.unwrap();

            let publisher = MongoDbPublisher::new(&input_config).await.unwrap();
            let msg1 = TestMsg {
                id: 1,
                data: "one".to_string(),
            };
            let msg2 = TestMsg {
                id: 2,
                data: "two".to_string(),
            };

            publisher
                .send(mq_bridge::msg!(&msg1, "test_msg"))
                .await
                .unwrap();
            publisher
                .send(mq_bridge::msg!(&msg2, "test_msg"))
                .await
                .unwrap();

            // Wait for processing
            let start = std::time::Instant::now();
            while counter.load(Ordering::SeqCst) < 2 {
                if start.elapsed() > std::time::Duration::from_secs(10) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            mq_bridge::stop_route("test_no_dupes_route").await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                2,
                "Should have processed exactly 2 messages"
            );
        },
    )
    .await;
}

pub async fn test_mongodb_replica_set_pipeline() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            let config_yaml = CONFIG_YAML
                .replace(
                    "mongodb://localhost:27017",
                    "mongodb://localhost:27018/?replicaSet=rs0",
                )
                .replace("memory_to_mongodb", "memory_to_mongodb_rs")
                .replace("mongodb_to_memory", "mongodb_rs_to_memory")
                .replace(
                    "{out_capacity}",
                    &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
                );
            run_pipeline_test("mongodb_rs", &config_yaml).await;
        },
    )
    .await;
}

pub async fn test_mongodb_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("mongodb", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

// CDC pipeline: the read side watches the collection via `$changeStream` (`consume: capture_new`),
// never deleting documents; the stream opens at deploy time (before writes) so no inserts are missed.
// Requires a replica set. The write side uses `format: raw` so the collection holds plain business
// documents, not the `{_id, payload, metadata}` envelope — the CDC reader emits documents verbatim,
// so raw storage is what lets payloads round-trip back to `message_num`.
const CDC_CONFIG_YAML: &str = r#"
routes:
  memory_to_mongodb_cdc:
    concurrency: 4
    batch_size: 1024
    input:
      memory: { topic: "test-in-mongodb-cdc" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      mongodb: { url: "mongodb://localhost:27018/?replicaSet=rs0", database: "mq_bridge_test", collection: "cdc_collection", format: raw }

  mongodb_cdc_to_memory:
    concurrency: 1
    batch_size: 1024
    input:
      mongodb: { url: "mongodb://localhost:27018/?replicaSet=rs0", database: "mq_bridge_test", collection: "cdc_collection", consume: capture_new }
    output:
      memory: { topic: "test-out-mongodb-cdc", capacity: {out_capacity} }
"#;

pub async fn test_mongodb_cdc_performance_pipeline() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            let config_yaml = CDC_CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            // A MongoDB change stream is an at-least-once source: it redelivers events on any
            // resume (getMore timeout / step-down / reconnect under load), so the collection's
            // 100k unique documents can surface as slightly more than 100k reads. Assert full
            // unique coverage (no loss) and tolerate those duplicate redeliveries.
            run_performance_pipeline_test_at_least_once_named(
                "mongodb_cdc",
                "mongodb_cdc",
                &config_yaml,
                PERF_TEST_MESSAGE_COUNT,
            )
            .await;
        },
    )
    .await;
}

// --- Isolated CDC (change-stream) read benchmarks -----------------------------------------------
// Like the Postgres CDC benches: the coupled pipeline test above is write-bound (insert + oplog +
// read timed together). Here the change-stream reader is opened first, the collection is seeded
// *untimed* (buffered in the oplog), and only the drain / per-change latency is timed — isolating
// the change-stream reader. Requires a replica set (mongodb-replica.yml).

const CDC_URL: &str = "mongodb://localhost:27018/?replicaSet=rs0";
const CDC_DB: &str = "mq_bridge_test";

fn cdc_reader_cfg(collection: &str) -> mq_bridge::models::MongoDbConfig {
    mq_bridge::models::MongoDbConfig {
        url: CDC_URL.to_string(),
        database: CDC_DB.to_string(),
        collection: Some(collection.to_string()),
        consume: Some(mq_bridge::models::MongoConsume::CaptureNew),
        format: mq_bridge::models::MongoDbFormat::Json,
        ..Default::default()
    }
}

/// Drain the change stream until `want` events are seen, acking each batch. Returns the count.
async fn drain_cdc(reader: &mut MongoDbChangeStreamReader, want: usize) -> usize {
    use mq_bridge::traits::{MessageConsumer, MessageDisposition};
    let mut got = 0usize;
    while got < want {
        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            reader.receive_batch(1024),
        )
        .await
        .expect("cdc drain timed out")
        .expect("receive_batch failed");
        let n = batch.messages.len();
        (batch.commit)(vec![MessageDisposition::Ack; n])
            .await
            .expect("commit (ack) failed");
        got += n;
    }
    got
}

pub async fn test_mongodb_capture_all_exits_on_empty() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            use mq_bridge::traits::{MessageConsumer, MessageDisposition};

            let collection = format!("capture_all_drain_{}", fast_uuid_v7::gen_id());
            let client = mongodb::Client::with_uri_str(CDC_URL).await.unwrap();
            let coll = client
                .database(CDC_DB)
                .collection::<mongodb::bson::Document>(&collection);
            coll.insert_many([
                mongodb::bson::doc! { "id": 1 },
                mongodb::bson::doc! { "id": 2 },
            ])
            .await
            .expect("seed collection");

            let mut reader = MongoDbChangeStreamReader::new(&cdc_reader_cfg(&collection), true)
                .await
                .expect("create capture_all reader");
            reader.set_exit_on_empty(true);

            // Advance the snapshot high-water mark by one page, then insert while snapshot paging
            // is still active. The document may also be visible to a later snapshot query, but the
            // change-stream phase must deliver its insert event; duplicates are valid at-least-once.
            let snapshot = reader.receive_batch(1).await.expect("read snapshot page");
            assert_eq!(snapshot.messages.len(), 1);
            (snapshot.commit)(vec![MessageDisposition::Ack])
                .await
                .expect("commit snapshot");
            coll.insert_one(mongodb::bson::doc! { "id": 3, "written": "during-snapshot" })
                .await
                .expect("insert during snapshot");

            let mut saw_stream_insert = false;
            loop {
                let batch = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    reader.receive_batch(1),
                )
                .await
                .expect("capture_all drain did not finish after its idle timeout")
                .expect("receive capture_all batch");
                if batch.messages.is_empty() {
                    break;
                }
                saw_stream_insert |= batch.messages.iter().any(|message| {
                    message
                        .metadata
                        .get("mongodb.operation")
                        .map(String::as_str)
                        == Some("insert")
                        && serde_json::from_slice::<serde_json::Value>(&message.payload)
                            .ok()
                            .and_then(|body| body.get("id").and_then(|id| id.as_i64()))
                            == Some(3)
                });
                let count = batch.messages.len();
                (batch.commit)(vec![MessageDisposition::Ack; count])
                    .await
                    .expect("commit capture_all batch");
            }
            assert!(
                saw_stream_insert,
                "change-stream phase must deliver an insert committed during snapshot paging"
            );
        },
    )
    .await;
}

pub async fn test_mongodb_cdc_read_throughput() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            let collection = format!("cdc_readbench_{}", fast_uuid_v7::gen_id());
            // Open the change stream BEFORE seeding so nothing is missed.
            let mut reader = MongoDbChangeStreamReader::new(&cdc_reader_cfg(&collection), false)
                .await
                .expect("create CDC reader");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let client = mongodb::Client::with_uri_str(CDC_URL).await.unwrap();
            let coll = client
                .database(CDC_DB)
                .collection::<mongodb::bson::Document>(&collection);

            let n = PERF_TEST_MESSAGE_COUNT;
            let docs: Vec<mongodb::bson::Document> = (1..=n as i64)
                .map(|id| mongodb::bson::doc! { "id": id, "name": format!("name-{id}") })
                .collect();
            coll.insert_many(docs).await.expect("seed collection");

            let start = std::time::Instant::now();
            let seen = drain_cdc(&mut reader, n).await;
            let elapsed = start.elapsed();

            assert!(seen >= n, "captured {seen} < {n}");
            let rps = n as f64 / elapsed.as_secs_f64();
            println!(
                "mongodb_cdc read-only throughput: {rps:.0} rows/s ({n} rows in {:.2}s)",
                elapsed.as_secs_f64()
            );
            add_performance_result(PerformanceResult {
                test_name: "mongodb_cdc read-only".to_string(),
                read_performance: rps,
                ..Default::default()
            });
        },
    )
    .await;
}

pub async fn test_mongodb_cdc_latency() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            use std::time::Instant;
            let collection = format!("cdc_latency_{}", fast_uuid_v7::gen_id());
            let mut reader = MongoDbChangeStreamReader::new(&cdc_reader_cfg(&collection), false)
                .await
                .expect("create CDC reader");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let n = 500usize;

            // Drain in a task, timestamping each event's arrival by id.
            let drain = tokio::spawn(async move {
                use mq_bridge::traits::{MessageConsumer, MessageDisposition};
                let mut arrivals: std::collections::HashMap<i64, Instant> =
                    std::collections::HashMap::new();
                while arrivals.len() < n {
                    let batch = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        reader.receive_batch(1024),
                    )
                    .await
                    .expect("latency drain timed out")
                    .expect("receive_batch failed");
                    let now = Instant::now();
                    let count = batch.messages.len();
                    for msg in &batch.messages {
                        let body: serde_json::Value =
                            serde_json::from_slice(&msg.payload).unwrap_or_default();
                        if let Some(id) = body.get("id").and_then(|v| v.as_i64()) {
                            arrivals.entry(id).or_insert(now);
                        }
                    }
                    (batch.commit)(vec![MessageDisposition::Ack; count])
                        .await
                        .expect("commit (ack) failed");
                }
                arrivals
            });

            // Insert one document at a time, recording the write instant per id.
            let client = mongodb::Client::with_uri_str(CDC_URL).await.unwrap();
            let coll = client
                .database(CDC_DB)
                .collection::<mongodb::bson::Document>(&collection);
            let mut sent: std::collections::HashMap<i64, Instant> =
                std::collections::HashMap::new();
            for id in 1..=n as i64 {
                coll.insert_one(mongodb::bson::doc! { "id": id, "name": format!("name-{id}") })
                    .await
                    .expect("insert");
                sent.insert(id, Instant::now());
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }

            let arrivals = drain.await.expect("drain task panicked");
            let mut lat_ms: Vec<f64> = sent
                .iter()
                .filter_map(|(id, t0)| {
                    arrivals
                        .get(id)
                        .map(|t1| t1.saturating_duration_since(*t0).as_secs_f64() * 1000.0)
                })
                .collect();
            assert!(!lat_ms.is_empty(), "no latencies collected");
            lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pct = |p: f64| lat_ms[(((lat_ms.len() as f64) * p) as usize).min(lat_ms.len() - 1)];
            println!(
                "mongodb_cdc latency (n={}): p50={:.2}ms p95={:.2}ms p99={:.2}ms",
                lat_ms.len(),
                pct(0.50),
                pct(0.95),
                pct(0.99)
            );
        },
    )
    .await;
}

/// Regression: a change stream that sits idle past the idle resume-token refresh interval
/// (10s) must keep streaming. The idle branch used to poll with a cancellable `StreamExt::next`
/// and then read `resume_token()`, which panics unless the driver's stream state is `Idle` —
/// the cancelled poll left it non-`Idle`, aborting the process on every idle CDC route.
pub async fn test_mongodb_cdc_survives_idle_resume_refresh() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            let collection = format!("cdc_idle_{}", fast_uuid_v7::gen_id());
            let mut reader = MongoDbChangeStreamReader::new(&cdc_reader_cfg(&collection), false)
                .await
                .expect("create CDC reader");

            let client = mongodb::Client::with_uri_str(CDC_URL).await.unwrap();
            let coll = client
                .database(CDC_DB)
                .collection::<mongodb::bson::Document>(&collection);

            // Idle well past IDLE_RESUME_REFRESH so the refresh branch runs at least twice,
            // then write: the stream must still deliver the change.
            let writer = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(25)).await;
                coll.insert_one(mongodb::bson::doc! { "id": 1i64, "name": "after-idle" })
                    .await
                    .expect("insert after idle");
            });

            let seen = drain_cdc(&mut reader, 1).await;
            assert!(
                seen >= 1,
                "change after a long idle period must be delivered"
            );
            writer.await.expect("writer task panicked");
        },
    )
    .await;
}

async fn run_mongodb_direct_perf_test_impl(
    compose_file: &str,
    url: &str,
    database: &str,
    collection_name: &str,
    test_name: &str,
) {
    setup_logging();
    let url = url.to_string();
    let database = database.to_string();
    let collection_name = collection_name.to_string();
    let test_name = test_name.to_string();

    run_test_with_docker(compose_file, || async move {
        let config = mq_bridge::models::MongoDbConfig {
            url,
            database,
            ..Default::default()
        };

        // Drop collection before test
        let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
        client
            .database(&config.database)
            .collection::<mongodb::bson::Document>(&collection_name)
            .drop()
            .await
            .ok();

        let result = run_direct_perf_test(
            &test_name,
            || async {
                let mut pub_config = config.clone();
                pub_config.collection = Some(collection_name.clone());
                Arc::new(MongoDbPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.collection = Some(collection_name.clone());
                Arc::new(tokio::sync::Mutex::new(
                    MongoDbConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_mongodb_performance_direct() {
    run_mongodb_direct_perf_test_impl(
        "tests/integration/docker-compose/mongodb.yml",
        "mongodb://localhost:27017",
        "mq_bridge_test_db",
        "perf_mongodb_direct",
        "MongoDB",
    )
    .await;
}

pub async fn test_mongodb_replica_set_performance_direct() {
    run_mongodb_direct_perf_test_impl(
        "tests/integration/docker-compose/mongodb-replica.yml",
        "mongodb://localhost:27018/?replicaSet=rs0",
        "mq_bridge_test_db_rs",
        "perf_mongodb_rs_direct",
        "MongoDB RS",
    )
    .await;
}

pub async fn test_mongodb_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/mongodb.yml",
        |controller| async move {
            let collection_name = "status_mongodb";
            let db_name = "mq_bridge_test_status";
            let config = mq_bridge::models::MongoDbConfig {
                url: "mongodb://localhost:27017".to_string(),
                database: db_name.to_string(),
                collection: Some(collection_name.to_string()),
                ..Default::default()
            };

            let publisher = MongoDbPublisher::new(&config).await.unwrap();
            let consumer = MongoDbConsumer::new(&config).await.unwrap();

            println!("[MongoDB] Checking initial status...");
            sleep(Duration::from_secs(2)).await;
            let pub_status = publisher.status().await;
            let con_status = consumer.status().await;
            assert!(
                pub_status.healthy,
                "Publisher should be healthy initially. Status: {:?}",
                pub_status
            );
            assert!(
                con_status.healthy,
                "Consumer should be healthy initially. Status: {:?}",
                con_status
            );
            println!("[MongoDB] Initial status check OK.");

            controller.stop_service("mongodb");
            println!("[MongoDB] Service 'mongodb' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[MongoDB] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!(
                        "[MongoDB] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("mongodb");
            println!("[MongoDB] Service 'mongodb' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                if publisher.status().await.healthy && consumer.status().await.healthy {
                    println!("[MongoDB] Reconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!("[MongoDB] Timeout waiting for reconnect.");
                }
                sleep(Duration::from_secs(1)).await;
            }
            println!("[MongoDB] Status test successful.");
        },
    )
    .await;
}

/// Two instances of one route share a MongoDB dedup store while their sinks fail now and
/// then. A failing instance reconnects and its claims outlive it in the store; the peer must
/// wait them out rather than ack the redeliveries as duplicates. Every key lands once.
#[cfg(feature = "dedup")]
pub async fn test_mongodb_dedup_store_competing_instances() {
    use mq_bridge::models::{
        DeduplicationMiddleware, Endpoint, EndpointType, FaultMode, MemoryConfig, Middleware,
        RandomPanicMiddleware,
    };
    use mq_bridge::{CanonicalMessage, Route};
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        const MESSAGES: usize = 200;
        let run = fast_uuid_v7::gen_id();
        let (in_topic, out_topic) = (format!("mdg_in_{run}"), format!("mdg_out_{run}"));
        let store = format!("mongodb://localhost:27017/mq_bridge_test/dedup_{run}");

        let mut instances = Vec::new();
        for fail_on in [[5usize, 17, 41], [7, 23, 37]] {
            let input = Endpoint::new(EndpointType::Memory(
                MemoryConfig::new(&in_topic, Some(4 * MESSAGES)).with_enable_nack(true),
            ))
            .add_middleware(Middleware::Deduplication(DeduplicationMiddleware {
                store: Some(store.clone()),
                sled_path: None,
                ttl_seconds: 3600,
                key: Some("${payload:id}".to_string()),
                replay_response: false,
            }));
            let output = fail_on.iter().fold(
                Endpoint::new_memory(&out_topic, 4 * MESSAGES),
                |endpoint, &n| {
                    endpoint.add_middleware(Middleware::RandomPanic(RandomPanicMiddleware {
                        mode: FaultMode::Disconnect,
                        trigger_on_message: Some(n),
                        enabled: true,
                        ..Default::default()
                    }))
                },
            );
            let handle = Route::new(input, output)
                .with_concurrency(2)
                .with_batch_size(8)
                .with_fault_injection(true)
                .with_reconnect_interval_ms(50)
                .run("mongo_dedup_shared")
                .await
                .unwrap();
            instances.push(handle);
        }

        let input = Endpoint::new_memory(&in_topic, 4 * MESSAGES)
            .channel()
            .unwrap();
        for i in 0..MESSAGES {
            for copy in 0..2u128 {
                let body = format!(r#"{{"id":"k{i}"}}"#);
                input
                    .send_message(CanonicalMessage::new(
                        body.into_bytes(),
                        Some((i as u128) << 1 | copy),
                    ))
                    .await
                    .unwrap();
            }
        }

        let output = Endpoint::new_memory(&out_topic, 4 * MESSAGES)
            .channel()
            .unwrap();
        let mut seen = std::collections::HashMap::<String, usize>::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut settled_at = None;
        loop {
            for msg in output.drain_messages() {
                let body: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
                *seen
                    .entry(body["id"].as_str().unwrap().to_string())
                    .or_default() += 1;
            }
            if seen.len() == MESSAGES
                && settled_at
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed()
                    > std::time::Duration::from_secs(1)
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let missing: Vec<usize> = (0..MESSAGES)
                    .filter(|i| !seen.contains_key(&format!("k{i}")))
                    .collect();
                panic!("lost messages: missing keys {missing:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        for instance in instances {
            instance.stop().await;
        }
        let duplicated: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
        assert!(duplicated.is_empty(), "duplicated keys: {duplicated:?}");
    })
    .await;
}
