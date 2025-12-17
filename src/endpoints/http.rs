//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue
use crate::models::HttpConfig;
use crate::traits::{BatchCommitFunc, BoxFuture, MessageConsumer, MessagePublisher};
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
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, instrument};

type HttpSourceMessage = (
    CanonicalMessage,
    oneshot::Sender<anyhow::Result<Option<CanonicalMessage>>>,
);

/// A source that listens for incoming HTTP requests.
pub struct HttpConsumer {
    request_rx: mpsc::Receiver<HttpSourceMessage>,
}

impl HttpConsumer {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<HttpSourceMessage>(100);

        let app = Router::new()
            .route("/", post(handle_request))
            .with_state(request_tx);

        let listen_address = config
            .listen_address
            .as_deref()
            .ok_or_else(|| anyhow!("'listen_address' is required for http source connection"))?;
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

                axum::serve(listener, app).await.unwrap();
            }
        });

        ready_rx.await?;
        Ok(Self { request_rx })
    }
}

#[async_trait]
impl MessageConsumer for HttpConsumer {
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (message, response_tx) = self
            .request_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("HTTP source channel closed"))?;

        // The commit function sends the response back to the HTTP client.
        let commit = Box::new(move |response_from_sink: Option<CanonicalMessage>| {
            Box::pin(async move {
                if response_tx.send(Ok(response_from_sink)).is_err() {
                    error!("Failed to send response back to HTTP source handler");
                }
            }) as BoxFuture<'static, ()>
        });

        Ok((vec![message], crate::traits::into_batch_commit_func(commit)))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[instrument(skip_all, fields(http.method = "POST", http.uri = "/"))]
async fn handle_request(
    State(tx): State<mpsc::Sender<HttpSourceMessage>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (response_tx, response_rx) = oneshot::channel();
    let mut message = CanonicalMessage::new(body.to_vec());

    let mut metadata = HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            metadata.insert(key.as_str().to_string(), value_str.to_string());
        }
    }
    message.metadata = Some(metadata);

    if tx.send((message, response_tx)).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to send request to bridge",
        )
            .into_response();
    }

    match response_rx.await {
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
    response_sink: Option<String>,
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

        Ok(Self {
            client: client_builder.build()?,
            url: config.url.clone().unwrap_or_default(),
            response_sink: config.response_sink.clone(),
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
        if self.response_sink.is_some() {
            let mut response_message = CanonicalMessage::new(response_bytes);
            if !response_metadata.is_empty() {
                response_message.metadata = Some(response_metadata);
            }
            Ok(Some(response_message))
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
