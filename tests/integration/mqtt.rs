#![allow(dead_code)]

use super::common::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    run_test_with_docker, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use mq_bridge::endpoints::mqtt::{MqttConsumer, MqttPublisher};
use std::sync::Arc;
use uuid::Uuid;
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
        let topic = "perf_test_mqtt_direct";
        let config = mq_bridge::models::MqttConfig {
            url: "mqtt://localhost:1883".to_string(),
            // Increase the client's incoming message buffer to hold all messages from the test run.
            queue_capacity: Some(PERF_TEST_MESSAGE_COUNT * 2), // For batch and single
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "MQTT",
            || async {
                let publisher_id = format!("pub-{}", Uuid::new_v4().as_simple());
                Arc::new(
                    MqttPublisher::new(&config, topic, &publisher_id)
                        .await
                        .unwrap(),
                )
            },
            || async {
                let consumer_id = format!("sub-{}", Uuid::new_v4().as_simple());
                Arc::new(tokio::sync::Mutex::new(
                    MqttConsumer::new(&config, topic, &consumer_id)
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
