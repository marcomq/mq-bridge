#![allow(dead_code, unused)]

use mq_bridge::models::{
    CookieJarMiddleware, Endpoint, EndpointType, HttpConfig, Middleware, Route,
};
use mq_bridge::test_utils::{get_free_port, setup_logging, PERF_TEST_MESSAGE_COUNT};
use serde_yaml_ng;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CONFIG_YAML: &str = r#"
routes:
  memory_to_http:
    concurrency: 1
    batch_size: 128
    input:
      memory: { topic: "test-in-http" }
    output:
      http:
        url: "http://127.0.0.1:{out_port}"
        request_timeout_ms: 5000
        batch_concurrency: 4

  http_to_memory:
    concurrency: 1
    batch_size: 128
    input:
      http:
        url: "127.0.0.1:{out_port}"
        internal_buffer_size: {buffer_size}
        fire_and_forget: false
    output:
      memory: { topic: "test-out-http", capacity: {out_capacity} }
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

pub async fn test_http_performance_pipeline() {
    setup_logging();
    tokio::time::timeout(Duration::from_secs(60), async {
        let in_route_name = "memory_to_http".to_string();
        let out_route_name = "http_to_memory".to_string();

        // Try a few times to avoid a TOCTOU race for an ephemeral port.
        let mut deployed_out_route: Option<Route> = None;
        let mut deployed_in_route: Option<Route> = None;
        for _attempt in 0..5 {
            let port = get_free_port();
            let config_yaml = CONFIG_YAML
                .replace("{out_port}", &port.to_string())
                .replace(
                    "{out_capacity}",
                    &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
                )
                .replace("{buffer_size}", &(PERF_TEST_MESSAGE_COUNT * 2).to_string());
            let yaml_val: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(&config_yaml).expect("Failed to parse YAML config");
            let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
            let routes: HashMap<String, Route> =
                serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

            let in_route = routes[&in_route_name].clone();
            let mut out_route = routes[&out_route_name].clone(); // Make out_route mutable

            let enable_dummy_handler = std::env::var("MQB_ENABLE_DUMMY_HANDLER")
                .map(|s| s.to_lowercase() == "true")
                .unwrap_or(false);

            let handler_description = if enable_dummy_handler {
                let dummy_handler = |msg: mq_bridge::CanonicalMessage| async move {
                    // This handler does minimal work: just forward the message.
                    // It simulates the overhead of a handler without actual business logic.
                    Ok(mq_bridge::Handled::Publish(msg))
                };
                out_route = out_route.with_handler(dummy_handler);
                " (with dummy handler)"
            } else {
                ""
            };

            // Attempt to deploy the HTTP consumer (server) and probe readiness.
            match out_route.deploy(&out_route_name).await {
                Ok(_) => {
                    let addr = format!("127.0.0.1:{}", port);
                    if wait_for_server_ready(&addr, Duration::from_secs(5)).await {
                        deployed_out_route = Some(out_route);
                        deployed_in_route = Some(in_route);
                        break;
                    } else {
                        // Not ready, stop and retry with a new port
                        let _ = mq_bridge::Route::stop(&out_route_name).await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                }
                Err(e) => {
                    eprintln!("Failed to deploy http consumer on port {}: {}", port, e);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        }

        let in_route = deployed_in_route.expect("Failed to deploy routes after retries");
        let out_route = deployed_out_route.expect("Failed to deploy http consumer after retries");

        // Get the memory output channel that the server writes into.
        let memory_channel = out_route.output.channel().unwrap();

        // Start the publisher (memory -> HTTP) route.
        in_route
            .deploy(&in_route_name)
            .await
            .expect("Failed to deploy memory_to_http route");

        // Obtain the input memory channel and fill it with test messages.
        let in_channel = in_route.input.channel().unwrap();
        let messages = mq_bridge::test_utils::generate_test_messages(PERF_TEST_MESSAGE_COUNT);
        in_channel.fill_messages(messages).await.unwrap();

        // Poll the output channel until all messages arrive or we time out.
        // Keep this below the outer 60s timeout to allow cleanup to complete.
        let deadline = Duration::from_secs(45);
        let start = Instant::now();
        let mut last_log = Instant::now();
        let mut received = 0usize;

        while start.elapsed() < deadline {
            let batch = memory_channel.drain_messages();
            if !batch.is_empty() {
                received += batch.len();
            }
            if received >= PERF_TEST_MESSAGE_COUNT {
                break;
            }
            if last_log.elapsed() >= Duration::from_secs(5) {
                println!(
                    "Progress: {}/{} messages received",
                    received, PERF_TEST_MESSAGE_COUNT
                );
                last_log = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let duration = start.elapsed();

        // Stop both routes (Route::stop has a built-in 5 s timeout so this won't hang).
        mq_bridge::Route::stop(&in_route_name).await;
        mq_bridge::Route::stop(&out_route_name).await;

        let messages_per_second = received as f64 / duration.as_secs_f64();
        mq_bridge::test_utils::add_performance_result(mq_bridge::test_utils::PerformanceResult {
            test_name: "HTTP Pipeline".to_string(),
            write_performance: messages_per_second,
            read_performance: messages_per_second,
            ..Default::default()
        });

        assert_eq!(
            received, PERF_TEST_MESSAGE_COUNT,
            "Expected {} messages, received {}",
            PERF_TEST_MESSAGE_COUNT, received
        );
    })
    .await
    .expect("HTTP pipeline test timed out");
}

#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread")]
async fn test_http_concurrency() {
    setup_logging();
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("127.0.0.1:{}", port),
        ..Default::default()
    }));
    // Publisher needs the schema
    let sender = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("http://127.0.0.1:{}", port),
        ..Default::default()
    }));
    let output = Endpoint::new_memory("con_out_http", 10);

    mq_bridge::test_utils::run_concurrency_test(input, output, sender).await;
}

#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread")]
async fn test_http_cookie_jar_persists_session_headers() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use mq_bridge::{CanonicalMessage, Publisher};
    use tokio::net::TcpListener;

    setup_logging();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{port}");
    let observed_headers = Arc::new(Mutex::new(Vec::<HashMap<String, String>>::new()));
    let request_count = Arc::new(AtomicUsize::new(0));

    let server = {
        let addr = addr.clone();
        let observed_headers = Arc::clone(&observed_headers);
        let request_count = Arc::clone(&request_count);
        tokio::spawn(async move {
            let listener = TcpListener::bind(&addr).await.unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let observed_headers = Arc::clone(&observed_headers);
                let request_count = Arc::clone(&request_count);
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let observed_headers = Arc::clone(&observed_headers);
                        let request_count = Arc::clone(&request_count);
                        async move {
                            let mut headers = HashMap::new();
                            if let Some(cookie) = req
                                .headers()
                                .get("cookie")
                                .and_then(|value| value.to_str().ok())
                            {
                                headers.insert("cookie".to_string(), cookie.to_string());
                            }
                            if let Some(csrf) = req
                                .headers()
                                .get("x-forwarded-csrf")
                                .and_then(|value| value.to_str().ok())
                            {
                                headers.insert("x-forwarded-csrf".to_string(), csrf.to_string());
                            }
                            observed_headers.lock().unwrap().push(headers);

                            let request_index = request_count.fetch_add(1, Ordering::SeqCst);
                            let mut builder = Response::builder().status(StatusCode::OK);
                            if request_index == 0 {
                                builder = builder
                                    .header("set-cookie", "session_id=abc123; Path=/; HttpOnly")
                                    .header("set-cookie", "theme=dark; Path=/")
                                    .header("x-csrf-token", "csrf-123");
                            }

                            Ok::<_, Infallible>(
                                builder.body(Full::new(Bytes::from_static(b"ok"))).unwrap(),
                            )
                        }
                    });

                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        })
    };

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let mut endpoint = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("http://{addr}"),
        request_timeout_ms: Some(2_000),
        ..Default::default()
    }));
    endpoint
        .middlewares
        .push(Middleware::CookieJar(CookieJarMiddleware {
            capture_metadata_keys: vec!["x-csrf-token".to_string()],
            inject_metadata: HashMap::from([(
                "x-forwarded-csrf".to_string(),
                "x-csrf-token".to_string(),
            )]),
            ..Default::default()
        }));

    let publisher = Publisher::new(endpoint).await.unwrap();
    publisher
        .request(CanonicalMessage::from("first"))
        .await
        .unwrap();
    publisher
        .request(CanonicalMessage::from("second"))
        .await
        .unwrap();

    let started = Instant::now();
    while request_count.load(Ordering::SeqCst) < 2 {
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let observed_headers = observed_headers.lock().unwrap().clone();
    assert_eq!(observed_headers.len(), 2);
    assert!(!observed_headers[0].contains_key("cookie"));
    let mut cookies: Vec<&str> = observed_headers[1]["cookie"].split("; ").collect();
    cookies.sort_unstable();
    assert_eq!(cookies, ["session_id=abc123", "theme=dark"]);
    assert_eq!(
        observed_headers[1]
            .get("x-forwarded-csrf")
            .map(String::as_str),
        Some("csrf-123")
    );

    server.abort();
}
