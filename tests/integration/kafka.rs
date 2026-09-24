#![allow(dead_code)]

use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, verify_subscriber_logic,
    PERF_TEST_MESSAGE_COUNT,
};
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_kafka:
    concurrency: 2
    batch_size: 1024
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
            - ["queue.buffering.max.ms", "1"]
            - ["acks", "1"]
            - ["compression.type", "snappy"]

  kafka_to_memory:
    concurrency: 2
    batch_size: 1024
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

#[cfg(feature = "perf-diagnostics")]
pub async fn test_kafka_consume_only_bench() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let s = fast_uuid_v7::gen_id();
        let cap = PERF_TEST_MESSAGE_COUNT + 1000;
        let cfg = format!(
            r#"
routes:
  memory_to_kafka:
    concurrency: 8
    batch_size: 128
    input:
      memory: {{ topic: "co-in-{s}", capacity: {cap} }}
    output:
      kafka:
        url: "localhost:9092"
        topic: "co-topic-{s}"
        partitions: 4
        delayed_ack: true
        producer_options:
          - ["queue.buffering.max.ms", "5"]
          - ["acks", "1"]
          - ["compression.type", "snappy"]
  kafka_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      kafka:
        url: "localhost:9092"
        topic: "co-topic-{s}"
        group_id: "co-grp-{s}"
        consumer_options:
          - ["auto.offset.reset", "earliest"]
    output:
      memory: {{ topic: "co-out-{s}", capacity: {cap} }}
"#,
            s = s,
            cap = cap
        );
        mq_bridge::test_utils::run_consume_only_bench("kafka", &cfg, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

#[cfg(feature = "perf-diagnostics")]
pub async fn test_kafka_produce_only_bench() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let s = fast_uuid_v7::gen_id();
        let cap = PERF_TEST_MESSAGE_COUNT + 1000;
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let concurrency = env("PO_CONCURRENCY", "4");
        let batch_size = env("PO_BATCH", "128");
        let linger = env("PO_LINGER_MS", "1");
        let batch_num_messages = env("PO_BATCH_NUM_MESSAGES", "10000"); // librdkafka default
        println!(
            "--- PRODUCE-ONLY params: concurrency={concurrency} batch_size={batch_size} linger_ms={linger} batch.num.messages={batch_num_messages}"
        );
        // Mirrors the committed coupled produce route (CONFIG_YAML memory_to_kafka):
        // retry middleware, delayed_ack default (false = await delivery).
        let cfg = format!(
            r#"
routes:
  memory_to_kafka:
    concurrency: {concurrency}
    batch_size: {batch_size}
    input:
      memory: {{ topic: "po-in-{s}", capacity: {cap} }}
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      kafka:
        url: "localhost:9092"
        topic: "po-topic-{s}"
        producer_options:
          - ["queue.buffering.max.ms", "{linger}"]
          - ["batch.num.messages", "{batch_num_messages}"]
          - ["acks", "1"]
          - ["compression.type", "snappy"]
"#,
            s = s,
            cap = cap
        );
        mq_bridge::test_utils::run_produce_only_bench("kafka", &cfg, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_kafka_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let topic = format!("sub_logic_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            topic: Some(topic),
            ..Default::default()
        };

        let publisher = Arc::new(KafkaPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(
            KafkaConsumer::new(
                &config
                    .clone()
                    .with_consumer_option("auto.offset.reset", "earliest"),
            )
            .await
            .unwrap(),
        ));
        let sub2 = Arc::new(tokio::sync::Mutex::new(
            KafkaConsumer::new(
                &config
                    .clone()
                    .with_consumer_option("auto.offset.reset", "earliest"),
            )
            .await
            .unwrap(),
        ));

        verify_subscriber_logic(publisher, sub1, sub2).await;
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
                ("queue.buffering.max.ms".to_string(), "1".to_string()), // Small linger; send_batch already enqueues a burst
                ("acks".to_string(), "1".to_string()), // Wait for leader ack, a good balance
                ("compression.type".to_string(), "snappy".to_string()), // Use snappy compression
            ]),
            delayed_ack: false, // Use "fire-and-forget" for high throughput
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "Kafka",
            || async {
                let mut pub_config = config.clone();
                pub_config.topic = Some(topic.to_string());
                Arc::new(KafkaPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.topic = Some(topic.to_string());
                Arc::new(tokio::sync::Mutex::new(
                    KafkaConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_kafka_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/kafka.yml",
        |controller| async move {
            let topic = "status_test_kafka";
            let config = mq_bridge::models::KafkaConfig {
                url: "localhost:9092".to_string(),
                group_id: Some("status_test_group".to_string()),
                topic: Some(topic.to_string()),
                ..Default::default()
            };

            let publisher = KafkaPublisher::new(&config).await.unwrap();
            let consumer = KafkaConsumer::new(&config).await.unwrap();

            println!("[Kafka] Checking initial status...");
            sleep(Duration::from_secs(5)).await; // Give time to connect
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
            println!("[Kafka] Initial status check OK.");

            controller.stop_service("kafka");
            println!("[Kafka] Service 'kafka' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[Kafka] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(30) {
                    panic!(
                        "[Kafka] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("kafka");
            println!("[Kafka] Service 'kafka' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if pub_status.healthy && con_status.healthy {
                    println!("[Kafka] Reconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(30) {
                    panic!(
                        "[Kafka] Timeout waiting for reconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }
            println!("[Kafka] Status test successful.");
        },
    )
    .await;
}

/// A nacked batch must be redelivered. Kafka commits are cumulative, so before the fix the
/// next batch's ack committed past the nacked records and they were never read again.
pub async fn test_kafka_nack_replays_from_committed_offset() {
    use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher};
    use mq_bridge::CanonicalMessage;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let topic = format!("nack_replay_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            topic: Some(topic.clone()),
            group_id: Some(format!("group_{topic}")),
            ..Default::default()
        }
        .with_consumer_option("auto.offset.reset", "earliest");

        let publisher = KafkaPublisher::new(&config).await.unwrap();
        let sent: Vec<CanonicalMessage> = (0..10)
            .map(|i| CanonicalMessage::from(format!("m{i}")))
            .collect();
        publisher.send_batch(sent).await.unwrap();

        let mut first = KafkaConsumer::new(&config).await.unwrap();
        let batch =
            tokio::time::timeout(std::time::Duration::from_secs(60), first.receive_batch(3))
                .await
                .expect("initial batch timed out")
                .unwrap();
        let nacked: Vec<String> = batch
            .messages
            .iter()
            .map(|m| m.get_payload_str().to_string())
            .collect();
        assert!(!nacked.is_empty());
        (batch.commit)(vec![MessageDisposition::Nack; nacked.len()])
            .await
            .unwrap();
        assert!(
            first.receive_batch(10).await.is_err(),
            "a consumer with a nacked batch must stop, so nothing commits past it"
        );
        drop(first);

        let mut second = KafkaConsumer::new(&config).await.unwrap();
        let mut replayed = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !nacked.iter().all(|n| replayed.contains(n)) {
                let batch = second.receive_batch(10).await.unwrap();
                replayed.extend(
                    batch
                        .messages
                        .iter()
                        .map(|m| m.get_payload_str().to_string()),
                );
                (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                    .await
                    .unwrap();
            }
        })
        .await
        .expect("the nacked records must be redelivered from the last committed offset");
    })
    .await;
}
