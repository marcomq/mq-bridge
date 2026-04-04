#![allow(dead_code)]
use std::sync::Arc;

use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, verify_subscriber_logic, PERF_TEST_MESSAGE_COUNT,
};
const CONFIG_YAML: &str = r#"
routes:
  memory_to_nats:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-nats" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream" }

  nats_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      nats: { url: "nats://localhost:4222", subject: "test-stream.pipeline", stream: "test-stream" }
    output:
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

pub async fn test_nats_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/nats.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("nats", &config_yaml, controller, "nats").await;
        },
    )
    .await;
}

pub async fn test_nats_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let subject = format!("sub_logic_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            subject: Some(subject),
            stream: Some("test-stream".to_string()),
            subscriber_mode: true,
            ..Default::default()
        };

        let publisher = Arc::new(NatsPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(NatsConsumer::new(&config).await.unwrap()));
        let sub2 = Arc::new(tokio::sync::Mutex::new(NatsConsumer::new(&config).await.unwrap()));

        verify_subscriber_logic(publisher, sub1, sub2).await;
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
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "NATS",
            || async {
                let mut pub_config = config.clone();
                pub_config.subject = Some(subject.to_string());
                pub_config.stream = Some(stream_name.to_string());
                Arc::new(NatsPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.subject = Some(subject.to_string());
                endpoint.stream = Some(stream_name.to_string());
                Arc::new(tokio::sync::Mutex::new(
                    NatsConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_nats_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/nats.yml",
        |controller| async move {
            let stream_name = "status_nats_direct";
            let subject = "status_nats_direct.subject";
            let config = mq_bridge::models::NatsConfig {
                url: "nats://localhost:4222".to_string(),
                ..Default::default()
            };

            let mut pub_config = config.clone();
            pub_config.subject = Some(subject.to_string());
            pub_config.stream = Some(stream_name.to_string());
            let publisher = NatsPublisher::new(&pub_config).await.unwrap();

            let mut consumer_config = config.clone();
            consumer_config.subject = Some(subject.to_string());
            consumer_config.stream = Some(stream_name.to_string());
            let consumer = NatsConsumer::new(&consumer_config).await.unwrap();

            println!("[NATS] Checking initial status...");
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
            println!("[NATS] Initial status check OK.");

            controller.stop_service("nats");
            println!("[NATS] Service 'nats' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                if !publisher.status().await.healthy && !consumer.status().await.healthy {
                    println!("[NATS] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!("[NATS] Timeout waiting for disconnect.");
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("nats");
            println!("[NATS] Service 'nats' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                if publisher.status().await.healthy && consumer.status().await.healthy {
                    println!("[NATS] Reconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!("[NATS] Timeout waiting for reconnect.");
                }
                sleep(Duration::from_secs(1)).await;
            }
            println!("[NATS] Status test successful.");
        },
    )
    .await;
}
