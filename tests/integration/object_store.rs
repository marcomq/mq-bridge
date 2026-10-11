#![allow(dead_code)]
//! Integration tests for the `object_store` endpoint against LocalStack S3.
//!
//! `object_store` reads its backend config (creds/endpoint/region) from the process
//! environment, so each test sets the LocalStack S3 vars before building endpoints.

use super::assert_permanent_consumer_error;
use mq_bridge::endpoints::object_store::{ObjectStoreConsumer, ObjectStorePublisher};
use mq_bridge::models::{FileFormat, ObjectStoreConfig};
use mq_bridge::test_utils::{run_pipeline_test, run_test_with_docker, setup_logging};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use mq_bridge::CanonicalMessage;

const BUCKET: &str = "mqb-object-store-test";
const ENDPOINT: &str = "http://localhost:4566";

/// Points the `object_store` crate at LocalStack S3 over plain HTTP.
fn set_s3_env() {
    // SAFETY: test-only; every caller writes the same values.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        std::env::set_var("AWS_REGION", "us-east-1");
        std::env::set_var("AWS_ENDPOINT", ENDPOINT);
        std::env::set_var("AWS_ALLOW_HTTP", "true");
    }
}

/// Creates the test bucket (path-style PUT). LocalStack accepts the unsigned request and
/// the call is idempotent, so re-running against a warm container is fine.
async fn ensure_bucket() {
    let client = reqwest::Client::new();
    client
        .put(format!("{ENDPOINT}/{BUCKET}"))
        .send()
        .await
        .expect("create bucket request")
        .error_for_status()
        .expect("create bucket response");
}

fn config(prefix: &str, checkpoint_store: Option<String>) -> ObjectStoreConfig {
    ObjectStoreConfig {
        url: format!("s3://{BUCKET}/{prefix}"),
        format: FileFormat::Normal,
        cursor_id: checkpoint_store.as_ref().map(|_| "resume-test".to_string()),
        checkpoint_store,
        polling_interval_ms: Some(100),
        ..Default::default()
    }
}

fn json_msg(n: i64) -> CanonicalMessage {
    CanonicalMessage::new(
        serde_json::to_vec(&serde_json::json!({ "n": n })).unwrap(),
        None,
    )
}

const CONFIG_YAML: &str = r#"
routes:
  memory_to_object_store:
    concurrency: 4
    batch_size: 64
    input:
      memory: { topic: "obj-test-in" }
    output:
      object_store:
        url: "s3://mqb-object-store-test/{prefix}"
        format: normal
  object_store_to_memory:
    concurrency: 1
    batch_size: 64
    input:
      object_store:
        url: "s3://mqb-object-store-test/{prefix}"
        format: normal
        polling_interval_ms: 100
    output:
      memory: { topic: "obj-test-out", capacity: {out_capacity} }
"#;

/// End-to-end pipeline: memory -> object_store (write objects) -> object_store (read) -> memory.
pub async fn test_object_store_pipeline() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/object_store.yml",
        || async {
            set_s3_env();
            ensure_bucket().await;
            // Unique prefix per run so a reused container can't leak objects between tests.
            let prefix = format!("pipeline/{}", fast_uuid_v7::gen_id_str());
            let config_yaml = CONFIG_YAML
                .replace("{prefix}", &prefix)
                .replace("{out_capacity}", "10000");
            run_pipeline_test("object_store", &config_yaml).await;
        },
    )
    .await;
}

/// Durable resume: after acking the first object, a fresh consumer built with the same
/// checkpoint store must not re-read it.
pub async fn test_object_store_resume() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/object_store.yml",
        || async {
            set_s3_env();
            ensure_bucket().await;

            let prefix = format!("resume/{}", fast_uuid_v7::gen_id_str());
            let ckpt_dir =
                std::env::temp_dir().join(format!("mqb_obj_ckpt_{}", fast_uuid_v7::gen_id_str()));
            let ckpt = format!("file://{}/cursor.json", ckpt_dir.display());

            // Write three single-record objects.
            let publisher = ObjectStorePublisher::new(&config(&prefix, None))
                .await
                .unwrap();
            for n in 0..3 {
                publisher.send_batch(vec![json_msg(n)]).await.unwrap();
            }

            // First consumer: read + ack exactly the first object, then drop.
            {
                let mut consumer = ObjectStoreConsumer::new(&config(&prefix, Some(ckpt.clone())))
                    .await
                    .unwrap();
                let batch = wait_for_batch(&mut consumer).await;
                assert_eq!(batch.messages.len(), 1, "one record per object");
                let first_payload = batch.messages[0].payload.clone();
                (batch.commit)(vec![mq_bridge::traits::MessageDisposition::Ack; 1])
                    .await
                    .unwrap();

                // A restarted consumer with the same checkpoint resumes past the acked object.
                let mut resumed = ObjectStoreConsumer::new(&config(&prefix, Some(ckpt.clone())))
                    .await
                    .unwrap();
                let next = wait_for_batch(&mut resumed).await;
                assert_eq!(next.messages.len(), 1);
                assert_ne!(
                    next.messages[0].payload, first_payload,
                    "resumed consumer must not re-read the acked object"
                );
            }

            tokio::fs::remove_dir_all(&ckpt_dir).await.ok();
        },
    )
    .await;
}

/// Polls `receive_batch` until a non-empty batch arrives (the source returns empty batches
/// while idle / between objects).
async fn wait_for_batch(consumer: &mut ObjectStoreConsumer) -> mq_bridge::traits::ReceivedBatch {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let batch = consumer.receive_batch(64).await.unwrap();
        if !batch.messages.is_empty() {
            return batch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a non-empty batch"
        );
    }
}

/// An `s3://` `checkpoint_store` must resolve static credentials and a custom endpoint from
/// the environment. Bare `object_store::parse_url` reads no env at all and falls through to
/// the EC2 metadata service, which made cloud checkpoints unusable against LocalStack/R2.
pub async fn test_object_store_checkpoint_round_trip() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/object_store.yml",
        || async {
            set_s3_env();
            ensure_bucket().await;

            let cursor_id = format!("cp-{}", fast_uuid_v7::gen_id());
            let spec = format!("s3://{BUCKET}/cursors");
            let backend = mq_bridge::checkpoint::parse_checkpoint_store(&spec)
                .expect("parse s3 checkpoint_store");

            let store = mq_bridge::checkpoint::build_external_store(
                backend.clone(),
                "object_store",
                &cursor_id,
            )
            .await
            .expect("build s3 checkpoint store (creds must come from env)");
            assert_eq!(store.load().await.unwrap(), None, "fresh cursor is empty");
            store.save("42").await.expect("save cursor");
            assert_eq!(store.load().await.unwrap(), Some("42".to_string()));
            store.save("99").await.expect("overwrite cursor");

            // A freshly built store for the same cursor sees the persisted value.
            let reopened =
                mq_bridge::checkpoint::build_external_store(backend, "object_store", &cursor_id)
                    .await
                    .expect("rebuild s3 checkpoint store");
            assert_eq!(reopened.load().await.unwrap(), Some("99".to_string()));
        },
    )
    .await;
}

/// Both permanent object_store misconfigurations must terminate the route as `Failed`
/// instead of spinning on the reconnect interval forever:
///
/// * an object larger than `max_object_bytes` — it will never shrink, so every retry
///   re-lists it and emits the same warning (this is what made `--drain` hang at zero rows);
/// * a `checkpoint_store` overlapping the source prefix — a config error, so rebuilding the
///   consumer reproduces it exactly.
pub async fn test_object_store_permanent_errors_fail_fast() {
    use mq_bridge::models::{Endpoint, EndpointType};
    use mq_bridge::Route;

    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/object_store.yml",
        || async {
            set_s3_env();
            ensure_bucket().await;

            // Seed one object that is comfortably over the limit set below.
            let prefix = format!("permanent/{}", fast_uuid_v7::gen_id_str());
            let publisher = ObjectStorePublisher::new(&config(&prefix, None))
                .await
                .expect("create publisher");
            publisher
                .send_batch((1..=20).map(json_msg).collect::<Vec<_>>())
                .await
                .expect("seed objects");
            drop(publisher);

            let cases: Vec<(&str, ObjectStoreConfig)> = vec![
                (
                    "max_object_bytes",
                    ObjectStoreConfig {
                        max_object_bytes: Some(4),
                        ..config(
                            &prefix,
                            Some("file:///tmp/mqb-permanent-cursor.json".into()),
                        )
                    },
                ),
                (
                    "checkpoint overlaps source prefix",
                    ObjectStoreConfig {
                        cursor_id: Some("overlap-test".to_string()),
                        checkpoint_store: Some(format!("s3://{BUCKET}/{prefix}/cursor")),
                        ..config(&prefix, None)
                    },
                ),
            ];

            for (label, cfg) in cases {
                let route_name = format!("permanent_{}", fast_uuid_v7::gen_id_str());
                let input = Endpoint::new(EndpointType::ObjectStore(cfg));
                let output = Endpoint::new_memory(&route_name, 1024);
                let route = Route::new(input, output);
                assert_permanent_consumer_error(route, &route_name, label).await;
            }
        },
    )
    .await;
}
