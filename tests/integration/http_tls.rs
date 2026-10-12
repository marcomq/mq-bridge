#![allow(dead_code, unused)]

use crate::integration::tls_helpers;
use mq_bridge::models::Route;
use mq_bridge::test_utils::{get_free_port, setup_logging};
use mq_bridge::test_utils::{PERF_TEST_MESSAGE_COUNT, PERF_TEST_SINGLE_MESSAGE_COUNT};
use serde_yaml_ng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const CONFIG_YAML: &str = r#"
routes:
  memory_to_http:
    concurrency: 2
    batch_size: 16
    input:
      memory: { topic: "test-in-http-tls" }
    output:
      http:
        url: "https://127.0.0.1:{out_port}"
        request_timeout_ms: 5000
        batch_concurrency: 2

  http_to_memory:
    concurrency: 2
    batch_size: 16
    input:
      http:
        url: "127.0.0.1:{out_port}"
        internal_buffer_size: {buffer_size}
        fire_and_forget: false
    output:
      memory: { topic: "test-out-http-tls", capacity: {out_capacity} }
"#;

async fn wait_for_server_ready(addr: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
#[ignore = "requires docker/local certs"]
async fn test_http_tls_pipeline() {
    // Generates local cert material, binds localhost ports, and pushes the
    // performance message count through an HTTPS pipeline.
    setup_logging();

    // Try a few times to avoid ephemeral port race
    for _ in 0..3 {
        let port = get_free_port();
        let config_yaml = CONFIG_YAML
            .replace("{out_port}", &port.to_string())
            .replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 100).to_string(),
            )
            .replace("{buffer_size}", &(PERF_TEST_MESSAGE_COUNT * 2).to_string());

        let yaml_val: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&config_yaml).expect("Failed to parse YAML config");
        let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
        let mut routes: HashMap<String, Route> =
            serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

        let in_route_name = "memory_to_http".to_string();
        let out_route_name = "http_to_memory".to_string();

        // Inject TLS settings using generated certs
        let cert_dir = tls_helpers::generate_service_certs("http").expect("generate certs");
        if let Some(out_route) = routes.get_mut(out_route_name.as_str()) {
            if let mq_bridge::models::EndpointType::Http(ref mut http_cfg) =
                out_route.input.endpoint_type
            {
                *http_cfg = tls_helpers::http_consumer_config_with_tls(
                    &cert_dir,
                    format!("127.0.0.1:{}", port),
                );
            }
        }
        if let Some(in_route) = routes.get_mut(in_route_name.as_str()) {
            if let mq_bridge::models::EndpointType::Http(ref mut http_cfg) =
                in_route.output.endpoint_type
            {
                *http_cfg = tls_helpers::http_publisher_config_with_tls(
                    &cert_dir,
                    format!("https://127.0.0.1:{}", port),
                );
            }
        }

        let in_route = routes[&in_route_name].clone();
        let out_route = routes[&out_route_name].clone();

        // Try to deploy the HTTPS server and probe readiness
        match out_route.deploy(&out_route_name).await {
            Ok(_) => {
                let addr = format!("127.0.0.1:{}", port);
                if wait_for_server_ready(&addr, Duration::from_secs(5)).await {
                    // Deploy publisher and run pipeline
                    in_route
                        .deploy(&in_route_name)
                        .await
                        .expect("Failed to deploy memory_to_http route");

                    // Fill input and wait for messages
                    let in_channel = in_route.input.channel().unwrap();
                    let messages =
                        mq_bridge::test_utils::generate_test_messages(PERF_TEST_MESSAGE_COUNT);
                    in_channel.fill_messages(messages).await.unwrap();

                    let memory_channel = out_route.output.channel().unwrap();
                    let deadline = Duration::from_secs(90);
                    let start = Instant::now();
                    let mut received = 0usize;
                    while start.elapsed() < deadline {
                        let batch = memory_channel.drain_messages();
                        if !batch.is_empty() {
                            received += batch.len();
                        }
                        if received >= PERF_TEST_MESSAGE_COUNT {
                            break;
                        }
                        if start.elapsed().as_secs().is_multiple_of(5) {
                            println!(
                                "Progress: {} / {} received after {:?}",
                                received,
                                PERF_TEST_MESSAGE_COUNT,
                                start.elapsed()
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }

                    mq_bridge::Route::stop(&in_route_name).await;
                    mq_bridge::Route::stop(&out_route_name).await;

                    assert_eq!(received, PERF_TEST_MESSAGE_COUNT);
                    return;
                } else {
                    let _ = mq_bridge::Route::stop(&out_route_name).await;
                    continue;
                }
            }
            Err(e) => {
                eprintln!("Failed to deploy http consumer: {}", e);
                continue;
            }
        }
    }
    panic!("Failed to run http TLS pipeline after retries");
}
