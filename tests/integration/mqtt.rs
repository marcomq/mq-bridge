#![allow(dead_code)]
use super::common::{
    measure_read_performance, measure_write_performance, run_performance_pipeline_test,
    run_pipeline_test, run_test_with_docker, setup_logging, PERF_TEST_CONCURRENCY,
    PERF_TEST_MESSAGE_COUNT,
};
use hot_queue::endpoints::mqtt::{MqttConsumer, MqttPublisher};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const PERF_TEST_MESSAGE_COUNT_DIRECT: usize = 20_000;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_mqtt:
    in:
      memory: { topic: "test-in-mqtt" }
    out:
      mqtt: { url: "mqtt://localhost:1883", topic: "test_topic_mqtt" }

  mqtt_to_memory:
    in:
      mqtt: { url: "mqtt://localhost:1883", topic: "test_topic_mqtt" }
    out:
      memory: { topic: "test-out-mqtt", capacity: {out_capacity} }
"#;

pub async fn test_mqtt_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        ); // Use a small capacity for non-perf test
        run_pipeline_test("mqtt", &config_yaml).await;
    })
    .await;
}

pub async fn test_mqtt_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        ); // Use a small capacity for non-perf test
        run_performance_pipeline_test("mqtt", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_mqtt_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let unique_id = Uuid::new_v4().as_simple().to_string();
        let topic = "test_topic_mqtt/direct";
        let publisher_id = format!("perftest-pub-{}", unique_id);
        let consumer_id = format!("perftest-sub-{}", unique_id);
        let config = hot_queue::models::MqttConfig {
            url: "mqtt://localhost:1883".to_string(),
            // Increase the client's incoming message buffer to hold all messages from the test run.
            queue_capacity: Some(PERF_TEST_MESSAGE_COUNT_DIRECT),
            ..Default::default()
        };

        // Create the consumer and subscribe before publishing messages.
        let consumer = Arc::new(tokio::sync::Mutex::new(
            MqttConsumer::new(&config, topic, &consumer_id)
                .await
                .unwrap(),
        ));

        // Give the consumer a moment to connect and subscribe.
        // This helps ensure the subscription is active before messages are published.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let publisher = Arc::new(
            MqttPublisher::new(&config, topic, &publisher_id)
                .await
                .unwrap(),
        );
        measure_write_performance(
            "MQTT",
            publisher.clone(),
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;

        // Explicitly disconnect the publisher to release the client ID and connection gracefully.
        // It's possible the publisher is already disconnected due to errors during the
        // performance test, so we don't unwrap the result.
        // let _ = publisher.disconnect().await;

        // Give the broker a moment to process the backlog of messages before the consumer connects.
        // This helps prevent the broker from overwhelming the new consumer and dropping the connection.
        tokio::time::sleep(Duration::from_secs(3)).await;

        measure_read_performance(
            "MQTT",
            consumer,
            PERF_TEST_MESSAGE_COUNT_DIRECT,
            PERF_TEST_CONCURRENCY,
        )
        .await;
    })
    .await;
}
