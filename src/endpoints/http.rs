//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::endpoints::create_publisher;
use crate::models::HttpConfig;
use crate::traits::{
    BoxFuture, CommitFunc, ConsumerError, MessageConsumer, MessagePublisher, PublisherError,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{header::HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use axum_server::{tls_rustls::RustlsConfig, Handle};
use std::any::Any;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, instrument, trace};

type HttpSourceMessage = (CanonicalMessage, CommitFunc);

/// A source that listens for incoming HTTP requests.
pub struct HttpConsumer {
    request_rx: mpsc::Receiver<HttpSourceMessage>,
    _shutdown_tx: watch::Sender<()>,
}

#[derive(Clone)]
struct HttpConsumerState {
    tx: mpsc::Sender<HttpSourceMessage>,
    response_sink: Option<Arc<dyn MessagePublisher>>,
}

impl HttpConsumer {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<HttpSourceMessage>(100);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());

        let response_sink = if let Some(endpoint) = &config.response_out {
            Some(create_publisher("http_response_sink", endpoint).await?)
        } else {
            None
        };

        let state = HttpConsumerState {
            tx: request_tx,
            response_sink,
        };

        let app = Router::new()
            .route("/", post(handle_request))
            .with_state(state);

        let listen_address = config
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("'url' is required for http source connection"))?;
        let addr: SocketAddr = listen_address
            .parse()
            .with_context(|| format!("Invalid listen address: {}", listen_address))?;

        let tls_config = config.tls.clone();
        let handle = Handle::new();
        // Channel to signal when the server is ready
        let (ready_tx, ready_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            if tls_config.is_tls_server_configured() {
                info!("Starting HTTPS source on {}", addr);

                // We clone the paths to move them into the async block.
                let cert_path = tls_config.cert_file.unwrap();
                let key_path = tls_config.key_file.unwrap();

                let tls_config = RustlsConfig::from_pem_file(cert_path, key_path)
                    .await
                    .unwrap();

                // Signal that we are about to start serving
                let _ = ready_tx.send(());

                let shutdown_handle = handle.clone();
                let mut shutdown_rx_clone = shutdown_rx.clone();
                tokio::spawn(async move {
                    let _ = shutdown_rx_clone.changed().await;
                    shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
                });

                axum_server::bind_rustls(addr, tls_config)
                    .handle(handle)
                    .serve(app.into_make_service())
                    .await
                    .unwrap();
            } else {
                let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
                info!("Starting HTTP source on {}", listener.local_addr().unwrap());

                // Signal that we are about to start serving
                let _ = ready_tx.send(());

                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.changed().await;
                    })
                    .await
                    .unwrap();
            }
        });

        ready_rx.await?;
        Ok(Self {
            request_rx,
            _shutdown_tx: shutdown_tx,
        })
    }
}

#[async_trait]
impl MessageConsumer for HttpConsumer {
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let (message, commit) = self
            .request_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("HTTP source channel closed"))?;

        Ok(ReceivedBatch {
            messages: vec![message],
            commit: crate::traits::into_batch_commit_func(commit),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[instrument(skip_all, fields(http.method = "POST", http.uri = "/"))]
async fn handle_request(
    State(state): State<HttpConsumerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut message = CanonicalMessage::new(body.to_vec(), None);
    trace!(message_id = %format!("{:032x}", message.message_id), "Received HTTP request");
    let mut metadata = HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            metadata.insert(key.as_str().to_string(), value_str.to_string());
        }
    }
    message.metadata = metadata;

    let message_for_sink = if state.response_sink.is_some() {
        Some(message.clone())
    } else {
        None
    };

    // Channel to receive the commit confirmation from the pipeline
    let (ack_tx, ack_rx) = oneshot::channel::<Option<CanonicalMessage>>();
    let commit = Box::new(move |resp| {
        Box::pin(async move {
            let _ = ack_tx.send(resp);
        }) as BoxFuture<'static, ()>
    });

    if state.tx.send((message, commit)).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to send request to bridge",
        )
            .into_response();
    }

    // Wait for pipeline to process the message
    let timeout_duration = Duration::from_secs(30);
    match tokio::time::timeout(timeout_duration, async {
        match ack_rx.await {
            Ok(pipeline_response) => {
                // Pipeline processed the message.
                // If a response sink is configured, use it to generate the response.
                if let Some(sink) = &state.response_sink {
                    match sink.send(message_for_sink.unwrap()).await {
                        Ok(Sent::Response(sink_response)) => make_response(Some(sink_response)),
                        Ok(Sent::Ack) => make_response(None),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Response sink error: {}", e),
                        )
                            .into_response(),
                    }
                } else {
                    // No sink configured, use the pipeline response (if any)
                    make_response(pipeline_response)
                }
            }
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Pipeline closed").into_response(),
        }
    })
    .await
    {
        Ok(response) => response,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "Request timed out").into_response(),
    }
}

fn make_response(message: Option<CanonicalMessage>) -> Response {
    match message {
        Some(msg) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                msg.metadata
                    .get("content-type")
                    .map(|s| s.as_str())
                    .unwrap_or("application/json"),
            )],
            msg.payload,
        )
            .into_response(),
        None => (StatusCode::ACCEPTED, "Message processed").into_response(),
    }
}

/// A sink that sends messages to an HTTP endpoint.
#[derive(Clone)]
pub struct HttpPublisher {
    client: reqwest::Client,
    url: String,
    response_out: Option<Arc<dyn MessagePublisher>>,
}

impl HttpPublisher {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        let mut client_builder = reqwest::Client::builder();

        if config.tls.is_mtls_client_configured() {
            let cert_path = config.tls.cert_file.as_ref().unwrap();
            let key_path = config.tls.key_file.as_ref().unwrap();
            let cert = tokio::fs::read(cert_path).await?;
            let key = tokio::fs::read(key_path).await?;
            let identity = reqwest::Identity::from_pem(&[cert, key].concat())?;
            client_builder = client_builder.identity(identity);
        }

        let response_sink = if let Some(endpoint) = &config.response_out {
            Some(Box::pin(create_publisher("http_response_sink", endpoint)).await?)
        } else {
            None
        };

        Ok(Self {
            client: client_builder.build()?,
            url: config.url.clone().unwrap_or_default(),
            response_out: response_sink,
        })
    }

    pub fn with_url(&self, url: &str) -> Self {
        Self {
            client: self.client.clone(),
            url: url.to_string(),
            response_out: self.response_out.clone(),
        }
    }
}

#[async_trait]
impl MessagePublisher for HttpPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            message_id = %format!("{:032x}", message.message_id),
            url = %self.url,
            "Sending HTTP request"
        );
        let mut request_builder = self.client.post(&self.url);
        for (key, value) in &message.metadata {
            request_builder = request_builder.header(key, value);
        }

        let response = request_builder
            .body(message.payload)
            .send()
            .await
            .with_context(|| format!("Failed to send HTTP request to {}", self.url))?;

        let response_status = response.status();
        let mut response_metadata = HashMap::new();
        for (key, value) in response.headers() {
            if let Ok(value_str) = value.to_str() {
                response_metadata.insert(key.as_str().to_string(), value_str.to_string());
            }
        }

        let response_bytes = response
            .bytes()
            .await
            .context("Failed to read HTTP response body")?
            .to_vec();

        if !response_status.is_success() {
            return Err(anyhow::anyhow!(
                "HTTP sink request failed with status {}: {:?}",
                response_status,
                String::from_utf8_lossy(&response_bytes)
            )
            .into());
        }

        // If a response sink is configured, wrap the response in a CanonicalMessage
        if let Some(sink) = &self.response_out {
            let mut response_message =
                CanonicalMessage::new(response_bytes, Some(message.message_id));
            if !response_metadata.is_empty() {
                response_message.metadata = response_metadata;
            }
            sink.send(response_message).await?;
            Ok(Sent::Ack)
        } else {
            let mut response_message =
                CanonicalMessage::new(response_bytes, Some(message.message_id));
            response_message.metadata = response_metadata;
            Ok(Sent::Response(response_message))
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        use futures::future::join_all;

        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        let send_futures = messages.into_iter().map(|message| {
            // Clone the message for the error case.
            let msg_for_err = message.clone();
            async move { self.send(message).await.map_err(|e| (msg_for_err, e)) }
        });

        let results = join_all(send_futures).await;

        let mut responses = Vec::new();
        let mut failed = Vec::new();

        for result in results {
            match result {
                Ok(Sent::Response(resp)) => responses.push(resp),
                Ok(Sent::Ack) => {}
                Err((msg, e)) => failed.push((msg, e)),
            }
        }

        if failed.is_empty() && responses.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: if responses.is_empty() {
                    None
                } else {
                    Some(responses)
                },
                failed,
            })
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Config, EndpointType, MemoryConfig};
    use crate::CanonicalMessage;
    use std::time::Duration;

    // Helper to find a free port
    fn get_free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn test_http_config_yaml() {
        let yaml = r#"
http_route:
  input:
    http:
      url: "127.0.0.1:8080"
      response_out:
        memory:
          topic: "response_topic"
  output:
    http:
      url: "http://localhost:9090"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
        let route = config.get("http_route").expect("Route not found");

        match &route.input.endpoint_type {
            EndpointType::Http(cfg) => {
                assert_eq!(cfg.config.url, Some("127.0.0.1:8080".to_string()));
                assert!(cfg.config.response_out.is_some());
                if let Some(sink) = &cfg.config.response_out {
                    match &sink.endpoint_type {
                        EndpointType::Memory(mem) => assert_eq!(mem.topic, "response_topic"),
                        _ => panic!("Expected memory endpoint for sink"),
                    }
                }
            }
            _ => panic!("Expected HTTP input"),
        }

        match &route.output.endpoint_type {
            EndpointType::Http(cfg) => {
                assert_eq!(cfg.config.url, Some("http://localhost:9090".to_string()));
            }
            _ => panic!("Expected HTTP output"),
        }
    }

    #[tokio::test]
    async fn test_http_consumer_publisher_integration() {
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let url = format!("http://{}", addr);

        let config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_out: None,
        };

        // Start Consumer (Server)
        let mut consumer = HttpConsumer::new(&config)
            .await
            .expect("Failed to create consumer");

        // Start Publisher (Client)
        let pub_config = HttpConfig {
            url: Some(url.clone()),
            tls: Default::default(),
            response_out: None,
        };
        let publisher = HttpPublisher::new(&pub_config)
            .await
            .expect("Failed to create publisher");

        // Send message
        let msg_payload = b"test_payload".to_vec();
        let msg = CanonicalMessage::new(msg_payload.clone(), None);

        // Spawn a task to handle the receiving side
        let receive_task = tokio::spawn(async move {
            let received = consumer.receive().await.expect("Failed to receive");
            // Send a response back via commit
            let response_msg = CanonicalMessage::new(b"response_payload".to_vec(), None);
            (received.commit)(Some(response_msg)).await;
            received.message
        });

        // Publisher sends
        let response = publisher.send(msg).await.expect("Failed to send");

        let received_msg = receive_task.await.expect("Receive task failed");
        assert_eq!(received_msg.payload, msg_payload);
        let response = match response {
            Sent::Response(msg) => msg,
            _ => panic!("Expected response"),
        };
        assert_eq!(response.payload, b"response_payload".to_vec());
    }

    #[tokio::test]
    async fn test_http_request_reply_with_sink() {
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let url = format!("http://{}", addr);

        let config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_out: None,
        };
        let mut consumer = HttpConsumer::new(&config)
            .await
            .expect("Failed to create consumer");

        let mem_config = MemoryConfig {
            topic: "reply_sink".to_string(),
            capacity: Some(10),
        };
        let sink_endpoint = crate::models::Endpoint::new(EndpointType::Memory(mem_config.clone()));

        let pub_config = HttpConfig {
            url: Some(url.clone()),
            tls: Default::default(),
            response_out: Some(Box::new(sink_endpoint)),
        };
        let publisher = HttpPublisher::new(&pub_config)
            .await
            .expect("Failed to create publisher");

        tokio::spawn(async move {
            loop {
                if let Ok(received) = consumer.receive().await {
                    let response_msg = CanonicalMessage::new(b"server_reply".to_vec(), None);
                    (received.commit)(Some(response_msg)).await;
                }
            }
        });

        let msg = CanonicalMessage::new(b"request".to_vec(), None);
        publisher.send(msg).await.expect("Failed to send");

        let channel = crate::endpoints::memory::get_or_create_channel(&mem_config);
        let mut attempts = 0;
        while channel.is_empty() && attempts < 20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            attempts += 1;
        }

        let responses = channel.drain_messages();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].payload.to_vec(), b"server_reply".to_vec());
    }

    #[tokio::test]
    async fn test_http_server_shutdown_on_drop() {
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_out: None,
        };

        {
            let _consumer = HttpConsumer::new(&config)
                .await
                .expect("Failed to create consumer");
            // Verify we can connect while consumer is alive
            assert!(tokio::net::TcpStream::connect(&addr).await.is_ok());
        } // consumer is dropped here, triggering shutdown via _shutdown_tx drop

        // Wait for shutdown to propagate
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify connection is refused (server is down)
        assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
    }

    #[tokio::test]
    async fn test_http_to_static_response() {
        // This test simulates a route: HTTP In -> Static Out.
        // It verifies that an HTTP request receives the static response.

        // 1. Setup an HttpConsumer (server)
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let http_config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_out: None,
        };
        let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

        // 2. Setup a StaticEndpointPublisher
        let static_content = "This is a static response";
        let static_publisher =
            crate::endpoints::static_endpoint::StaticEndpointPublisher::new(static_content)
                .unwrap();

        // 3. Emulate the route logic in a separate task
        tokio::spawn(async move {
            if let Ok(received) = consumer.receive().await {
                let static_response_outcome =
                    static_publisher.send(received.message).await.unwrap();
                let pipeline_response = match static_response_outcome {
                    Sent::Response(msg) => Some(msg),
                    Sent::Ack => None,
                };
                (received.commit)(pipeline_response).await;
            }
        });

        // 4. Make a request to the server
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{}", addr))
            .send()
            .await
            .unwrap();

        // 5. Assert the response from the server
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        let expected_body = serde_json::to_string(static_content).unwrap();
        assert_eq!(body, expected_body);
    }

    #[tokio::test]
    async fn test_http_to_response_endpoint() {
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let http_config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_out: None,
        };
        let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

        // Create ResponsePublisher via factory to simulate route config
        let response_endpoint =
            crate::models::Endpoint::new(EndpointType::Response(crate::models::ResponseConfig {}));
        let publisher = create_publisher("test_response", &response_endpoint)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Ok(received) = consumer.receive().await {
                let outcome = publisher.send(received.message).await.unwrap();
                let resp = match outcome {
                    Sent::Response(msg) => Some(msg),
                    Sent::Ack => None,
                };
                (received.commit)(resp).await;
            }
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}", addr))
            .body("echo_test")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "echo_test");
    }
}
