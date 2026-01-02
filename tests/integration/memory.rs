#![allow(dead_code)]

use super::common::{run_performance_pipeline_test, setup_logging};
const PERF_TEST_MESSAGE_COUNT: usize = 1_250_000;
const PERF_TEST_CONCURRENCY: usize = 1;
const CONFIG_YAML: &str = r#"
metrics:
  enabled: false

routes:
  memory_to_internal:
    input:
      memory: { topic: "test-in-internal" }
    output:
      memory: { topic: "test-inntermediate-memory", capacity: {out_capacity} }

  internal_to_memory:
    input:
      memory: { topic: "test-inntermediate-memory", capacity: {out_capacity}  }
    output:
      memory: { topic: "test-out-internal", capacity: {out_capacity} }
"#;

pub async fn test_memory_performance_pipeline() {
    setup_logging();
    let config_yaml = CONFIG_YAML.replace(
        "{out_capacity}",
        &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
    );
    run_performance_pipeline_test("internal", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
}
