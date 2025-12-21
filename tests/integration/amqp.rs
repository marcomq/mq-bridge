#![allow(dead_code)]

use super::common::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    run_test_with_docker, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_amqp:
    in:
      memory: { topic: "amqp-test-in" }
    out:
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp", delayed_ack: false  }

  amqp_to_memory:
    in:
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp", delayed_ack: false  }
    out:
      memory: { topic: "amqp-test-out", capacity: {out_capacity} }
"#;

pub async fn test_amqp_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("AMQP", &config_yaml).await;
    })
    .await;
}

pub async fn test_amqp_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("AMQP", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_amqp_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let queue = "perf_test_amqp_direct";
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            delayed_ack: false,
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "AMQP",
            || async { Arc::new(AmqpPublisher::new(&config, queue).await.unwrap()) },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    AmqpConsumer::new(&config, queue).await.unwrap(),
                ))
            },
        )
        .await;
        add_performance_result(result);
    })
    .await;
}
