//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
use crate::models::HttpConfig;
use crate::traits::{BatchCommitFunc, BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
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
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, instrument, warn};

type HttpSourceMessage = (CanonicalMessage, CommitFunc);

/// A source that listens for incoming HTTP requests.
pub struct HttpConsumer {
    request_rx: mpsc::Receiver<HttpSourceMessage>,
    _shutdown_tx: watch::Sender<()>,
}

#[derive(Clone)]
struct HttpConsumerState {
    tx: mpsc::Sender<HttpSourceMessage>,
    pending_requests: Option<
        Arc<Mutex<HashMap<u128, oneshot::Sender<anyhow::Result<Option<CanonicalMessage>>>>>>,
    >,
}

impl HttpConsumer {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<HttpSourceMessage>(100);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());

        let pending_requests = if let Some(endpoint) = &config.response_sink {
            let pending = Arc::new(Mutex::new(HashMap::<u128, oneshot::Sender<anyhow::Result<Option<CanonicalMessage>>>>::new()));
            let pending_clone = pending.clone();
            let mut consumer = Box::pin(create_consumer_from_route("http_response_sink", endpoint)).await?;
            let mut shutdown_rx_clone = shutdown_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_rx_clone.changed() => {
                            info!("HTTP response sink consumer shutting down.");
                            break;
                        }
                        result = consumer.receive() => {
                            match result {
                                Ok((msg, commit)) => {
                                    if let Some(id) = msg.message_id {
                                        let sender = {
                                            let mut map = pending_clone.lock().unwrap();
                                            map.remove(&id)
                                        };
                                        if let Some(tx) = sender {
                                            let _ = tx.send(Ok(Some(msg)));
                                        } else {
                                            warn!(message_id = %id, "Received response for unknown/timed-out request in HTTP sink");
                                        }
                                    }
                                    commit(None).await;
                                }
                                Err(_) => {
                                    // Consumer stream ended, exit loop
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            Some(pending)
        } else {
            None
        };

        let state = HttpConsumerState {
            tx: request_tx,
            pending_requests,
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
        Ok(Self { request_rx, _shutdown_tx: shutdown_tx })
    }
}

#[async_trait]
impl MessageConsumer for HttpConsumer {
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (message, commit) = self
            .request_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("HTTP source channel closed"))?;

        Ok((vec![message], crate::traits::into_batch_commit_func(commit)))
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
    let (final_tx, final_rx) = oneshot::channel();
    let mut message = CanonicalMessage::new(body.to_vec(), None);

    let mut metadata = HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            metadata.insert(key.as_str().to_string(), value_str.to_string());
        }
    }
    message.metadata = Some(metadata);

    if let Some(pending) = &state.pending_requests {
        if message.message_id.is_none() {
            message.gen_id();
        }
        let id = message.message_id.unwrap();
        {
            let mut map = pending.lock().unwrap();
            map.insert(id, final_tx);
        }

        // Send to pipeline with a dummy ack channel, as we wait for the sink response
        let (ack_tx, ack_rx) = oneshot::channel::<anyhow::Result<Option<CanonicalMessage>>>();
        let commit = Box::new(move |resp| {
            Box::pin(async move {
                let _ = ack_tx.send(Ok(resp));
            }) as BoxFuture<'static, ()>
        });

        if state.tx.send((message, commit)).await.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to send request to bridge",
            )
                .into_response();
        }

        // Wait for pipeline acceptance
        match ack_rx.await {
            Ok(Err(e)) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Pipeline error: {}", e),
                )
                    .into_response()
            }
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Pipeline closed").into_response()
            }
            Ok(Ok(_)) => {} // Pipeline accepted, continue waiting for sink response
        }
    } else {
        let commit = Box::new(move |resp| {
            Box::pin(async move {
                let _ = final_tx.send(Ok(resp));
            }) as BoxFuture<'static, ()>
        });

        if state.tx.send((message, commit)).await.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to send request to bridge",
            )
                .into_response();
        }
    }

    match final_rx.await {
        Ok(Ok(Some(response_message))) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                response_message
                    .metadata
                    .as_ref()
                    .as_ref()
                    .and_then(|m| m.get("content-type"))
                    .map(|s| s.as_str())
                    .unwrap_or("application/json"),
            )],
            response_message.payload,
        )
            .into_response(),
        Ok(Ok(None)) => (StatusCode::ACCEPTED, "Message processed").into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error processing message: {}", e),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to receive response from bridge",
        )
            .into_response(),
    }
}

/// A sink that sends messages to an HTTP endpoint.
#[derive(Clone)]
pub struct HttpPublisher {
    client: reqwest::Client,
    url: String,
    response_sink: Option<Arc<dyn MessagePublisher>>,
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

        let response_sink = if let Some(endpoint) = &config.response_sink {
            Some(Box::pin(create_publisher_from_route("http_response_sink", endpoint)).await?)
        } else {
            None
        };

        Ok(Self {
            client: client_builder.build()?,
            url: config.url.clone().unwrap_or_default(),
            response_sink,
        })
    }

    pub fn with_url(&self, url: &str) -> Self {
        Self {
            client: self.client.clone(),
            url: url.to_string(),
            response_sink: self.response_sink.clone(),
        }
    }
}

#[async_trait]
impl MessagePublisher for HttpPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let mut request_builder = self.client.post(&self.url);
        if let Some(metadata) = &message.metadata {
            for (key, value) in metadata {
                request_builder = request_builder.header(key, value);
            }
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

        let response_bytes = response.bytes().await?.to_vec();

        if !response_status.is_success() {
            return Err(anyhow!(
                "HTTP sink request failed with status {}: {:?}",
                response_status,
                String::from_utf8_lossy(&response_bytes)
            ));
        }

        // If a response sink is configured, wrap the response in a CanonicalMessage
        if let Some(sink) = &self.response_sink {
            let mut response_message = CanonicalMessage::new(response_bytes, message.message_id);
            if !response_metadata.is_empty() {
                response_message.metadata = Some(response_metadata);
            }
            sink.send(response_message).await?;
            Ok(None)
        } else {
            Ok(None)
        }
    }

    // not a real bulk, but fast enough
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
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
      response_sink:
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
                assert!(cfg.config.response_sink.is_some());
                if let Some(sink) = &cfg.config.response_sink {
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
            response_sink: None,
        };

        // Start Consumer (Server)
        let mut consumer = HttpConsumer::new(&config).await.expect("Failed to create consumer");

        // Start Publisher (Client)
        let pub_config = HttpConfig {
            url: Some(url.clone()),
            tls: Default::default(),
            response_sink: None,
        };
        let publisher = HttpPublisher::new(&pub_config).await.expect("Failed to create publisher");

        // Send message
        let msg_payload = b"test_payload".to_vec();
        let msg = CanonicalMessage::new(msg_payload.clone(), None);

        // Spawn a task to handle the receiving side
        let receive_task = tokio::spawn(async move {
            let (received_msg, commit) = consumer.receive().await.expect("Failed to receive");
            // Send a response back via commit
            let response_msg = CanonicalMessage::new(b"response_payload".to_vec(), None);
            commit(Some(response_msg)).await;
            received_msg
        });

        // Publisher sends
        let response = publisher.send(msg).await.expect("Failed to send");

        let received_msg = receive_task.await.expect("Receive task failed");
        assert_eq!(received_msg.payload, msg_payload);
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn test_http_request_reply_with_sink() {
        let port = get_free_port();
        let addr = format!("127.0.0.1:{}", port);
        let url = format!("http://{}", addr);

        let config = HttpConfig {
            url: Some(addr.clone()),
            tls: Default::default(),
            response_sink: None,
        };
        let mut consumer = HttpConsumer::new(&config).await.expect("Failed to create consumer");

        let mem_config = MemoryConfig {
            topic: "reply_sink".to_string(),
            capacity: Some(10),
        };
        let sink_endpoint = crate::models::Endpoint::new(EndpointType::Memory(mem_config.clone()));

        let pub_config = HttpConfig {
            url: Some(url.clone()),
            tls: Default::default(),
            response_sink: Some(Box::new(sink_endpoint)),
        };
        let publisher = HttpPublisher::new(&pub_config).await.expect("Failed to create publisher");

        tokio::spawn(async move {
            loop {
                if let Ok((_, commit)) = consumer.receive().await {
                    let response_msg = CanonicalMessage::new(b"server_reply".to_vec(), None);
                    commit(Some(response_msg)).await;
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
            response_sink: None,
        };

        {
            let _consumer = HttpConsumer::new(&config).await.expect("Failed to create consumer");
            // Verify we can connect while consumer is alive
            assert!(tokio::net::TcpStream::connect(&addr).await.is_ok());
        } // consumer is dropped here, triggering shutdown via _shutdown_tx drop

        // Wait for shutdown to propagate
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify connection is refused (server is down)
        assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
    }
}