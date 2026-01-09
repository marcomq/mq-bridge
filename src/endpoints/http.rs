//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::create_publisher_from_route;
use crate::models::HttpConfig;
#[cfg(feature = "actix-web")]
use crate::traits::CommitFunc;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, ReceivedBatch, Sent,
};
#[cfg(feature = "reqwest")]
use crate::traits::{PublisherError, SentBatch};
use crate::CanonicalMessage;
#[cfg(feature = "actix-web")]
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
#[cfg(feature = "actix-web")]
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, trace};
use uuid::Uuid;

#[cfg(feature = "actix-web")]
type HttpSourceMessage = (CanonicalMessage, CommitFunc);

/// A source that listens for incoming HTTP requests.
#[cfg(feature = "actix-web")]
pub struct HttpConsumer {
    request_rx: tokio::sync::mpsc::Receiver<HttpSourceMessage>,
    _shutdown_tx: tokio::sync::watch::Sender<()>,
    _server_handle: actix_web::dev::ServerHandle,
}

#[cfg(feature = "actix-web")]
#[derive(Clone)]
struct HttpConsumerState {
    tx: tokio::sync::mpsc::Sender<HttpSourceMessage>,
    response_sink: Option<Arc<dyn MessagePublisher>>,
    message_id_header: String,
}

#[cfg(feature = "actix-web")]
impl HttpConsumer {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = tokio::sync::mpsc::channel::<HttpSourceMessage>(100);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());

        let response_sink = if let Some(endpoint) = &config.response_out {
            Some(create_publisher_from_route("http_response_sink", endpoint).await?)
        } else {
            None
        };

        let message_id_header = config
            .message_id_header
            .clone()
            .unwrap_or_else(|| "message-id".to_string());
        let state = HttpConsumerState {
            tx: request_tx,
            response_sink,
            message_id_header,
        };

        let listen_address = config
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("'url' is required for http source connection"))?;
        let addr: SocketAddr = listen_address
            .parse()
            .with_context(|| format!("Invalid listen address: {}", listen_address))?;

        let tls_config = config.tls.clone();
        // Channel to signal when the server is ready
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

        let workers = config.workers.unwrap_or(0);
        let workers = if workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            workers
        };
        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                // actual request handle here:
                .route("/", web::post().to(handle_request))
        })
        .workers(workers)
        .disable_signals(); // We handle shutdown manually

        let server = if tls_config.is_tls_server_configured() {
            info!("Starting HTTPS source on {} with {} workers", addr, workers);
            let config = load_rustls_config(&tls_config)?;
            server.bind_rustls_0_23(addr, config)?
        } else {
            info!("Starting HTTP source on {} with {} workers", addr, workers);
            server.bind(addr)?
        };

        let server = server.run();
        let handle = server.handle();

        tokio::spawn(async move {
            // Signal that we are about to start serving
            let _ = ready_tx.send(());
            if let Err(e) = server.await {
                tracing::error!("HTTP server error: {}", e);
            }
        });

        // Spawn shutdown handler
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.changed().await;
            shutdown_handle.stop(true).await;
        });

        ready_rx.await?;
        Ok(Self {
            request_rx,
            _shutdown_tx: shutdown_tx,
            _server_handle: handle,
        })
    }
}

#[cfg(feature = "actix-web")]
fn load_rustls_config(
    tls_config: &crate::models::TlsConfig,
) -> anyhow::Result<rustls::ServerConfig> {
    let cert_file = tls_config
        .cert_file
        .as_ref()
        .ok_or_else(|| anyhow!("Missing cert_file"))?;
    let key_file = tls_config
        .key_file
        .as_ref()
        .ok_or_else(|| anyhow!("Missing key_file"))?;

    let cert_file = std::fs::File::open(cert_file)?;
    let mut cert_reader = std::io::BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    let key_file = std::fs::File::open(key_file)?;
    let mut key_reader = std::io::BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow!("No private key found"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(config)
}

#[cfg(feature = "actix-web")]
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

#[cfg(feature = "actix-web")]
#[tracing::instrument(skip_all, fields(http.method = "POST", http.uri = "/"))]
async fn handle_request(
    state: web::Data<HttpConsumerState>,
    req: HttpRequest,
    body: web::Bytes,
) -> impl Responder {
    let mut message_id = None;
    if let Some(header_value) = req.headers().get(state.message_id_header.as_str()) {
        if let Ok(s) = header_value.to_str() {
            if let Ok(uuid) = Uuid::parse_str(s) {
                message_id = Some(uuid.as_u128());
            } else if let Ok(n) = u128::from_str_radix(s.trim_start_matches("0x"), 16) {
                message_id = Some(n);
            } else if let Ok(n) = s.parse::<u128>() {
                message_id = Some(n);
            }
        }
    }

    let mut message = CanonicalMessage::new(body.to_vec(), message_id);
    trace!(message_id = %format!("{:032x}", message.message_id), "Received HTTP request");
    let mut metadata = HashMap::new();
    for (key, value) in req.headers().iter() {
        if let Ok(value_str) = value.to_str() {
            metadata.insert(key.as_str().to_string(), value_str.to_string());
        }
    }
    message.metadata = metadata;

    // Channel to receive the commit confirmation from the pipeline
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<Option<CanonicalMessage>>();
    let commit = Box::new(move |resp| {
        Box::pin(async move {
            let _ = ack_tx.send(resp);
        }) as BoxFuture<'static, ()>
    });

    if state.tx.send((message, commit)).await.is_err() {
        return HttpResponse::InternalServerError().body("Failed to send request to bridge");
    }

    // Wait for pipeline to process the message
    let timeout_duration = std::time::Duration::from_secs(30);
    match tokio::time::timeout(timeout_duration, async {
        match ack_rx.await {
            Ok(pipeline_response) => {
                // Pipeline processed the message.
                // If a response sink is configured, use it to generate the response.
                if let Some(sink) = &state.response_sink {
                    if let Some(resp) = pipeline_response {
                        match sink.send(resp).await {
                            Ok(Sent::Response(sink_response)) => make_response(Some(sink_response)),
                            Ok(Sent::Ack) => make_response(None),
                            Err(e) => HttpResponse::InternalServerError()
                                .body(format!("Response sink error: {}", e)),
                        }
                    } else {
                        make_response(None)
                    }
                } else {
                    // No sink configured, use the pipeline response (if any)
                    make_response(pipeline_response)
                }
            }
            Err(_) => HttpResponse::InternalServerError().body("Pipeline closed"),
        }
    })
    .await
    {
        Ok(response) => response,
        Err(_) => HttpResponse::GatewayTimeout().body("Request timed out"),
    }
}

#[cfg(feature = "actix-web")]
fn make_response(message: Option<CanonicalMessage>) -> HttpResponse {
    match message {
        Some(msg) => {
            let content_type = msg
                .metadata
                .get("content-type")
                .map(|s| s.as_str())
                .unwrap_or("application/json");
            HttpResponse::Ok()
                .content_type(content_type)
                .body(msg.payload)
        }
        None => HttpResponse::Accepted().body("Message processed"),
    }
}

/// A sink that sends messages to an HTTP endpoint.
#[cfg(feature = "reqwest")]
#[derive(Clone)]
pub struct HttpPublisher {
    client: reqwest::Client,
    url: String,
    response_out: Option<Arc<dyn MessagePublisher>>,
}

#[cfg(feature = "reqwest")]
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
            Some(Box::pin(create_publisher_from_route("http_response_sink", endpoint)).await?)
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

#[cfg(feature = "reqwest")]
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
#[cfg(all(feature = "actix-web", feature = "reqwest"))]
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
            ..Default::default()
        };

        // Start Consumer (Server)
        let mut consumer = HttpConsumer::new(&config)
            .await
            .expect("Failed to create consumer");

        // Start Publisher (Client)
        let pub_config = HttpConfig {
            url: Some(url.clone()),
            ..Default::default()
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

        let mem_config = MemoryConfig {
            topic: "reply_sink".to_string(),
            capacity: Some(10),
        };
        let sink_endpoint = crate::models::Endpoint::new(EndpointType::Memory(mem_config.clone()));

        let config = HttpConfig {
            url: Some(addr.clone()),
            response_out: Some(Box::new(sink_endpoint)),
            ..Default::default()
        };
        let mut consumer = HttpConsumer::new(&config)
            .await
            .expect("Failed to create consumer");

        let pub_config = HttpConfig {
            url: Some(url.clone()),
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
        assert_eq!(response.status(), reqwest::StatusCode::OK);
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
            ..Default::default()
        };
        let mut consumer = HttpConsumer::new(&http_config).await.unwrap();

        // Create ResponsePublisher via factory to simulate route config
        let response_endpoint =
            crate::models::Endpoint::new(EndpointType::Response(crate::models::ResponseConfig {}));
        let publisher = create_publisher_from_route("test_response", &response_endpoint)
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

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "echo_test");
    }
}
