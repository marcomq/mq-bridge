#![allow(dead_code)]

use mq_bridge::endpoints::aws::{AwsConsumer, AwsPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
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

pub async fn test_aws_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/aws.yml", || async {
        ensure_queue_exists().await;
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("aws", &config_yaml).await;
    })
    .await;
}

pub async fn test_aws_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/aws.yml",
        |controller| async move {
            ensure_queue_exists().await;
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("aws", &config_yaml, controller, "localstack").await;
        },
    )
    .await;
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
