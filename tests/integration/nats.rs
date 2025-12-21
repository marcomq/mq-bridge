#![allow(dead_code)]
use std::sync::Arc;

use super::common::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    run_test_with_docker, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
const PERF_TEST_MESSAGE_COUNT_DIRECT: usize = 20_000;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_nats:
    in:
      memory: { topic: "test-in-nats" }
    out:
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream", delayed_ack: false }

  nats_to_memory:
    in:
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream", delayed_ack: false  }
    out:
      memory: { topic: "test-out-nats", capacity: {out_capacity} }
"#;

pub async fn test_nats_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        ); // Use a small capacity for non-perf test
        run_pipeline_test("nats", &config_yaml).await;
    })
    .await;
}

pub async fn test_nats_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("nats", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_nats_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let stream_name = "perf_nats_direct";
        let subject = "perf_nats_direct.subject";
        let config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            delayed_ack: false,
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "NATS",
            || async {
                Arc::new(
                    NatsPublisher::new(&config, stream_name, subject)
                        .await
                        .unwrap(),
                )
            },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    NatsConsumer::new(&config, stream_name, subject)
                        .await
                        .unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}
