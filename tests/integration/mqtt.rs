#![allow(dead_code)]

use mq_bridge::endpoints::mqtt::{MqttConsumer, MqttPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, verify_subscriber_logic, PERF_TEST_BATCH_MESSAGE_COUNT,
};
use std::sync::Arc;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_mqtt:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-mqtt", enable_nack: true }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      mqtt: { url: "mqtt://localhost:1883", topic: "test_topic_mqtt", client_id: "test-publisher-chaos", clean_session: false, qos: 1, max_inflight: 500, queue_capacity: 1000, delayed_ack: false }

  mqtt_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      mqtt: { url: "mqtt://localhost:1883", topic: "test_topic_mqtt", client_id: "test-consumer-chaos", clean_session: false, qos: 1, max_inflight: 500, queue_capacity: 1000 }
    output:
      memory: { topic: "test-out-mqtt", capacity: {out_capacity} }
"#;

pub async fn test_mqtt_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_BATCH_MESSAGE_COUNT + 1000).to_string(),
        ); // Use a small capacity for non-perf test
        run_pipeline_test("mqtt", &config_yaml).await;
    })
    .await;
}

pub async fn test_mqtt_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/mqtt.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_BATCH_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("mqtt", &config_yaml, controller, "mosquitto").await;
        },
    )
    .await;
}

pub async fn test_mqtt_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let topic = format!("sub_logic_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::MqttConfig {
            url: "mqtt://localhost:1883".to_string(),
            topic: Some(topic),
            clean_session: true,
            ..Default::default()
        };

        let publisher = Arc::new(MqttPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(MqttConsumer::new(&config).await.unwrap()));
        let sub2 = Arc::new(tokio::sync::Mutex::new(MqttConsumer::new(&config).await.unwrap()));

        verify_subscriber_logic(publisher, sub1, sub2).await;
    })
    .await;
}

pub async fn test_mqtt_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_BATCH_MESSAGE_COUNT + 1000).to_string(),
        ); // Use a small capacity for non-perf test
        run_performance_pipeline_test("mqtt", &config_yaml, PERF_TEST_BATCH_MESSAGE_COUNT).await;
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
            queue_capacity: Some(PERF_TEST_BATCH_MESSAGE_COUNT * 2), // For batch and single
            topic: Some(topic.to_string()),
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "MQTT",
            || async {
                let publisher_id = format!("pub-{}", fast_uuid_v7::gen_id());
                let mut pub_config = config.clone();
                pub_config.client_id = Some(publisher_id);
                Arc::new(MqttPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let consumer_id = format!("sub-{}", fast_uuid_v7::gen_id());
                let mut consumer_config = config.clone();
                consumer_config.client_id = Some(consumer_id);
                Arc::new(tokio::sync::Mutex::new(
                    MqttConsumer::new(&consumer_config).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_mqtt_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/mqtt.yml",
        |controller| async move {
            let topic = "status_test_mqtt";
            let config = mq_bridge::models::MqttConfig {
                url: "mqtt://localhost:1883".to_string(),
                topic: Some(topic.to_string()),
                ..Default::default()
            };

            let publisher_id = format!("pub-status-{}", fast_uuid_v7::gen_id());
            let mut pub_config = config.clone();
            pub_config.client_id = Some(publisher_id);
            let publisher = MqttPublisher::new(&pub_config).await.unwrap();

            let consumer_id = format!("sub-status-{}", fast_uuid_v7::gen_id());
            let mut consumer_config = config.clone();
            consumer_config.client_id = Some(consumer_id);
            let consumer = MqttConsumer::new(&consumer_config).await.unwrap();

            println!("[MQTT] Checking initial status...");
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
            println!("[MQTT] Initial status check OK.");

            controller.stop_service("mosquitto");
            println!("[MQTT] Service 'mosquitto' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[MQTT] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!(
                        "[MQTT] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("mosquitto");
            println!("[MQTT] Service 'mosquitto' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                if publisher.status().await.healthy && consumer.status().await.healthy {
                    println!("[MQTT] Reconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!("[MQTT] Timeout waiting for reconnect.");
                }
                sleep(Duration::from_secs(1)).await;
            }
            println!("[MQTT] Status test successful.");
        },
    )
    .await;
}
