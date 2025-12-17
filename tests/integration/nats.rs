#![allow(dead_code)]
use std::{sync::Arc, time::Duration};

use crate::integration::common::PERF_TEST_CONCURRENCY;

use super::common::{
    add_performance_result, measure_read_performance, measure_write_performance,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker, setup_logging,
};
use hot_queue::endpoints::nats::{NatsConsumer, NatsPublisher};
const PERF_TEST_MESSAGE_COUNT_DIRECT: usize = 20_000;
const PERF_TEST_MESSAGE_COUNT: usize = 50_000;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_nats:
    in:
      memory: { topic: "test-in-nats" }
    out:
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream", await_ack: true }

  nats_to_memory:
    in:
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream", await_ack: true  }
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
        let stream_name = "perf_stream_nats_direct";
        let subject = format!("{}.direct", stream_name);
        let config = hot_queue::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            skip_ack: false,
            ..Default::default()
        };

        let publisher = Arc::new(
            NatsPublisher::new(&config, &subject, Some(stream_name))
                .await
                .unwrap(),
        );
        let write_perf = measure_write_performance(
            "NATS",
            publisher,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;

        tokio::time::sleep(Duration::from_secs(3)).await;

        let consumer = Arc::new(tokio::sync::Mutex::new(
            NatsConsumer::new(&config, stream_name, &subject)
                .await
                .unwrap(),
        ));
        let read_perf =
            measure_read_performance("NATS", consumer, PERF_TEST_MESSAGE_COUNT_DIRECT).await;

        add_performance_result(super::common::PerformanceResult {
            test_name: "NATS Direct".to_string(),
            write_performance: write_perf,
            read_performance: read_perf,
        });
    })
    .await;
}
