#![allow(dead_code)]
use super::common::{
    measure_read_performance, measure_write_performance, run_performance_pipeline_test,
    run_pipeline_test, run_test_with_docker, setup_logging, PERF_TEST_CONCURRENCY,
    PERF_TEST_MESSAGE_COUNT,
};
use hot_queue::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
use std::{sync::Arc, time::Duration};

const PERF_TEST_MESSAGE_COUNT_DIRECT: usize = 20_000;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_kafka:
    in:
      memory: { topic: "kafka-test-in" }
    out:
      kafka: { brokers: "localhost:9092", topic: "test_topic_kafka" }

  kafka_to_memory:
    in:
      kafka: { brokers: "localhost:9092", topic: "test_topic_kafka", group_id: "test_group" }
    out:
      memory: { topic: "kafka-test-out", capacity: {out_capacity} }
"#;

pub async fn test_kafka_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("kafka", &config_yaml).await;
    })
    .await;
}

pub async fn test_kafka_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("kafka", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_kafka_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let topic = "perf_test_kafka_direct";
        let config = hot_queue::models::KafkaConfig {
            brokers: "localhost:9092".to_string(),
            group_id: Some("perf_test_group_kafka".to_string()),
            producer_options: Some(vec![
                ("queue.buffering.max.ms".to_string(), "50".to_string()), // Linger for 50ms to batch messages
                ("acks".to_string(), "1".to_string()), // Wait for leader ack, a good balance
                ("compression.type".to_string(), "snappy".to_string()), // Use snappy compression
            ]),
            await_ack: false, // Use "fire-and-forget" for high throughput
            ..Default::default()
        };

        let publisher = Arc::new(KafkaPublisher::new(&config, topic).await.unwrap());
        measure_write_performance(
            "Kafka",
            publisher,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;

        tokio::time::sleep(Duration::from_secs(5)).await;

        let consumer = Arc::new(tokio::sync::Mutex::new(
            KafkaConsumer::new(&config, topic).unwrap(),
        ));
        measure_read_performance(
            "Kafka",
            consumer,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;
    })
    .await;
}
