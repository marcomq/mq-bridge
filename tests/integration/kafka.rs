#![allow(dead_code)]

use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_kafka:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "kafka-test-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      kafka: 
        url: "localhost:9092"
        topic: "test_topic_kafka"
        producer_options: 
            - ["queue.buffering.max.ms", "50"]
            - ["acks", "1"]
            - ["compression.type", "snappy"]

  kafka_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      kafka:
        url: "localhost:9092"
        topic: "test_topic_kafka"
        group_id: "test_group"
    output:
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

pub async fn test_kafka_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/kafka.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("kafka", &config_yaml, controller, "kafka").await;
        },
    )
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
        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            group_id: Some("perf_test_group_kafka".to_string()),
            producer_options: Some(vec![
                ("queue.buffering.max.ms".to_string(), "50".to_string()), // Linger for 50ms to batch messages
                ("acks".to_string(), "1".to_string()), // Wait for leader ack, a good balance
                ("compression.type".to_string(), "snappy".to_string()), // Use snappy compression
            ]),
            delayed_ack: false, // Use "fire-and-forget" for high throughput
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "Kafka",
            || async { Arc::new(KafkaPublisher::new(&config, topic).await.unwrap()) },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    KafkaConsumer::new(&config, topic).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}
