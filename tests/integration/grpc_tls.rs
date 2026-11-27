#![allow(dead_code)]
#![allow(unused_imports)]

use crate::integration::tls_helpers;
use mq_bridge::models::Route;
use mq_bridge::test_utils::setup_logging;
use serde_yaml_ng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const CONFIG_YAML: &str = r#"
routes:
  memory_to_grpc:
    concurrency: 2
    batch_size: 16
    input:
      memory: { topic: "test-in-grpc" }
    output:
      grpc:
        url: "http://127.0.0.1:50051"

  grpc_to_memory:
    concurrency: 2
    batch_size: 16
    input:
      grpc:
        url: "127.0.0.1:50051"
    output:
      memory: { topic: "test-out-grpc", capacity: 100 }
"#;

#[tokio::test]
#[ignore = "requires local certs and network"]
async fn test_grpc_tls_roundtrip() {
    // Generates local cert material and binds localhost ports for an in-process
    // TLS gRPC pipeline. No Docker is expected.
    setup_logging();

    let yaml_val: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(CONFIG_YAML).expect("Failed to parse YAML config");
    let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
    let mut routes: HashMap<String, Route> =
        serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

    // Bind an ephemeral port so the test doesn't rely on a hardcoded port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();

    // Release the reserved listener so the embedded server can bind to the port.
    drop(listener);

    // Generate certs and set up TLS configs
    let cert_dir = tls_helpers::generate_service_certs("grpc").expect("generate certs");
    if let Some(out_route) = routes.get_mut("grpc_to_memory") {
        if let mq_bridge::models::EndpointType::Grpc(ref mut cfg) = out_route.input.endpoint_type {
            *cfg =
                tls_helpers::grpc_server_config_with_tls(&cert_dir, format!("127.0.0.1:{}", port));
            cfg.topic = Some("tls_test".to_string());
        }
    }
    if let Some(in_route) = routes.get_mut("memory_to_grpc") {
        if let mq_bridge::models::EndpointType::Grpc(ref mut cfg) = in_route.output.endpoint_type {
            *cfg = tls_helpers::grpc_client_config_with_tls(
                &cert_dir,
                format!("https://127.0.0.1:{}", port),
            );
            cfg.topic = Some("tls_test".to_string());
        }
    }

    let in_route = routes["memory_to_grpc"].clone();
    let out_route = routes["grpc_to_memory"].clone();

    // Start server (deploy) and wait until the socket accepts connections.
    out_route
        .deploy("grpc_to_memory")
        .await
        .expect("Failed to deploy gRPC server");
    let server_addr = format!("127.0.0.1:{}", port);
    let ready_start = Instant::now();
    loop {
        match tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(&server_addr),
        )
        .await
        {
            Ok(Ok(_stream)) => break,
            _ if ready_start.elapsed() < Duration::from_secs(2) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => panic!("gRPC server did not become ready at {}", server_addr),
        }
    }

    in_route
        .deploy("memory_to_grpc")
        .await
        .expect("Failed to deploy memory_to_grpc route");

    // simple sanity check: send one message and ensure it's received
    let in_channel = in_route.input.channel().unwrap();
    in_channel
        .fill_messages(vec![
            mq_bridge::test_utils::generate_test_messages(1)
                .pop()
                .unwrap(),
        ])
        .await
        .unwrap();

    let memory_channel = out_route.output.channel().unwrap();
    let start = Instant::now();
    let mut batch = Vec::new();
    while start.elapsed() < Duration::from_secs(10) {
        batch = memory_channel.drain_messages();
        if !batch.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        !batch.is_empty(),
        "Expected at least one message via gRPC within 10s"
    );

    mq_bridge::Route::stop("memory_to_grpc").await;
    mq_bridge::Route::stop("grpc_to_memory").await;
}
