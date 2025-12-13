#![allow(dead_code)]
use super::common::{
    measure_read_performance, measure_write_performance, run_performance_pipeline_test,
    run_pipeline_test, run_test_with_docker, setup_logging, PERF_TEST_CONCURRENCY,
    PERF_TEST_MESSAGE_COUNT,
};
use hot_queue::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
use std::{sync::Arc, time::Duration};

const PERF_TEST_MESSAGE_COUNT_DIRECT: usize = 20_000;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_amqp:
    in:
      memory: { topic: "amqp-test-in" }
    out:
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp", await_ack: true  }

  amqp_to_memory:
    in:
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp", await_ack: true  }
    out:
      memory: { topic: "amqp-test-out", capacity: {out_capacity} }
"#;

pub async fn test_amqp_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("AMQP", &config_yaml).await;
    })
    .await;
}

pub async fn test_amqp_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("AMQP", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_amqp_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let queue = "perf_test_amqp_direct";
        let config = hot_queue::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            await_ack: false,
            ..Default::default()
        };

        let publisher = Arc::new(AmqpPublisher::new(&config, queue).await.unwrap());
        measure_write_performance(
            "AMQP",
            publisher,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;

        tokio::time::sleep(Duration::from_secs(10)).await;

        let consumer = Arc::new(tokio::sync::Mutex::new(
            AmqpConsumer::new(&config, queue).await.unwrap(),
        ));
        measure_read_performance(
            "AMQP",
            consumer,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;
    })
    .await;
}
