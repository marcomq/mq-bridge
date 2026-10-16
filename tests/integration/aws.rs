#![allow(dead_code)]

use mq_bridge::endpoints::aws::{AwsConsumer, AwsPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test,
    run_test_with_docker, run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_aws:
    concurrency: 4
    batch_size: 10
    input:
      memory: { topic: "aws-test-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      aws:
        queue_url: "http://localhost:4566/000000000000/test-queue"
        region: "us-east-1"
        endpoint_url: "http://localhost:4566"
        access_key: "test"
        secret_key: "test"

  aws_to_memory:
    concurrency: 4
    batch_size: 10
    input:
      aws:
        queue_url: "http://localhost:4566/000000000000/test-queue"
        region: "us-east-1"
        endpoint_url: "http://localhost:4566"
        access_key: "test"
        secret_key: "test"
        wait_time_seconds: 1
    output:
      memory: { topic: "aws-test-out", capacity: {out_capacity} }
"#;

async fn ensure_queue_exists() {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url("http://localhost:4566")
        .credentials_provider(aws_sdk_sqs::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .load()
        .await;
    let client = aws_sdk_sqs::Client::new(&config);
    let _ = client.create_queue().queue_name("test-queue").send().await;
    // Purge to ensure clean state
    let queue_url = "http://localhost:4566/000000000000/test-queue";
    let _ = client.purge_queue().queue_url(queue_url).send().await;
}

pub async fn test_aws_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/aws.yml", || async {
        ensure_queue_exists().await;
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("aws", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_aws_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/aws.yml", || async {
        ensure_queue_exists().await;

        let config = mq_bridge::models::AwsConfig {
            queue_url: Some("http://localhost:4566/000000000000/test-queue".to_string()),
            topic_arn: None,
            region: Some("us-east-1".to_string()),
            endpoint_url: Some("http://localhost:4566".to_string()),
            access_key: Some("test".to_string()),
            secret_key: Some("test".to_string()),
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "AWS",
            || async { Arc::new(AwsPublisher::new(&config).await.unwrap()) },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    AwsConsumer::new(&config).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_aws_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/aws.yml",
        |controller| async move {
            ensure_queue_exists().await;
            let config = mq_bridge::models::AwsConfig {
                queue_url: Some("http://localhost:4566/000000000000/test-queue".to_string()),
                region: Some("us-east-1".to_string()),
                endpoint_url: Some("http://localhost:4566".to_string()),
                access_key: Some("test".to_string()),
                secret_key: Some("test".to_string()),
                ..Default::default()
            };

            let publisher = AwsPublisher::new(&config).await.unwrap();
            let consumer = AwsConsumer::new(&config).await.unwrap();

            println!("[AWS] Checking initial status...");
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
            println!("[AWS] Initial status check OK.");

            controller.stop_service("localstack");
            println!("[AWS] Service 'localstack' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[AWS] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!(
                        "[AWS] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("localstack");
            println!("[AWS] Service 'localstack' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                // LocalStack state is ephemeral. Re-create the queue after restart.
                // This also helps wait for the SQS service to become ready.
                ensure_queue_exists().await;

                let p_res = AwsPublisher::new(&config).await;
                let c_res = AwsConsumer::new(&config).await;

                if let (Ok(p), Ok(c)) = (&p_res, &c_res) {
                    if p.status().await.healthy && c.status().await.healthy {
                        println!("[AWS] Reconnect detected.");
                        break;
                    }
                }

                if start.elapsed() > Duration::from_secs(30) {
                    // Give more time for AWS SDK to reconnect
                    let status = match (p_res, c_res) {
                        (Ok(p), Ok(c)) => format!(
                            "Pub Status: {:?}, Con Status: {:?}",
                            p.status().await,
                            c.status().await
                        ),
                        (p, c) => format!(
                            "Pub Init Error: {:?}, Con Init Error: {:?}",
                            p.err(),
                            c.err()
                        ),
                    };
                    panic!(
                        "[AWS] Timeout waiting for reconnect. Last state: {}",
                        status
                    );
                }
                sleep(Duration::from_secs(2)).await;
            }
            println!("[AWS] Status test successful.");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_aws_large_binary_batch_round_trips() {
    if !mq_bridge::test_utils::should_run("aws") {
        return;
    }
    use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher, SentBatch};

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/aws.yml", || async {
        ensure_queue_exists().await;
        let config = mq_bridge::models::AwsConfig {
            queue_url: Some("http://localhost:4566/000000000000/test-queue".to_string()),
            region: Some("us-east-1".to_string()),
            endpoint_url: Some("http://localhost:4566".to_string()),
            access_key: Some("test".to_string()),
            secret_key: Some("test".to_string()),
            wait_time_seconds: Some(1),
            binary_payload_mode: true,
            ..Default::default()
        };
        // Ten bodies of 200 kB: together twice what one batch request may carry.
        let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let batch = (0..10)
            .map(|_| mq_bridge::CanonicalMessage::new(payload.clone(), None))
            .collect();
        let sent = AwsPublisher::new(&config)
            .await
            .unwrap()
            .send_batch(batch)
            .await
            .unwrap();
        assert!(matches!(sent, SentBatch::Ack), "the batch was not sent");

        let mut consumer = AwsConsumer::new(&config).await.unwrap();
        let mut received = 0;
        while received < 10 {
            let batch = consumer.receive_batch(10).await.unwrap();
            assert!(!batch.messages.is_empty(), "only {received} of 10 arrived");
            for message in &batch.messages {
                assert_eq!(&message.payload[..], &payload[..], "payload changed");
            }
            received += batch.messages.len();
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
    })
    .await;
}
