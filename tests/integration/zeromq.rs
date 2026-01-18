#![allow(dead_code)]

use mq_bridge::test_utils::{
    run_performance_pipeline_test, setup_logging, PERF_TEST_MESSAGE_COUNT,
};

const CONFIG_YAML: &str = r#"
routes:
  memory_to_zeromq:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-zeromq" }
    output:
      zeromq:
        url: "tcp://127.0.0.1:5558"
        socket_type: "push"
        bind: true

  zeromq_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      zeromq:
        url: "tcp://127.0.0.1:5558"
        socket_type: "pull"
        bind: false
    output:
      memory: { topic: "test-out-zeromq", capacity: {out_capacity} }
"#;

pub async fn test_zeromq_performance_pipeline() {
    setup_logging();
    let config_yaml = CONFIG_YAML.replace(
        "{out_capacity}",
        &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
    );
    run_performance_pipeline_test("zeromq", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
}
