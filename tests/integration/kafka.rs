#![allow(dead_code)]

use super::common::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    run_test_with_docker, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use mq_bridge::CanonicalMessage;
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_kafka:
    input:
      memory: { topic: "kafka-test-in" }
    output:
      kafka: 
        url: "localhost:9092"
        topic: "test_topic_kafka"
        producer_options: 
            - ["queue.buffering.max.ms", "50"]
            - ["acks", "1"]
            - ["compression.type", "snappy"]

  kafka_to_memory:
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

pub async fn test_kafka_request_reply() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let request_topic = "test_req_rep_topic";
        let reply_topic = "test_req_rep_reply_topic";

        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            group_id: Some("req_rep_group".to_string()),
            producer_options: Some(vec![("acks".to_string(), "1".to_string())]),
            ..Default::default()
        };

        // 1. Client Publisher (sends requests) - Creates request_topic
        let client_publisher = KafkaPublisher::new(&config, request_topic)
            .await
            .expect("Failed to create client publisher");

        // 2. Ensure reply_topic exists
        let _ = KafkaPublisher::new(&config, reply_topic)
            .await
            .expect("Failed to create reply topic");

        // 3. Service Consumer (reads requests)
        let mut service_consumer = KafkaConsumer::new(&config, request_topic)
            .await
            .expect("Failed to create service consumer");

        // 4. Client Consumer (reads replies)
        let mut reply_config = config.clone();
        reply_config.group_id = Some("reply_group".to_string());
        let mut client_consumer = KafkaConsumer::new(&reply_config, reply_topic)
            .await
            .expect("Failed to create client consumer");

        // Give Kafka a moment to settle consumer groups
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // 4. Send Request
        let correlation_id = "cid-12345";
        let mut req_msg = CanonicalMessage::new(b"request".to_vec(), None);
        req_msg
            .metadata
            .insert("kafka_reply_topic".to_string(), reply_topic.to_string());
        req_msg.metadata.insert(
            "kafka_correlation_id".to_string(),
            correlation_id.to_string(),
        );

        client_publisher
            .send(req_msg)
            .await
            .expect("Failed to send request");

        // 5. Service receives request
        let received_req = service_consumer
            .receive()
            .await
            .expect("Failed to receive request");

        // 6. Service sends response via commit
        let resp_msg = CanonicalMessage::new(b"response".to_vec(), None);
        (received_req.commit)(Some(resp_msg)).await;

        // 7. Client receives reply
        let received_resp = client_consumer
            .receive()
            .await
            .expect("Failed to receive response");

        assert_eq!(received_resp.message.payload, b"response".as_slice());
        assert_eq!(
            received_resp
                .message
                .metadata
                .get("kafka_correlation_id")
                .map(|s| s.as_str()),
            Some(correlation_id)
        );

        println!("Kafka Request-Reply test passed!");
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
