//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;
use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
use crate::models::{Config, Endpoint, EndpointType, StreamBufferConfig};
use crate::test_utils::get_free_port;
use hyper::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

fn raw_text_static_endpoint(body: &str) -> Endpoint {
    let mut metadata = HashMap::new();
    metadata.insert("content-type".to_string(), "text/plain".to_string());
    Endpoint::new(EndpointType::Static(crate::models::StaticConfig {
        body: body.to_string(),
        raw: true,
        metadata,
    }))
}

fn init_crypto() {
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[test]
fn test_http_config_yaml() {
    let yaml = r#"
http_route:
  input:
    http:
      url: "127.0.0.1:8080"
  output:
    http:
      url: "http://localhost:9090"
      pass_through_status: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
    let route = config.get("http_route").expect("Route not found");

    match &route.input.endpoint_type {
        EndpointType::Http(cfg) => {
            assert_eq!(cfg.url, "127.0.0.1:8080".to_string());
        }
        _ => panic!("Expected HTTP input"),
    }

    match &route.output.endpoint_type {
        EndpointType::Http(cfg) => {
            assert_eq!(cfg.url, "http://localhost:9090".to_string());
            assert!(cfg.pass_through_status);
        }
        _ => panic!("Expected HTTP output"),
    }
}

#[test]
fn test_http_config_yaml_server_protocol() {
    let yaml = r#"
http_route:
  input:
    http:
      url: "127.0.0.1:8080"
      server_protocol: http2_only
  output:
    response: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
    let route = config.get("http_route").expect("Route not found");

    match &route.input.endpoint_type {
        EndpointType::Http(cfg) => {
            assert_eq!(cfg.server_protocol, HttpServerProtocol::Http2Only);
        }
        _ => panic!("Expected HTTP input"),
    }
}

#[tokio::test]
async fn http_publisher_pass_through_status_is_opt_in() {
    init_crypto();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|req: Request<Incoming>| async move {
                    let status = if req.uri().path() == "/unavailable" {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::NOT_FOUND
                    };
                    Ok::<_, anyhow::Error>(
                        Response::builder()
                            .status(status)
                            .header("x-upstream", "stub")
                            .body(full(status.as_str().to_string()))
                            .unwrap(),
                    )
                });
                AutoBuilder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .unwrap();
            });
        }
    });

    let default_publisher = HttpPublisher::new(&HttpConfig {
        url: format!("http://{addr}/not-found"),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        default_publisher
            .send(CanonicalMessage::from_vec("request"))
            .await
            .unwrap_err(),
        PublisherError::NonRetryable(_)
    ));

    for (path, expected_status) in [("not-found", "404"), ("unavailable", "503")] {
        let publisher = HttpPublisher::new(&HttpConfig {
            url: format!("http://{addr}/{path}"),
            pass_through_status: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let Sent::Response(response) = publisher
            .send(CanonicalMessage::from_vec("request"))
            .await
            .unwrap()
        else {
            panic!("pass-through status returned Ack");
        };
        assert_eq!(
            response.metadata.get(HTTP_STATUS_CODE).map(String::as_str),
            Some(expected_status)
        );
        assert_eq!(response.get_payload_str(), expected_status);
    }
    server_task.abort();
}

#[test]
fn test_guess_content_type_from_path() {
    assert_eq!(
        guess_content_type("/assets/app.bundle.js"),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(guess_content_type("images/logo.SVG"), "image/svg+xml");
}

#[test]
fn test_guess_content_type_from_extension() {
    assert_eq!(guess_content_type("html"), "text/html; charset=utf-8");
    assert_eq!(guess_content_type(".woff2"), "font/woff2");
    assert_eq!(
        guess_content_type("JSON"),
        "application/json; charset=utf-8"
    );
}

#[test]
fn test_guess_content_type_unknown_defaults_to_octet_stream() {
    assert_eq!(guess_content_type(""), "application/octet-stream");
    assert_eq!(
        guess_content_type("unknown-ext"),
        "application/octet-stream"
    );
    assert_eq!(
        guess_content_type("archive.custombin"),
        "application/octet-stream"
    );
}

#[test]
fn test_request_accepts_text_defaults_true_without_accept_header() {
    let headers = hyper::HeaderMap::new();
    assert!(request_accepts_text(&headers));
}

#[test]
fn test_request_accepts_text_matches_text_and_wildcards() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(ACCEPT, "application/json, text/plain".parse().unwrap());
    assert!(request_accepts_text(&headers));

    headers.insert(ACCEPT, "*/*".parse().unwrap());
    assert!(request_accepts_text(&headers));
}

#[test]
fn test_request_accepts_text_rejects_binary_only_accept_header() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(ACCEPT, "application/octet-stream".parse().unwrap());
    assert!(!request_accepts_text(&headers));
}

#[test]
fn test_request_accepts_gzip_false_without_header() {
    // No Accept-Encoding => identity only, do not compress.
    let headers = hyper::HeaderMap::new();
    assert!(!request_accepts(&headers, "gzip", true));
}

#[test]
fn test_request_accepts_gzip_matches_gzip_and_wildcard() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, "gzip, deflate, br".parse().unwrap());
    assert!(request_accepts(&headers, "gzip", true));

    headers.insert(ACCEPT_ENCODING, "deflate, gzip;q=0.8".parse().unwrap());
    assert!(request_accepts(&headers, "gzip", true));

    headers.insert(ACCEPT_ENCODING, "*".parse().unwrap());
    assert!(request_accepts(&headers, "gzip", true));
}

#[test]
fn test_request_accepts_gzip_honors_q_zero_and_other_codings() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, "gzip;q=0".parse().unwrap());
    assert!(!request_accepts(&headers, "gzip", true));

    headers.insert(ACCEPT_ENCODING, "*;q=0".parse().unwrap());
    assert!(!request_accepts(&headers, "gzip", true));

    headers.insert(ACCEPT_ENCODING, "br, deflate".parse().unwrap());
    assert!(!request_accepts(&headers, "gzip", true));
}

#[test]
fn test_negotiate_response_compression_best_of_all() {
    let mut headers = hyper::HeaderMap::new();

    // A bare `*` accepts gzip but not lz4/zstd (a generic client can't decode those).
    headers.insert(ACCEPT_ENCODING, "*".parse().unwrap());
    assert_eq!(negotiate_response_compression(&headers), Compression::Gzip);

    // Explicit lz4 is honored when it's all the client accepts.
    headers.insert(ACCEPT_ENCODING, "lz4".parse().unwrap());
    assert_eq!(negotiate_response_compression(&headers), Compression::Lz4);

    // zstd wins over gzip when both are explicitly accepted.
    headers.insert(ACCEPT_ENCODING, "gzip, br, zstd".parse().unwrap());
    assert_eq!(negotiate_response_compression(&headers), Compression::Zstd);

    // No zstd/lz4 token => falls back to gzip.
    headers.insert(ACCEPT_ENCODING, "gzip, br".parse().unwrap());
    assert_eq!(negotiate_response_compression(&headers), Compression::Gzip);

    // Nothing acceptable => identity.
    headers.insert(ACCEPT_ENCODING, "br".parse().unwrap());
    assert_eq!(negotiate_response_compression(&headers), Compression::None);
}

#[test]
fn test_config_compression_resolution() {
    // Publisher: `compression_enabled` alone => gzip; explicit `compression` overrides it.
    let mut cfg = HttpConfig::new("http://localhost");
    cfg.compression_enabled = Some(true);
    assert_eq!(cfg.publisher_compression(), Compression::Gzip);
    assert!(cfg.consumer_compression_enabled());

    cfg.compression = Compression::Lz4;
    assert_eq!(cfg.publisher_compression(), Compression::Lz4);

    // An explicit codec drives the publisher but does NOT enable the consumer (flag-only).
    let mut only_codec = HttpConfig::new("http://localhost");
    only_codec.compression = Compression::Zstd;
    assert_eq!(only_codec.publisher_compression(), Compression::Zstd);
    assert!(!only_codec.consumer_compression_enabled());

    // Neither set => off.
    let plain = HttpConfig::new("http://localhost");
    assert_eq!(plain.publisher_compression(), Compression::None);
    assert!(!plain.consumer_compression_enabled());
}

#[test]
fn test_compress_decompress_round_trip_lz4_and_zstd() {
    for (method, token) in [(Compression::Lz4, "lz4"), (Compression::Zstd, "zstd")] {
        let data = Bytes::from(vec![b'x'; 4096]);
        let (compressed, encoding) = compress_if_needed(data.clone(), method, 16).unwrap();
        assert_eq!(encoding, Some(token));
        assert!(compressed.len() < data.len());
        let restored = decompress_if_needed(compressed, Some(token)).unwrap();
        assert_eq!(restored, data, "method {method:?}");
    }
}

#[test]
fn test_compress_decompress_round_trip_gzip_reuses_encoder() {
    // Repeated so the thread-local encoder is exercised on a reset, not just a fresh build.
    for _ in 0..3 {
        for len in [1024usize, 4096, 200_000] {
            let data = Bytes::from((0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
            let (compressed, encoding) =
                compress_if_needed(data.clone(), Compression::Gzip, 16).unwrap();
            assert_eq!(encoding, Some("gzip"), "len {len}");
            let restored = decompress_if_needed(compressed, Some("gzip")).unwrap();
            assert_eq!(restored, data, "len {len}");
        }
    }
}

#[test]
fn test_gzip_http_grows_output_past_initial_capacity() {
    // Incompressible input: deflate emits more than the `len / 2 + 64` the output
    // buffer starts with, so the member is only correct if the growth loop is.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let data: Vec<u8> = (0..64 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();

    for _ in 0..2 {
        let gzipped = gzip_http(&data).unwrap();
        assert!(gzipped.len() > data.len() / 2 + 64);
        let restored = decompress_if_needed(Bytes::from(gzipped), Some("gzip")).unwrap();
        assert_eq!(restored, data);
    }
}

#[test]
fn test_text_error_response_sets_text_content_type_when_accepted() {
    let response = text_error_response(StatusCode::BAD_REQUEST, "bad request", true, None);

    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
}

#[test]
fn test_text_error_response_skips_text_content_type_when_not_accepted() {
    let response = text_error_response(StatusCode::BAD_REQUEST, "bad request", false, None);

    assert!(response.headers().get(CONTENT_TYPE).is_none());
}

#[test]
fn test_text_error_response_preserves_custom_content_type() {
    let mut headers = HashMap::new();
    headers.insert(
        "content-type".to_string(),
        "application/problem+json".to_string(),
    );

    let response =
        text_error_response(StatusCode::BAD_REQUEST, "bad request", true, Some(&headers));

    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
}

#[test]
fn test_basic_auth_header_value_omits_empty_credentials() {
    let empty = (String::new(), String::new());
    assert_eq!(basic_auth_header_value(Some(&empty)), None);
    assert_eq!(basic_auth_header_value(None), None);
}

#[test]
fn test_configured_basic_auth_omits_empty_credentials() {
    let empty = (String::new(), String::new());
    assert_eq!(configured_basic_auth(Some(&empty)), None);
    assert_eq!(configured_basic_auth(None), None);
}

#[test]
fn test_configured_basic_auth_keeps_non_empty_credentials() {
    let creds = ("user".to_string(), "pass".to_string());
    assert_eq!(configured_basic_auth(Some(&creds)), Some(("user", "pass")));
}

#[test]
fn test_basic_auth_header_value_encodes_configured_credentials() {
    let creds = ("user".to_string(), "pass".to_string());
    assert_eq!(
        basic_auth_header_value(Some(&creds)).as_deref(),
        Some("Basic dXNlcjpwYXNz")
    );
}

#[tokio::test]
async fn test_http_consumer_publisher_integration() {
    init_crypto();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let url = format!("http://{}", addr);

    let config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };

    let mut consumer = HttpConsumer::new(&config)
        .await
        .expect("Failed to create consumer");

    let pub_config = HttpConfig {
        url: url.clone(),
        ..Default::default()
    };
    let publisher = HttpPublisher::new(&pub_config)
        .await
        .expect("Failed to create publisher");

    let msg_payload = b"test_payload".to_vec();
    let msg = CanonicalMessage::new(msg_payload.clone(), None);

    let receive_task = tokio::spawn(async move {
        let received = consumer.receive().await.expect("Failed to receive");
        let response_msg = CanonicalMessage::new(b"response_payload".to_vec(), None);
        let _ = (received.commit)(crate::traits::MessageDisposition::Reply(response_msg)).await;
        received.message
    });

    let response = publisher.send(msg).await.expect("Failed to send");

    let received_msg = receive_task.await.expect("Receive task failed");
    assert_eq!(received_msg.payload, msg_payload);
    let response = match response {
        Sent::Response(msg) => msg,
        _ => panic!("Expected response"),
    };
    assert_eq!(response.payload, b"response_payload".to_vec());
    // The publisher exposes the response status alongside the headers/version.
    assert_eq!(
        response.metadata.get(HTTP_STATUS_CODE).map(String::as_str),
        Some("200")
    );
}

#[tokio::test]
async fn test_http_receive_streamable_sse_items_share_correlation_id() {
    init_crypto();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let url = format!("http://{}", addr);

    let config = HttpConfig {
        url: addr.clone(),
        receive_streamable: true,
        ..Default::default()
    };

    let mut consumer = HttpConsumer::new(&config)
        .await
        .expect("Failed to create consumer");

    let publisher = HttpPublisher::new(&HttpConfig {
        url,
        ..Default::default()
    })
    .await
    .expect("Failed to create publisher");

    let receive_task = tokio::spawn(async move {
        let first = consumer.receive().await.expect("first stream item");
        let second = consumer.receive().await.expect("second stream item");

        assert_eq!(first.message.get_payload_str(), "first");
        assert_eq!(second.message.get_payload_str(), "second");
        assert_ne!(first.message.message_id, second.message.message_id);

        let first_correlation = first
            .message
            .metadata
            .get("correlation_id")
            .cloned()
            .expect("first correlation_id");
        let second_correlation = second
            .message
            .metadata
            .get("correlation_id")
            .cloned()
            .expect("second correlation_id");
        assert_eq!(first_correlation, second_correlation);
        assert_eq!(
            first
                .message
                .metadata
                .get("http_stream_index")
                .map(String::as_str),
            Some("0")
        );
        assert_eq!(
            second
                .message
                .metadata
                .get("http_stream_index")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            second.message.metadata.get("sse_id").map(String::as_str),
            Some("evt-2")
        );
        assert_eq!(
            second.message.metadata.get("sse_event").map(String::as_str),
            Some("update")
        );

        let first_reply = CanonicalMessage::from_vec("reply-first");
        let second_reply = CanonicalMessage::from_vec("reply-second");
        (first.commit)(MessageDisposition::Reply(first_reply))
            .await
            .expect("commit first reply");
        (second.commit)(MessageDisposition::Reply(second_reply))
            .await
            .expect("commit second reply");
    });

    let request =
        CanonicalMessage::from_vec("data: first\n\nid: evt-2\nevent: update\ndata: second\n\n")
            .with_metadata_kv("content-type", "text/event-stream")
            .with_metadata_kv("accept", "text/event-stream")
            .with_metadata_kv("correlation_id", "shared-stream-correlation");

    let response = publisher
        .send(request)
        .await
        .expect("stream request succeeds");
    receive_task.await.expect("receive task finished");

    let response = match response {
        Sent::Response(message) => message,
        Sent::Ack => panic!("expected streamed HTTP response body"),
    };
    let body = response.get_payload_str();
    assert!(body.contains("data: reply-first"));
    assert!(body.contains("data: reply-second"));
    assert_eq!(
        response.metadata.get("content-type").map(String::as_str),
        Some("text/event-stream")
    );
}

#[tokio::test]
async fn test_http_publisher_stream_response_to_sink() {
    init_crypto();

    let port = get_free_port();
    let bind_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept test request");
        let io = TokioIo::new(stream);
        let service = hyper::service::service_fn(|_req: Request<Incoming>| async move {
            let stream = futures::stream::iter(vec![
                Ok::<_, anyhow::Error>(Frame::data(Bytes::from_static(
                    b"id: one\ndata: alpha\n\n",
                ))),
                Ok::<_, anyhow::Error>(Frame::data(Bytes::from_static(
                    b"id: two\nevent: delta\ndata: beta\n\n",
                ))),
            ]);
            Ok::<_, anyhow::Error>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(streamed(stream))
                    .unwrap(),
            )
        });
        let builder = AutoBuilder::new(TokioExecutor::new());
        builder
            .serve_connection(io, service)
            .await
            .expect("serve test response");
    });

    let sink_endpoint = Endpoint::new_memory(
        &format!("http_stream_sink_{}", fast_uuid_v7::gen_id_str()),
        10,
    );
    let mut sink_consumer = create_consumer_from_route("http_stream_sink", &sink_endpoint)
        .await
        .expect("create stream sink consumer");

    let publisher_endpoint = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("http://{}", addr),
        stream_response_to: Some(Box::new(sink_endpoint)),
        ..Default::default()
    }));
    let publisher = create_publisher_from_route("http_stream_publisher", &publisher_endpoint)
        .await
        .expect("create http publisher");

    let sent = publisher
        .send(
            CanonicalMessage::from_vec("prompt").with_metadata_kv("correlation_id", "llm-stream-1"),
        )
        .await
        .expect("publish request");
    assert!(matches!(sent, Sent::Ack));
    server_task.abort();
    let _ = server_task.await;

    let first = sink_consumer
        .receive()
        .await
        .expect("first streamed response");
    assert_eq!(first.message.get_payload_str(), "alpha");
    assert_eq!(
        first
            .message
            .metadata
            .get("correlation_id")
            .map(String::as_str),
        Some("llm-stream-1")
    );
    assert_eq!(
        first
            .message
            .metadata
            .get("http_stream_index")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(
        first
            .message
            .metadata
            .get("http_stream_end")
            .map(String::as_str),
        Some("false")
    );
    (first.commit)(MessageDisposition::Ack).await.unwrap();

    let second = sink_consumer
        .receive()
        .await
        .expect("second streamed response");
    assert_eq!(second.message.get_payload_str(), "beta");
    assert_eq!(
        second.message.metadata.get("sse_event").map(String::as_str),
        Some("delta")
    );
    assert_eq!(
        second
            .message
            .metadata
            .get("http_stream_index")
            .map(String::as_str),
        Some("1")
    );
    (second.commit)(MessageDisposition::Ack).await.unwrap();

    let end = sink_consumer.receive().await.expect("stream end marker");
    assert!(end.message.payload.is_empty());
    assert_eq!(
        end.message
            .metadata
            .get("http_stream_end")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        end.message
            .metadata
            .get("http_stream_index")
            .map(String::as_str),
        Some("2")
    );
    (end.commit)(MessageDisposition::Ack).await.unwrap();
}

#[tokio::test]
async fn test_http_publisher_stream_response_to_stream_buffer_isolates_parallel_responses() {
    init_crypto();

    let port = get_free_port();
    let bind_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let server_task = tokio::spawn(async move {
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("accept test request");
            tasks.push(tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = hyper::service::service_fn(|req: Request<Incoming>| async move {
                    let path = req.uri().path().trim_start_matches('/').to_string();
                    let first = format!("data: {}-1\n\n", path);
                    let second = format!("data: {}-2\n\n", path);
                    let stream = futures::stream::iter(vec![
                        Ok::<_, anyhow::Error>(Frame::data(Bytes::from(first))),
                        Ok::<_, anyhow::Error>(Frame::data(Bytes::from(second))),
                    ]);
                    Ok::<_, anyhow::Error>(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(streamed(stream))
                            .unwrap(),
                    )
                });
                let builder = AutoBuilder::new(TokioExecutor::new());
                builder
                    .serve_connection(io, service)
                    .await
                    .expect("serve test response");
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    });

    let topic = format!("http_stream_buffer_parallel_{}", fast_uuid_v7::gen_id_str());
    let sink_endpoint = Endpoint::new(EndpointType::StreamBuffer(StreamBufferConfig {
        topic: topic.clone(),
        correlation_id: None,
        capacity: Some(20),
        idle_ttl_secs: None,
    }));
    let mut consumer_a = create_consumer_from_route(
        "http_stream_buffer_a",
        &Endpoint::new(EndpointType::StreamBuffer(StreamBufferConfig {
            topic: topic.clone(),
            correlation_id: Some("stream-a".to_string()),
            capacity: Some(20),
            idle_ttl_secs: None,
        })),
    )
    .await
    .expect("create stream-a consumer");
    let mut consumer_b = create_consumer_from_route(
        "http_stream_buffer_b",
        &Endpoint::new(EndpointType::StreamBuffer(StreamBufferConfig {
            topic: topic.clone(),
            correlation_id: Some("stream-b".to_string()),
            capacity: Some(20),
            idle_ttl_secs: None,
        })),
    )
    .await
    .expect("create stream-b consumer");

    let publisher_endpoint = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("http://{}", addr),
        stream_response_to: Some(Box::new(sink_endpoint)),
        ..Default::default()
    }));
    let publisher: std::sync::Arc<dyn MessagePublisher> =
        create_publisher_from_route("http_stream_buffer_publisher", &publisher_endpoint)
            .await
            .expect("create http publisher");

    let send_a = {
        let publisher = publisher.clone();
        tokio::spawn(async move {
            publisher
                .send(
                    CanonicalMessage::from_vec("prompt-a")
                        .with_metadata_kv("http_path", "/a")
                        .with_metadata_kv("correlation_id", "stream-a"),
                )
                .await
                .expect("send stream-a")
        })
    };
    let send_b = {
        let publisher = publisher.clone();
        tokio::spawn(async move {
            publisher
                .send(
                    CanonicalMessage::from_vec("prompt-b")
                        .with_metadata_kv("http_path", "/b")
                        .with_metadata_kv("correlation_id", "stream-b"),
                )
                .await
                .expect("send stream-b")
        })
    };

    assert!(matches!(send_a.await.expect("join stream-a"), Sent::Ack));
    assert!(matches!(send_b.await.expect("join stream-b"), Sent::Ack));
    server_task.abort();
    let _ = server_task.await;

    let mut stream_a_payloads = Vec::new();
    loop {
        let received = consumer_a.receive().await.expect("stream-a item");
        let is_end = received
            .message
            .metadata
            .get("http_stream_end")
            .is_some_and(|value| value == "true");
        assert_eq!(
            received
                .message
                .metadata
                .get("correlation_id")
                .map(String::as_str),
            Some("stream-a")
        );
        if !is_end {
            stream_a_payloads.push(received.message.get_payload_str().to_string());
        }
        (received.commit)(MessageDisposition::Ack).await.unwrap();
        if is_end {
            break;
        }
    }

    let mut stream_b_payloads = Vec::new();
    loop {
        let received = consumer_b.receive().await.expect("stream-b item");
        let is_end = received
            .message
            .metadata
            .get("http_stream_end")
            .is_some_and(|value| value == "true");
        assert_eq!(
            received
                .message
                .metadata
                .get("correlation_id")
                .map(String::as_str),
            Some("stream-b")
        );
        if !is_end {
            stream_b_payloads.push(received.message.get_payload_str().to_string());
        }
        (received.commit)(MessageDisposition::Ack).await.unwrap();
        if is_end {
            break;
        }
    }

    assert_eq!(stream_a_payloads, vec!["a-1", "a-2"]);
    assert_eq!(stream_b_payloads, vec!["b-1", "b-2"]);
}

#[tokio::test]
async fn test_http_publisher_stream_response_to_stream_buffer_uses_message_id_fallback() {
    init_crypto();

    let port = get_free_port();
    let bind_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept test request");
        let io = TokioIo::new(stream);
        let service = hyper::service::service_fn(|_req: Request<Incoming>| async move {
            let stream = futures::stream::iter(vec![Ok::<_, anyhow::Error>(Frame::data(
                Bytes::from_static(b"data: fallback\n\n"),
            ))]);
            Ok::<_, anyhow::Error>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(streamed(stream))
                    .unwrap(),
            )
        });
        let builder = AutoBuilder::new(TokioExecutor::new());
        builder
            .serve_connection(io, service)
            .await
            .expect("serve test response");
    });

    let topic = format!("http_stream_buffer_fallback_{}", fast_uuid_v7::gen_id_str());
    let sink_endpoint = Endpoint::new(EndpointType::StreamBuffer(StreamBufferConfig {
        topic: topic.clone(),
        correlation_id: None,
        capacity: Some(10),
        idle_ttl_secs: None,
    }));
    let publisher_endpoint = Endpoint::new(EndpointType::Http(HttpConfig {
        url: format!("http://{}", addr),
        stream_response_to: Some(Box::new(sink_endpoint)),
        ..Default::default()
    }));
    let publisher =
        create_publisher_from_route("http_stream_fallback_publisher", &publisher_endpoint)
            .await
            .expect("create http publisher");

    let request = CanonicalMessage::from_vec("prompt");
    let expected_correlation_id = format!("{:032x}", request.message_id);
    let mut consumer = create_consumer_from_route(
        "http_stream_fallback_consumer",
        &Endpoint::new(EndpointType::StreamBuffer(StreamBufferConfig {
            topic: topic.clone(),
            correlation_id: Some(expected_correlation_id.clone()),
            capacity: Some(10),
            idle_ttl_secs: None,
        })),
    )
    .await
    .expect("create fallback consumer");

    let sent = publisher.send(request).await.expect("send request");
    assert!(matches!(sent, Sent::Ack));
    server_task.abort();
    let _ = server_task.await;

    let item = consumer.receive().await.expect("fallback stream item");
    assert_eq!(item.message.get_payload_str(), "fallback");
    assert_eq!(
        item.message
            .metadata
            .get("correlation_id")
            .map(String::as_str),
        Some(expected_correlation_id.as_str())
    );
    (item.commit)(MessageDisposition::Ack).await.unwrap();

    let end = consumer.receive().await.expect("fallback end marker");
    assert_eq!(
        end.message
            .metadata
            .get("http_stream_end")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        end.message
            .metadata
            .get("correlation_id")
            .map(String::as_str),
        Some(expected_correlation_id.as_str())
    );
    (end.commit)(MessageDisposition::Ack).await.unwrap();
}

#[tokio::test]
async fn test_http_server_shutdown_on_drop() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };

    {
        let _consumer = HttpConsumer::new(&config)
            .await
            .expect("Failed to create consumer");
        assert!(tokio::net::TcpStream::connect(&addr).await.is_ok());
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
}

#[tokio::test]
async fn test_http2_only_listener_accepts_h2c_prior_knowledge() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/h2c".to_string()),
        server_protocol: HttpServerProtocol::Http2Only,
        ..Default::default()
    }));
    let output = raw_text_static_endpoint("h2c-ok");
    let handle = crate::Route::new(input, output)
        .run("test_http2_only_h2c_prior_knowledge")
        .await
        .unwrap();

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let (mut client, connection) = h2::client::handshake(stream).await.unwrap();
    let connection_task = tokio::spawn(connection);

    let request = Request::builder()
        .method("GET")
        .uri(format!("http://{addr}/h2c"))
        .body(())
        .unwrap();
    let (response, _) = client.send_request(request, true).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes, b"h2c-ok");

    connection_task.abort();
    let _ = connection_task.await;
    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http2_only_listener_rejects_plain_http11() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/h2c-only".to_string()),
        server_protocol: HttpServerProtocol::Http2Only,
        ..Default::default()
    }));
    let output = raw_text_static_endpoint("should-not-be-served-over-http1");
    let handle = crate::Route::new(input, output)
        .run("test_http2_only_rejects_http11")
        .await
        .unwrap();

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            format!("GET /h2c-only HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let _ = stream.shutdown().await;

    let response = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.map(|_| buf)
    })
    .await
    .expect("HTTP/1.1 rejection should complete promptly");

    match response {
        Ok(buf) => {
            let text = String::from_utf8_lossy(&buf);
            assert!(
                !text.contains(" 200 ")
                    && !text.starts_with("HTTP/1.1 200")
                    && !text.starts_with("HTTP/1.0 200"),
                "Http2Only listener unexpectedly returned success: {text:?}"
            );
        }
        Err(err) => {
            assert!(
                matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                ),
                "unexpected HTTP/1.1 read error: {err}"
            );
        }
    }

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_default_auto_listener_accepts_http11() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/auto-http1".to_string()),
        ..Default::default()
    }));
    let output = raw_text_static_endpoint("auto-ok");
    let handle = crate::Route::new(input, output)
        .run("test_http_default_auto_accepts_http11")
        .await
        .unwrap();

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            format!("GET /auto-http1 HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = vec![0; 1024];
    let bytes_read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .expect("HTTP/1.1 response should arrive promptly")
        .unwrap();
    let text = String::from_utf8_lossy(&response[..bytes_read]);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "default auto listener did not serve HTTP/1.1 successfully: {text:?}"
    );
    assert!(text.contains("auto-ok"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_to_static_response() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let http_config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };
    let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

    let static_content = "This is a static response";
    let static_publisher =
        crate::endpoints::structural::static_endpoint::StaticEndpointPublisher::new(
            &crate::models::StaticConfig::from(static_content),
        )
        .unwrap();

    tokio::spawn(async move {
        if let Ok(received) = consumer.receive().await {
            let static_response_outcome = static_publisher.send(received.message).await.unwrap();
            let disposition = match static_response_outcome {
                Sent::Response(msg) => crate::traits::MessageDisposition::Reply(msg),
                Sent::Ack => crate::traits::MessageDisposition::Ack,
            };
            let _ = (received.commit)(disposition).await;
        }
    });

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            format!("GET /static HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = vec![0; 1024];
    let bytes_read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .expect("static response should arrive promptly")
        .unwrap();
    let text = String::from_utf8_lossy(&response[..bytes_read]);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "static endpoint did not reply 200: {text:?}"
    );
    assert!(
        text.contains(static_content),
        "static endpoint reply missing body: {text:?}"
    );
}

#[tokio::test]
async fn test_http_to_response_endpoint() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let http_config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };
    let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

    let response_endpoint =
        crate::models::Endpoint::new(EndpointType::Response(crate::models::ResponseConfig {}));
    let publisher = create_publisher_from_route("test_response", &response_endpoint)
        .await
        .unwrap();

    tokio::spawn(async move {
        if let Ok(received) = consumer.receive().await {
            let outcome = publisher.send(received.message).await.unwrap();
            let disposition = match outcome {
                Sent::Response(msg) => crate::traits::MessageDisposition::Reply(msg),
                Sent::Ack => crate::traits::MessageDisposition::Ack,
            };
            let _ = (received.commit)(disposition).await;
        }
    });

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let body = "echo me";
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /echo HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = vec![0; 1024];
    let bytes_read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .expect("response should arrive promptly")
        .unwrap();
    let text = String::from_utf8_lossy(&response[..bytes_read]);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "response endpoint did not reply 200: {text:?}"
    );
    assert!(
        text.contains(body),
        "response endpoint did not echo the request body: {text:?}"
    );
}

#[tokio::test]
async fn test_http_route_inline_response_does_not_echo_unchanged_request_headers() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/inline".to_string()),
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));

    let route = crate::Route::new(input, output);
    let handle = route.run("test_http_inline_fast_path").await.unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/inline"))
        .header("content-type", "application/json")
        .header("accept", "application/octet-stream")
        .header("x-request-id", "req-123")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            br#"{"value":1}"#,
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // `content-type` describes the response representation and is exempt from
    // request-echo suppression: the echoed body is the JSON request body, so the
    // reply correctly carries `application/json` rather than falling back to
    // `application/octet-stream`. Arbitrary request headers such as `x-request-id`
    // are still suppressed.
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert!(response.headers().get("x-request-id").is_none());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(br#"{"value":1}"#));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_route_inline_response_can_be_disabled() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/inline-disabled".to_string()),
        inline_response_fast_path: Some(false),
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));

    let route = crate::Route::new(input, output);
    let handle = route
        .run("test_http_inline_fast_path_disabled")
        .await
        .unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/inline-disabled"))
        .header("content-type", "application/json")
        .header("accept", "application/octet-stream")
        .header("x-request-id", "req-123")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            br#"{"value":1}"#,
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(response.headers().get("x-request-id").unwrap(), "req-123");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(br#"{"value":1}"#));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_to_static_raw_sets_content_type_handler_free() {
    // The handler-free fast path: `http -> static` replies inline with a raw
    // (unquoted) body and a configured content-type header. This is the
    // TechEmpower plaintext path that bypasses any handler.
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/plaintext".to_string()),
        ..Default::default()
    }));
    let mut metadata = HashMap::new();
    metadata.insert("content-type".to_string(), "text/plain".to_string());
    let output = Endpoint::new(EndpointType::Static(crate::models::StaticConfig {
        body: "Hello, World!".to_string(),
        raw: true,
        metadata,
    }));

    let route = crate::Route::new(input, output);
    let handle = route.run("test_http_to_static_raw").await.unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::GET)
        .uri(format!("http://{addr}/plaintext"))
        .body(http_body_util::Full::<Bytes>::new(Bytes::new()))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    // Raw: no JSON quoting around the body.
    assert_eq!(body, Bytes::from_static(b"Hello, World!"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_route_handler_response_uses_inline_path() {
    use crate::traits::Handled;

    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/handler".to_string()),
        ..Default::default()
    }));
    let mut output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));
    let handler = |mut msg: CanonicalMessage| async move {
        msg.payload = Bytes::from_static(b"handled-response");
        msg.metadata
            .insert("content-type".to_string(), "text/plain".to_string());
        msg.metadata
            .insert("x-response-id".to_string(), "resp-1".to_string());
        msg.metadata
            .insert("http_status_code".to_string(), "201".to_string());
        Ok(Handled::Publish(msg))
    };
    output.handler = Some(std::sync::Arc::new(handler));

    let route = crate::Route::new(input, output);
    let handle = route.run("test_http_inline_handler_path").await.unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/handler"))
        .header("content-type", "application/json")
        .header("accept", "application/octet-stream")
        .header("x-request-id", "req-123")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            br#"{"value":1}"#,
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    assert_eq!(response.headers().get("x-response-id").unwrap(), "resp-1");
    assert!(response.headers().get("x-request-id").is_none());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"handled-response"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_route_handler_content_type_matching_request_is_not_suppressed() {
    // Regression: a handler-set reply `content-type` whose value byte-matches the
    // request's `Content-Type` must still be sent. Request-echo suppression must
    // not drop it and fall back to `application/octet-stream`.
    use crate::traits::Handled;

    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/ct-match".to_string()),
        ..Default::default()
    }));
    let mut output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));
    let handler = |mut msg: CanonicalMessage| async move {
        msg.payload = Bytes::from_static(b"42");
        msg.metadata
            .insert("content-type".to_string(), "text/plain".to_string());
        Ok(Handled::Publish(msg))
    };
    output.handler = Some(std::sync::Arc::new(handler));

    let route = crate::Route::new(input, output);
    let handle = route.run("test_http_inline_ct_match").await.unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    // The request `Content-Type` is byte-equal to the handler's reply value.
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/ct-match"))
        .header("content-type", "text/plain")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            b"20",
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"42"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_route_handler_with_buffer_uses_inline_path() {
    use crate::models::{BufferMiddleware, Middleware};
    use crate::traits::Handled;

    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/handler-buffered".to_string()),
        ..Default::default()
    }));
    let mut output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));
    output
        .middlewares
        .push(Middleware::Buffer(BufferMiddleware {
            max_messages: 16,
            max_delay_ms: 0,
        }));
    let handler = |mut msg: CanonicalMessage| async move {
        msg.payload = Bytes::from_static(b"handled-buffered");
        msg.metadata
            .insert("content-type".to_string(), "text/plain".to_string());
        Ok(Handled::Publish(msg))
    };
    output.handler = Some(std::sync::Arc::new(handler));

    let route = crate::Route::new(input, output);
    let handle = route
        .run("test_http_inline_handler_buffer_path")
        .await
        .unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/handler-buffered"))
        .header("content-type", "application/json")
        .header("accept", "application/octet-stream")
        .header("x-request-id", "req-123")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            br#"{"value":1}"#,
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    assert!(response.headers().get("x-request-id").is_none());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"handled-buffered"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_streamable_route_handler_uses_inline_path() {
    use crate::traits::Handled;

    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/handler-stream".to_string()),
        receive_streamable: true,
        ..Default::default()
    }));
    let mut output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));
    let handler = |mut msg: CanonicalMessage| async move {
        let payload = msg.get_payload_str();
        msg.set_payload_str(format!("reply-{payload}"));
        Ok(Handled::Publish(msg))
    };
    output.handler = Some(std::sync::Arc::new(handler));

    let route = crate::Route::new(input, output);
    let handle = route
        .run("test_http_inline_streamable_handler_path")
        .await
        .unwrap();

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/handler-stream"))
        .header("content-type", "application/x-ndjson")
        .header("accept", "application/x-ndjson")
        .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
            b"first\nsecond\n",
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/x-ndjson"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"reply-first\nreply-second\n"));

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_http_reply_with_custom_status_code() {
    use crate::traits::Handled;
    init_crypto();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let http_config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };
    let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

    let mut response_endpoint =
        crate::models::Endpoint::new(EndpointType::Response(crate::models::ResponseConfig {}));

    let handler = |mut msg: CanonicalMessage| async move {
        msg.metadata
            .insert("http_status_code".to_string(), "201".to_string());
        Ok(Handled::Publish(msg))
    };
    response_endpoint.handler = Some(std::sync::Arc::new(handler));

    let publisher = create_publisher_from_route("test_response_handler_status", &response_endpoint)
        .await
        .unwrap();

    tokio::spawn(async move {
        if let Ok(received) = consumer.receive().await {
            let outcome = publisher.send(received.message).await.unwrap();
            let disposition = match outcome {
                Sent::Response(msg) => crate::traits::MessageDisposition::Reply(msg),
                Sent::Ack => crate::traits::MessageDisposition::Ack,
            };
            let _ = (received.commit)(disposition).await;
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn test_http_consumers_share_listener_by_path() {
    init_crypto();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);
    let url = format!("http://{}", addr);

    let mut alpha_consumer = HttpConsumer::new(&HttpConfig {
        url: addr.clone(),
        path: Some("/alpha".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    let mut beta_consumer = HttpConsumer::new(&HttpConfig {
        url: addr.clone(),
        path: Some("/beta".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    let publisher = HttpPublisher::new(&HttpConfig {
        url,
        ..Default::default()
    })
    .await
    .unwrap();

    let alpha_task = tokio::spawn(async move {
        let received = consumer_receive_ack(&mut alpha_consumer).await;
        received.payload
    });
    let beta_task = tokio::spawn(async move {
        let received = consumer_receive_ack(&mut beta_consumer).await;
        received.payload
    });

    let mut alpha_message = CanonicalMessage::new(b"alpha".to_vec(), None);
    alpha_message
        .metadata
        .insert("http_path".to_string(), "/alpha".to_string());
    let mut beta_message = CanonicalMessage::new(b"beta".to_vec(), None);
    beta_message
        .metadata
        .insert("http_path".to_string(), "/beta".to_string());

    publisher.send(alpha_message).await.unwrap();
    publisher.send(beta_message).await.unwrap();

    assert_eq!(alpha_task.await.unwrap(), b"alpha".to_vec());
    assert_eq!(beta_task.await.unwrap(), b"beta".to_vec());
}

#[tokio::test]
async fn test_http_consumer_rejects_duplicate_path_registration() {
    init_crypto();

    let port = get_free_port();
    let addr = format!("127.0.0.1:{}", port);

    let _consumer = HttpConsumer::new(&HttpConfig {
        url: addr.clone(),
        path: Some("/shared".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    let error = HttpConsumer::new(&HttpConfig {
        url: addr,
        path: Some("/shared".to_string()),
        ..Default::default()
    })
    .await
    .err()
    .expect("duplicate registration should fail");

    assert!(
        error
            .to_string()
            .contains("Conflicting HTTP consumer registration"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_http_consumers_on_ephemeral_ports_do_not_share_listener() {
    init_crypto();

    let first_consumer = HttpConsumer::new(&HttpConfig {
        url: "127.0.0.1:0".to_string(),
        ..Default::default()
    })
    .await
    .unwrap();
    let second_consumer = HttpConsumer::new(&HttpConfig {
        url: "127.0.0.1:0".to_string(),
        ..Default::default()
    })
    .await
    .unwrap();

    let first_addr = first_consumer.bound_addr().unwrap();
    let second_addr = second_consumer.bound_addr().unwrap();

    assert_ne!(first_addr, second_addr);
    assert_ne!(first_addr.port(), 0);
    assert_ne!(second_addr.port(), 0);
}

async fn consumer_receive_ack(consumer: &mut HttpConsumer) -> CanonicalMessage {
    let received = consumer.receive().await.unwrap();
    let message = received.message.clone();
    (received.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();
    message
}

// --- Sensitive request headers -------------------------------------------------------------
// Credential headers reach the handler verbatim, under the reserved `mqb.src.` namespace that
// every publisher strips on the way out. See `SENSITIVE_HEADER_METADATA_PREFIX`.

fn basic_credentials(user_pass: &str) -> String {
    format!("Basic {}", general_purpose::STANDARD.encode(user_pass))
}

fn test_http_client(
) -> hyper_util::client::legacy::Client<HttpConnector, http_body_util::Full<Bytes>> {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector)
}

/// Sends one request carrying `headers` to a bare consumer and returns the metadata the
/// handler ends up seeing.
async fn handler_metadata_for_request_headers(headers: &[(&str, &str)]) -> HashMap<String, String> {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{port}");
    let config = HttpConfig {
        url: addr.clone(),
        ..Default::default()
    };
    let mut consumer = HttpConsumer::new(&config).await.unwrap();

    let receive_task = tokio::spawn(async move {
        let received = consumer
            .receive()
            .await
            .expect("request never reached the handler");
        let _ = (received.commit)(crate::traits::MessageDisposition::Ack).await;
        received.message
    });

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let mut request = Request::builder()
        .method(hyper::Method::POST)
        .uri(format!("http://{addr}/"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = test_http_client()
        .request(
            request
                .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
                    b"body",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    receive_task.await.unwrap().metadata
}

#[test]
fn test_sensitive_header_prefix_is_reserved_source_metadata() {
    // The design rests on this: publishers drop `mqb.src.*` when serializing metadata, so the
    // credential reaches the handler but never a sink or a response header.
    assert!(SENSITIVE_HEADER_METADATA_PREFIX
        .starts_with(crate::canonical_message::SOURCE_METADATA_PREFIX));
    assert!(crate::canonical_message::is_source_metadata_key(&format!(
        "{SENSITIVE_HEADER_METADATA_PREFIX}authorization"
    )));
}

#[tokio::test]
async fn test_sensitive_request_headers_reach_handler_verbatim() {
    let metadata = handler_metadata_for_request_headers(&[
        ("authorization", "Basic dXNlcjpwYXNzd29yZA=="),
        ("cookie", "session=abc123"),
        ("x-api-key", "super-secret-key"),
        ("x-trace-id", "trace-42"),
    ])
    .await;

    // The handler gets what the client actually sent, so "is the client authenticating, and
    // with what" is answerable while debugging.
    assert_eq!(
        metadata
            .get("mqb.src.http_authorization")
            .map(String::as_str),
        Some("Basic dXNlcjpwYXNzd29yZA==")
    );
    assert_eq!(
        metadata.get("mqb.src.http_cookie").map(String::as_str),
        Some("session=abc123")
    );
    assert_eq!(
        metadata.get("mqb.src.http_x-api-key").map(String::as_str),
        Some("super-secret-key")
    );

    // The plain names stay absent, so nothing reading `metadata["authorization"]` today
    // suddenly starts seeing a credential, and no publisher can forward one.
    assert!(!metadata.contains_key("authorization"));
    assert!(!metadata.contains_key("cookie"));
    assert!(!metadata.contains_key("x-api-key"));

    // Ordinary headers are untouched.
    assert_eq!(
        metadata.get("x-trace-id").map(String::as_str),
        Some("trace-42")
    );

    // Every key holding a credential is reserved, hence dropped by each publisher.
    for (key, value) in &metadata {
        if value.contains("dXNlcjpwYXNzd29yZA==")
            || value.contains("abc123")
            || value.contains("super-secret-key")
        {
            assert!(
                crate::canonical_message::is_source_metadata_key(key),
                "credential sits in forwardable metadata key {key:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_spoofed_source_metadata_request_headers_are_dropped() {
    // A client must not be able to forge the reserved namespace and have the handler show
    // its value as if it were the real request header.
    let metadata = handler_metadata_for_request_headers(&[
        ("mqb.src.http_authorization", "Basic injected"),
        ("mqb.src.kafka_offset", "999"),
        ("x-trace-id", "trace-42"),
    ])
    .await;

    assert!(
        !metadata.keys().any(|key| key.starts_with("mqb.src.")),
        "spoofed source metadata survived: {metadata:?}"
    );
    assert_eq!(
        metadata.get("x-trace-id").map(String::as_str),
        Some("trace-42")
    );
}

#[tokio::test]
async fn test_sensitive_header_metadata_is_not_echoed_on_the_reply() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{port}");

    // The routed path applies no request-echo suppression (see
    // `test_http_route_inline_response_can_be_disabled`), so it is where a reserved-prefix key
    // would surface as a response header if it were forwardable.
    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/echo".to_string()),
        inline_response_fast_path: Some(false),
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::Response(
        crate::models::ResponseConfig::default(),
    ));
    let handle = crate::Route::new(input, output)
        .run("test_http_sensitive_header_reply")
        .await
        .unwrap();

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let response = test_http_client()
        .request(
            Request::builder()
                .method(hyper::Method::POST)
                .uri(format!("http://{addr}/echo"))
                .header("authorization", "Basic dXNlcjpwYXNzd29yZA==")
                .header("cookie", "session=abc123")
                .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
                    b"echo me",
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let headers: String = response
        .headers()
        .iter()
        .map(|(name, value)| format!("{name}: {}\n", value.to_str().unwrap_or("")))
        .collect();
    assert!(
        !headers.contains("mqb.src."),
        "reserved metadata leaked as a response header: {headers}"
    );
    assert!(
        !headers.contains("dXNlcjpwYXNzd29yZA==") && !headers.contains("abc123"),
        "credential echoed on the reply: {headers}"
    );
    assert!(response.headers().get("authorization").is_none());
    assert!(response.headers().get("cookie").is_none());

    handle.stop().await;
    let _ = handle.join().await;
}

#[tokio::test]
async fn test_basic_auth_enforcement_is_unaffected_by_header_capture() {
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{port}");
    let config = HttpConfig {
        url: addr.clone(),
        basic_auth: Some(("user".to_string(), "password".to_string())),
        ..Default::default()
    };
    let mut consumer = HttpConsumer::new(&config).await.unwrap();

    let receive_task = tokio::spawn(async move {
        let received = consumer
            .receive()
            .await
            .expect("authorized request never reached the handler");
        let _ = (received.commit)(crate::traits::MessageDisposition::Ack).await;
        received.message
    });

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);
    let client = test_http_client();

    let build = |auth: Option<String>| {
        let mut request = Request::builder()
            .method(hyper::Method::POST)
            .uri(format!("http://{addr}/"));
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        request
            .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
                b"body",
            )))
            .unwrap()
    };

    // Enforcement reads the request headers directly, before metadata is built.
    let missing = client.request(build(None)).await.unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let wrong = client
        .request(build(Some(basic_credentials("user:wrong"))))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let accepted = client
        .request(build(Some(basic_credentials("user:password"))))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);

    let metadata = receive_task.await.unwrap().metadata;
    assert_eq!(
        metadata
            .get("mqb.src.http_authorization")
            .map(String::as_str),
        Some(basic_credentials("user:password").as_str())
    );
}

#[tokio::test]
async fn test_sensitive_header_metadata_is_not_written_to_a_sink() {
    // End-to-end proof of the forwarding half: the `json` file sink serialises the whole
    // metadata map, so if the reserved prefix were forwardable the credential would land on
    // disk verbatim. Covers the same `strip_source_metadata` contract every other sink uses.
    init_crypto();
    let port = get_free_port();
    let addr = format!("127.0.0.1:{port}");
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("sink.jsonl");

    let input = Endpoint::new(EndpointType::Http(HttpConfig {
        url: addr.clone(),
        path: Some("/ingest".to_string()),
        fire_and_forget: true,
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::File(crate::models::FileConfig {
        path: out_path.to_string_lossy().to_string(),
        format: crate::models::FileFormat::Json,
        ..Default::default()
    }));
    let handle = crate::Route::new(input, output)
        .run("test_http_sensitive_header_sink")
        .await
        .unwrap();

    assert!(wait_for_server_ready(&addr, Duration::from_secs(5)).await);

    let response = test_http_client()
        .request(
            Request::builder()
                .method(hyper::Method::POST)
                .uri(format!("http://{addr}/ingest"))
                .header("authorization", "Basic dXNlcjpwYXNzd29yZA==")
                .header("cookie", "session=abc123")
                .header("x-api-key", "super-secret-key")
                .header("x-trace-id", "trace-42")
                .body(http_body_util::Full::<Bytes>::new(Bytes::from_static(
                    b"{\"v\":1}",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let mut written = String::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        written = std::fs::read_to_string(&out_path).unwrap_or_default();
        if written.contains("trace-42") {
            break;
        }
    }
    assert!(
        written.contains("trace-42"),
        "route never reached the file sink: {written:?}"
    );

    // The ordinary header survives the hop; every credential-bearing one is gone.
    assert!(
        !written.contains("mqb.src."),
        "reserved metadata written to sink: {written}"
    );
    for secret in [
        "dXNlcjpwYXNzd29yZA==",
        "abc123",
        "super-secret-key",
        "authorization",
        "cookie",
        "x-api-key",
    ] {
        assert!(
            !written.contains(secret),
            "{secret:?} leaked to the file sink: {written}"
        );
    }

    handle.stop().await;
    let _ = handle.join().await;
}
