use metrics::{Key, Recorder};
use metrics_exporter_prometheus::PrometheusBuilder;
use mq_bridge_app::config::AppConfig;
use mq_bridge_app::web_ui;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

fn get_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn test_mutex() -> &'static tokio::sync::Mutex<()> {
    static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn unique_config_path(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mq_bridge_app_test_{}_{}.yml",
        port,
        uuid::Uuid::new_v4()
    ))
}

async fn start_test_server(port: u16, config: AppConfig, config_file: &Path) -> JoinHandle<()> {
    let builder = PrometheusBuilder::new();
    let recorder = builder.build_recorder();
    let handle = recorder.handle();
    let config_file_path = config_file.to_string_lossy().to_string();

    let server = tokio::spawn(async move {
        web_ui::start_web_server(
            format!("127.0.0.1:{}", port),
            config,
            Vec::new(),
            handle,
            config_file_path,
        )
        .await
        .unwrap();
    });

    let timeout = Duration::from_secs(8);
    let started_at = std::time::Instant::now();
    loop {
        let response = http_get(port, "/health").await;
        if response.contains("200 OK") {
            break;
        }
        if started_at.elapsed() > timeout {
            panic!("web UI did not become healthy within {:?}", timeout);
        }
        sleep(Duration::from_millis(100)).await;
    }
    server
}

async fn send_http_request(port: u16, request: &str) -> String {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(8);
    let mut stream = loop {
        match TcpStream::connect(format!("127.0.0.1:{}", port)).await {
            Ok(stream) => break stream,
            Err(_error) => {
                if start.elapsed() > timeout {
                    panic!(
                        "failed to connect to test server on 127.0.0.1:{} within {:?}",
                        port, timeout
                    );
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
    };

    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.unwrap();
    String::from_utf8_lossy(&buffer).into_owned()
}

async fn http_get(port: u16, path: &str) -> String {
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    send_http_request(port, &request).await
}

async fn http_post_json(port: u16, path: &str, json_payload: &str) -> String {
    http_post_json_with_headers(port, path, json_payload, &[]).await
}

async fn http_post_json_with_headers(
    port: u16,
    path: &str,
    json_payload: &str,
    headers: &[(&str, &str)],
) -> String {
    let extra_headers = headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}\r\n"))
        .collect::<String>();
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        extra_headers,
        json_payload.len(),
        json_payload
    );
    send_http_request(port, &request).await
}

async fn http_post(port: u16, path: &str) -> String {
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        path
    );
    send_http_request(port, &request).await
}

fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

async fn read_json_response(port: u16, path: &str) -> serde_json::Value {
    let response = http_get(port, path).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );
    serde_json::from_str(response_body(&response)).unwrap()
}

async fn stop_all_routes() {
    for route in mq_bridge_app::mq_bridge::list_routes() {
        mq_bridge_app::mq_bridge::stop_route(&route).await;
    }
}

#[tokio::test]
async fn test_web_ui_health_check() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let response = http_get(port, "/health").await;
    assert!(response.contains("200 OK"));
    assert!(response.contains("OK"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_metrics_endpoint() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let builder = PrometheusBuilder::new();
    let recorder = builder.build_recorder();
    let handle = recorder.handle();

    let key = Key::from_name("test_metric");
    let metadata = metrics::Metadata::new("test_metric", metrics::Level::INFO, None);
    let _ = recorder.register_counter(&key, &metadata);

    let config_file_path = config_file.to_string_lossy().to_string();
    let server = tokio::spawn(async move {
        web_ui::start_web_server(
            format!("127.0.0.1:{}", port),
            AppConfig::default(),
            Vec::new(),
            handle,
            config_file_path,
        )
        .await
        .unwrap();
    });
    sleep(Duration::from_millis(200)).await;

    let response = http_get(port, "/metrics").await;
    assert!(response.contains("200 OK"));
    assert!(response.contains("# TYPE test_metric counter"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_schema_and_index_expose_custom_ui_shape() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let schema_response = http_get(port, "/schema.json").await;
    assert!(schema_response.contains("200 OK"));
    assert!(schema_response.contains("\"AppConfig\""));
    assert!(schema_response.contains("\"enabled\""));
    assert!(!schema_response.contains("\"view\""));

    let index_response = http_get(port, "/").await;
    assert!(index_response.contains("200 OK"));
    assert!(index_response.contains("id=\"route-toggle\""));
    assert!(index_response.contains("id=\"pub-proto-label\">Type</span>"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_exposes_storage_security_json() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let storage_security = read_json_response(port, "/storage-security").await;
    assert_eq!(storage_security["target"], "cli");
    assert_eq!(storage_security["encrypted"], false);
    assert_eq!(storage_security["persistent"], true);

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_serves_custom_static_assets() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let app_bundle = http_get(port, "/index.js").await;
    assert!(app_bundle.contains("200 OK"));
    assert!(app_bundle.contains("pub-copy"));
    assert!(app_bundle.contains("pub-save-variant"));
    assert!(app_bundle.contains("cons-output-mode"));
    assert!(app_bundle.contains("cons-output-publisher"));
    assert!(app_bundle.contains("Execution History"));
    assert!(app_bundle.contains("js-storage-security-note"));

    let style_css = http_get(port, "/style.css").await;
    assert!(style_css.contains("200 OK"));
    assert!(style_css.contains(".route-disabled-tag"));
    assert!(style_css.contains(".request-field-label"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_post_config_migrates_legacy_route_to_consumer_config() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let route_name = format!("test_route_{}", uuid::Uuid::new_v4().simple());
    let json_payload = format!(
        r#"{{"log_level":"debug","logger":"plain","routes":{{"{route_name}":{{"enabled":true,"input":{{"memory":{{"topic":"in"}}}},"output":{{"memory":{{"topic":"out"}}}}}}}}}}"#
    );
    let response = http_post_json(port, "/config", &json_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );

    sleep(Duration::from_millis(200)).await;
    let routes = mq_bridge_app::mq_bridge::list_routes();
    assert!(!routes.contains(&route_name));

    let config_json = read_json_response(port, "/config").await;
    assert_eq!(config_json["consumers"][0]["name"], route_name);
    assert_eq!(
        config_json["publishers"][0]["name"],
        format!("{route_name}_publisher")
    );
    assert!(
        config_json["routes"]
            .as_object()
            .map(|routes| routes.is_empty())
            .unwrap_or(true)
    );

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_consumer_custom_response_is_returned_for_http_consumer() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let ui_port = get_free_port();
    let consumer_port = get_free_port();
    let config_file = unique_config_path(ui_port);
    let server = start_test_server(ui_port, AppConfig::default(), &config_file).await;

    let consumer_name = format!("test_consumer_{}", uuid::Uuid::new_v4().simple());
    let json_payload = format!(
        r#"{{
            "log_level":"debug",
            "logger":"plain",
            "consumers":[{{
                "name":"{consumer_name}",
                "endpoint":{{"http":{{"url":"127.0.0.1:{consumer_port}"}}}},
                "comment":"",
                "response":{{
                    "headers":{{"Content-Type":"application/json","X-Test-Reply":"configured"}},
                    "payload":"{{\"ok\":true}}"
                }}
            }}]
        }}"#
    );
    let response = http_post_json(ui_port, "/config", &json_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );
    let saved_config = read_json_response(ui_port, "/config").await;
    let consumer_id = saved_config["consumers"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !consumer_id.is_empty(),
        "expected saved consumer id in config: {}",
        saved_config
    );

    let start_response = http_post(
        ui_port,
        &format!("/consumer-start?consumer_id={consumer_id}"),
    )
    .await;
    assert!(
        start_response.contains("200 OK"),
        "unexpected response: {}",
        start_response
    );

    sleep(Duration::from_millis(200)).await;

    let consumer_response = send_http_request(
        consumer_port,
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    assert!(
        consumer_response.contains("200 OK"),
        "unexpected consumer response: {}",
        consumer_response
    );
    let consumer_response_lower = consumer_response.to_ascii_lowercase();
    assert!(consumer_response_lower.contains("x-test-reply: configured"));
    assert!(consumer_response_lower.contains("content-type: application/json"));
    assert!(consumer_response.contains("{\"ok\":true}"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_collects_http_consumer_messages_and_updates_runtime_status() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let ui_port = get_free_port();
    let consumer_port = get_free_port();
    let config_file = unique_config_path(ui_port);
    let server = start_test_server(ui_port, AppConfig::default(), &config_file).await;

    let consumer_name = format!("message_consumer_{}", uuid::Uuid::new_v4().simple());
    let json_payload = format!(
        r#"{{
            "log_level":"debug",
            "logger":"plain",
            "consumers":[{{
                "name":"{consumer_name}",
                "endpoint":{{"http":{{"url":"127.0.0.1:{consumer_port}","path":"/ui-test","method":"POST"}}}},
                "comment":"",
                "response":null
            }}],
            "publishers":[]
        }}"#
    );
    let response = http_post_json(ui_port, "/config", &json_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );
    let saved_config = read_json_response(ui_port, "/config").await;
    let consumer_id = saved_config["consumers"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !consumer_id.is_empty(),
        "expected saved consumer id in config: {}",
        saved_config
    );

    let start_response = http_post(
        ui_port,
        &format!("/consumer-start?consumer_id={consumer_id}"),
    )
    .await;
    assert!(
        start_response.contains("200 OK"),
        "unexpected response: {}",
        start_response
    );

    sleep(Duration::from_millis(200)).await;

    let consumer_response = send_http_request(
        consumer_port,
        "POST /ui-test HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    assert!(
        consumer_response.contains("202 Accepted"),
        "unexpected consumer response: {}",
        consumer_response
    );

    let runtime = read_json_response(ui_port, "/runtime-status").await;
    assert_eq!(
        runtime["consumers"][&consumer_id]["message_sequence"]
            .as_u64()
            .unwrap_or_default(),
        1
    );

    let messages =
        read_json_response(ui_port, &format!("/messages?consumer_id={consumer_id}")).await;
    let payload = messages[&consumer_id][0]["payload"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(payload, "hello");

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_post_config_accepts_disabled_route_and_migrates_it() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let route_name = format!("disabled_route_{}", uuid::Uuid::new_v4().simple());
    let json_payload = format!(
        r#"{{"log_level":"debug","logger":"plain","routes":{{"{route_name}":{{"enabled":false,"input":{{"memory":{{"topic":"disabled-in"}}}},"output":{{"memory":{{"topic":"disabled-out"}}}}}}}}}}"#
    );
    let response = http_post_json(port, "/config", &json_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );

    sleep(Duration::from_millis(200)).await;
    let routes = mq_bridge_app::mq_bridge::list_routes();
    assert!(!routes.contains(&route_name));

    let config_json = read_json_response(port, "/config").await;
    assert_eq!(config_json["consumers"][0]["name"], route_name);
    assert!(
        config_json["routes"]
            .as_object()
            .map(|routes| routes.is_empty())
            .unwrap_or(true)
    );

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_rejects_cross_origin_post_requests() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let response = http_post_json_with_headers(
        port,
        "/config",
        r#"{"log_level":"debug","logger":"plain","routes":{}}"#,
        &[("Origin", "http://evil.example")],
    )
    .await;

    assert!(
        response.contains("403 Forbidden"),
        "unexpected response: {}",
        response
    );
    assert!(response_body(&response).contains("Forbidden"));

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}

#[tokio::test]
async fn test_web_ui_legacy_route_payload_does_not_create_active_runtime_routes() {
    let _guard = test_mutex().lock().await;
    stop_all_routes().await;

    let port = get_free_port();
    let config_file = unique_config_path(port);
    let server = start_test_server(port, AppConfig::default(), &config_file).await;

    let route_name = format!("toggle_route_{}", uuid::Uuid::new_v4().simple());
    let enabled_payload = format!(
        r#"{{"log_level":"debug","logger":"plain","routes":{{"{route_name}":{{"enabled":true,"input":{{"memory":{{"topic":"toggle-in"}}}},"output":{{"memory":{{"topic":"toggle-out"}}}}}}}}}}"#
    );
    let response = http_post_json(port, "/config", &enabled_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );

    sleep(Duration::from_millis(200)).await;
    let runtime_before = read_json_response(port, "/runtime-status").await;
    assert!(
        !runtime_before["active_routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == &serde_json::Value::String(route_name.clone()))
    );

    let disabled_payload = format!(
        r#"{{"log_level":"debug","logger":"plain","routes":{{"{route_name}":{{"enabled":false,"input":{{"memory":{{"topic":"toggle-in"}}}},"output":{{"memory":{{"topic":"toggle-out"}}}}}}}}}}"#
    );
    let response = http_post_json(port, "/config", &disabled_payload).await;
    assert!(
        response.contains("200 OK"),
        "unexpected response: {}",
        response
    );

    sleep(Duration::from_millis(200)).await;
    let routes = mq_bridge_app::mq_bridge::list_routes();
    assert!(!routes.contains(&route_name));

    let runtime_after = read_json_response(port, "/runtime-status").await;
    assert!(
        !runtime_after["active_routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == &serde_json::Value::String(route_name.clone()))
    );

    let config_json = read_json_response(port, "/config").await;
    assert_eq!(config_json["consumers"][0]["name"], route_name);

    server.abort();
    stop_all_routes().await;
    let _ = std::fs::remove_file(config_file);
}
