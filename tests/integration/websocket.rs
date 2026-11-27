#![allow(dead_code, unused)]

use mq_bridge::models::{Endpoint, EndpointType, WebSocketConfig};
use mq_bridge::test_utils::{
    PERF_TEST_MESSAGE_COUNT, get_free_port, run_performance_pipeline_test, run_pipeline_test,
    setup_logging,
};

const CONFIG_YAML: &str = r#"
routes:
  memory_to_websocket:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-websocket" }
    output:
      websocket:
        url: "ws://127.0.0.1:{port}/events"

  websocket_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      websocket:
        url: "127.0.0.1:{port}"
        path: "/events"
        routed_queue_capacity: {buffer_size}
    output:
      memory: { topic: "test-out-websocket", capacity: {out_capacity} }
"#;

fn websocket_config_yaml(port: u16, message_count: usize) -> String {
    CONFIG_YAML
        .replace("{port}", &port.to_string())
        .replace("{out_capacity}", &(message_count + 1000).to_string())
        .replace("{buffer_size}", &(message_count * 2).to_string())
}

#[cfg(feature = "websocket")]
#[tokio::test(flavor = "multi_thread")]
async fn test_websocket_smoke_pipeline() {
    setup_logging();
    let config_yaml = websocket_config_yaml(get_free_port(), 5);
    run_pipeline_test("websocket", &config_yaml).await;
}

pub async fn test_websocket_performance_pipeline() {
    setup_logging();
    let config_yaml = websocket_config_yaml(get_free_port(), PERF_TEST_MESSAGE_COUNT);
    run_performance_pipeline_test("websocket", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
}

#[cfg(feature = "websocket")]
#[tokio::test(flavor = "multi_thread")]
async fn test_websocket_concurrency() {
    setup_logging();
    let port = get_free_port();
    let path = "/events";
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}")).with_path(path),
    ));
    let sender = Endpoint::new(EndpointType::WebSocket(WebSocketConfig::new(format!(
        "ws://127.0.0.1:{port}{path}"
    ))));
    let output = Endpoint::new_memory("con_out_websocket", 10);

    mq_bridge::test_utils::run_concurrency_test(input, output, sender).await;
}
