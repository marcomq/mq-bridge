//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::models::WebSocketConfig;
use crate::traits::{
    BoxFuture, CommitFunc, ConsumerError, Handled, Handler, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use std::any::Any;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::watch;
use tokio_websockets::{ClientBuilder, Message, ServerBuilder, WebSocketStream};
use tracing::{debug, trace, warn};
use uuid::Uuid;

type WebSocketSourceMessage = (CanonicalMessage, CommitFunc);
type WebSocketResponseTx = tokio::sync::mpsc::Sender<Message>;
type ClientWebSocketStream = WebSocketStream<tokio_websockets::MaybeTlsStream<TcpStream>>;

const DEFAULT_WEBSOCKET_LISTEN_BACKLOG: u32 = 4096;
const WEBSOCKET_REUSEPORT_ENV: &str = "MQ_BRIDGE_WEBSOCKET_REUSEPORT";
const WEBSOCKET_ACCEPT_WORKERS_ENV: &str = "MQ_BRIDGE_WEBSOCKET_ACCEPT_WORKERS";

fn bind_websocket_listener(addr: SocketAddr, backlog: Option<u32>) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.bind(addr)?;
    socket.listen(backlog.unwrap_or(DEFAULT_WEBSOCKET_LISTEN_BACKLOG))
}

fn websocket_reuseport_enabled() -> bool {
    match std::env::var(WEBSOCKET_REUSEPORT_ENV) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => cfg!(unix),
    }
}

fn websocket_accept_workers() -> usize {
    std::env::var(WEBSOCKET_ACCEPT_WORKERS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|workers| *workers > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        })
}

fn bind_reuseport_websocket_listener(
    addr: SocketAddr,
    backlog: Option<u32>,
) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    socket.set_reuseport(true)?;
    socket.bind(addr)?;
    socket.listen(backlog.unwrap_or(DEFAULT_WEBSOCKET_LISTEN_BACKLOG))
}

fn bind_websocket_listeners(
    addr: SocketAddr,
    backlog: Option<u32>,
) -> std::io::Result<(Vec<TcpListener>, SocketAddr)> {
    let bind_shared = || {
        let listener = bind_websocket_listener(addr, backlog)?;
        let bound_addr = listener.local_addr()?;
        Ok((vec![listener], bound_addr))
    };

    let workers = websocket_accept_workers();
    if workers <= 1 || !websocket_reuseport_enabled() {
        return bind_shared();
    }

    let first = match bind_reuseport_websocket_listener(addr, backlog) {
        Ok(listener) => listener,
        Err(error) => {
            warn!(
                error = %error,
                "SO_REUSEPORT bind failed for WebSocket listener; using one shared listener"
            );
            return bind_shared();
        }
    };
    let bound_addr = first.local_addr()?;
    let mut listeners = Vec::with_capacity(workers);
    listeners.push(first);

    for _ in 1..workers {
        match bind_reuseport_websocket_listener(bound_addr, backlog) {
            Ok(listener) => listeners.push(listener),
            Err(error) => {
                warn!(
                    error = %error,
                    listeners = listeners.len(),
                    workers,
                    "SO_REUSEPORT sibling bind failed for WebSocket listener"
                );
                break;
            }
        }
    }

    tracing::info!(
        listeners = listeners.len(),
        workers,
        "WebSocket server using SO_REUSEPORT listener sharding"
    );
    Ok((listeners, bound_addr))
}

pub struct WebSocketConsumer {
    request_rx: tokio::sync::mpsc::Receiver<WebSocketSourceMessage>,
    shutdown_tx: watch::Sender<bool>,
    queue_capacity: usize,
    url: String,
    bound_addr: SocketAddr,
    /// Drain mode: only then does an idle recv time out into an empty batch.
    exit_on_empty: bool,
}

impl WebSocketConsumer {
    pub async fn new(config: &WebSocketConfig) -> anyhow::Result<Self> {
        let queue_capacity = config.routed_queue_capacity.unwrap_or(100).max(1);
        let listen_addr: SocketAddr = config
            .url
            .parse()
            .with_context(|| format!("Invalid listen address: {}", config.url))?;
        let (listeners, bound_addr) = bind_websocket_listeners(listen_addr, config.backlog)?;
        let path = config.path.as_deref().map(normalize_websocket_path);
        let message_id_header = config
            .message_id_header
            .clone()
            .unwrap_or_else(|| "message-id".to_string());
        let (request_tx, request_rx) = tokio::sync::mpsc::channel(queue_capacity);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        for listener in listeners {
            spawn_accept_loop(
                listener,
                request_tx.clone(),
                shutdown_rx.clone(),
                path.clone(),
                message_id_header.clone(),
            );
        }

        let url = if let Some(path) = path {
            format!("ws://{}{}", bound_addr, path)
        } else {
            format!("ws://{}", bound_addr)
        };

        Ok(Self {
            request_rx,
            shutdown_tx,
            queue_capacity,
            url,
            bound_addr,
            exit_on_empty: false,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }
}

impl Drop for WebSocketConsumer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

pub struct WebSocketPublisher {
    url: String,
    single_stream: tokio::sync::Mutex<Option<ClientWebSocketStream>>,
}

impl WebSocketPublisher {
    pub fn new(config: &WebSocketConfig) -> Self {
        Self {
            url: config.url.clone(),
            single_stream: tokio::sync::Mutex::new(None),
        }
    }

    async fn connect_stream(&self) -> Result<ClientWebSocketStream, PublisherError> {
        let uri = self
            .url
            .parse()
            .with_context(|| format!("Invalid WebSocket URL '{}'", self.url))
            .map_err(PublisherError::Connection)?;
        let (stream, _) = ClientBuilder::from_uri(uri)
            .connect()
            .await
            .with_context(|| format!("Failed to connect to WebSocket endpoint '{}'", self.url))
            .map_err(PublisherError::Connection)?;
        Ok(stream)
    }

    async fn send_reusing_connection(
        &self,
        messages: impl IntoIterator<Item = CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut stream_guard = self.single_stream.lock().await;
        if stream_guard.is_none() {
            *stream_guard = Some(self.connect_stream().await?);
        }
        let stream = stream_guard
            .as_mut()
            .expect("websocket stream was initialized above");

        for message in messages {
            if let Err(error) = stream.feed(canonical_to_websocket_message(&message)).await {
                *stream_guard = None;
                return Err(PublisherError::Retryable(anyhow!(error)));
            }
        }
        if let Err(error) = stream.flush().await {
            *stream_guard = None;
            return Err(PublisherError::Retryable(anyhow!(error)));
        }

        Ok(SentBatch::Ack)
    }
}

#[derive(Clone, Default)]
struct HandshakeMetadata {
    path: String,
    message_id: Option<u128>,
    headers: HashMap<String, String>,
}

fn spawn_accept_loop(
    listener: TcpListener,
    request_tx: tokio::sync::mpsc::Sender<WebSocketSourceMessage>,
    mut shutdown_rx: watch::Receiver<bool>,
    expected_path: Option<String>,
    message_id_header: String,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(parts) => parts,
                        Err(error) => {
                            warn!(error = %error, "WebSocket accept failed");
                            continue;
                        }
                    };
                    let _ = stream.set_nodelay(true);

                    let request_tx = request_tx.clone();
                    let expected_path = expected_path.clone();
                    let message_id_header = message_id_header.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_routed_connection(
                            stream,
                            peer_addr,
                            request_tx,
                            expected_path,
                            message_id_header,
                        )
                        .await
                        {
                            debug!(error = %error, %peer_addr, "WebSocket connection closed with error");
                        }
                    });
                }
            }
        }
    });
}

async fn handle_routed_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    request_tx: tokio::sync::mpsc::Sender<WebSocketSourceMessage>,
    expected_path: Option<String>,
    message_id_header: String,
) -> anyhow::Result<()> {
    let Some((ws_stream, metadata)) =
        accept_websocket_connection(stream, expected_path, message_id_header).await?
    else {
        return Ok(());
    };
    let (mut write_stream, mut read_stream) = ws_stream.split();
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Message>(16);
    let writer_peer_addr = peer_addr;
    let writer_task = tokio::spawn(async move {
        while let Some(message) = response_rx.recv().await {
            if let Err(error) = write_stream.send(message).await {
                debug!(error = %error, %writer_peer_addr, "Failed to send WebSocket response");
                break;
            }
        }
    });

    while let Some(frame) = read_stream.next().await {
        let frame = frame?;
        // tokio-websockets auto-queues pong/close replies for control frames and
        // flushes them on the next read poll, so we only handle data frames here.
        let Some(message) = canonical_from_websocket_frame(frame, &metadata, peer_addr) else {
            continue;
        };

        let response_tx = response_tx.clone();
        let commit: CommitFunc =
            Box::new(move |disposition| websocket_commit(disposition, response_tx));
        if request_tx.send((message, commit)).await.is_err() {
            break;
        }
    }

    drop(response_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), writer_task).await;
    Ok(())
}

pub(crate) async fn run_direct_response_route(
    name: &str,
    config: WebSocketConfig,
    handler: Option<Arc<dyn Handler>>,
    shutdown_rx: async_channel::Receiver<()>,
    ready_tx: Option<async_channel::Sender<()>>,
) -> anyhow::Result<bool> {
    let listen_addr: SocketAddr = config
        .url
        .parse()
        .with_context(|| format!("Invalid listen address: {}", config.url))?;
    let (listeners, _) = bind_websocket_listeners(listen_addr, config.backlog)?;
    let expected_path = config.path.as_deref().map(normalize_websocket_path);
    let message_id_header = config
        .message_id_header
        .clone()
        .unwrap_or_else(|| "message-id".to_string());

    tracing::info!(
        route = name,
        has_output_handler = handler.is_some(),
        "Running WebSocket direct response route"
    );
    if let Some(tx) = ready_tx {
        let _ = tx.send(()).await;
    }

    let (accepted_tx, accepted_rx) = async_channel::bounded(listeners.len().max(1) * 1024);
    let (accept_shutdown_tx, accept_shutdown_rx) = watch::channel(false);
    for listener in listeners {
        let accepted_tx = accepted_tx.clone();
        let mut accept_shutdown_rx = accept_shutdown_rx.clone();
        let route_name = name.to_string();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = accept_shutdown_rx.changed() => {
                        if changed.is_err() || *accept_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accept_result = listener.accept() => {
                        let (stream, peer_addr) = match accept_result {
                            Ok(parts) => parts,
                            Err(error) => {
                                warn!(error = %error, route = %route_name, "WebSocket direct accept failed");
                                continue;
                            }
                        };
                        let _ = stream.set_nodelay(true);
                        if accepted_tx.send((stream, peer_addr)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(accepted_tx);

    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!(
                    "Shutdown signal received in WebSocket direct response runner for route '{}'.",
                    name
                );
                let _ = accept_shutdown_tx.send(true);
                break;
            }
            accepted = accepted_rx.recv() => {
                let (stream, peer_addr) = match accepted {
                    Ok(parts) => parts,
                    Err(_) => break,
                };
                let expected_path = expected_path.clone();
                let message_id_header = message_id_header.clone();
                let handler = handler.clone();
                let route_name = name.to_string();
                connections.spawn(async move {
                    if let Err(error) = handle_direct_connection(
                        stream,
                        peer_addr,
                        expected_path,
                        message_id_header,
                        handler,
                    )
                    .await
                    {
                        debug!(error = %error, %peer_addr, route = %route_name, "WebSocket direct connection closed with error");
                    }
                });
            }
            Some(join_result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = join_result {
                    warn!(error = %error, route = name, "WebSocket direct connection task failed");
                }
            }
        }
    }

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(true)
}

async fn handle_direct_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    expected_path: Option<String>,
    message_id_header: String,
    handler: Option<Arc<dyn Handler>>,
) -> anyhow::Result<()> {
    let Some((mut ws_stream, metadata)) =
        accept_websocket_connection(stream, expected_path, message_id_header).await?
    else {
        return Ok(());
    };

    if handler.is_none() {
        while let Some(frame) = ws_stream.next().await {
            let frame = frame?;
            if frame.is_close() {
                let _ = ws_stream.flush().await;
                break;
            }
            if frame.is_ping() {
                ws_stream.flush().await?;
                continue;
            }
            if frame.is_pong() {
                continue;
            }
            if frame.as_text().is_some() || frame.is_binary() {
                ws_stream.send(frame).await?;
            }
        }

        return Ok(());
    }

    let handler = handler.expect("handler was checked above");
    while let Some(frame) = ws_stream.next().await {
        let frame = frame?;
        if frame.is_close() {
            let _ = ws_stream.flush().await;
            break;
        }
        if frame.is_ping() {
            ws_stream.flush().await?;
            continue;
        }
        if frame.is_pong() {
            continue;
        }
        let Some(message) = canonical_from_websocket_frame(frame, &metadata, peer_addr) else {
            continue;
        };

        let original_id = message.message_id;
        let inbound_correlation_id = message.metadata.get("correlation_id").cloned();
        let handled = match handler.handle(message).await {
            Ok(Handled::Publish(mut response_msg)) => {
                response_msg.message_id = original_id;
                response_msg
                    .metadata
                    .entry("correlation_id".to_string())
                    .or_insert(
                        inbound_correlation_id.unwrap_or_else(|| format!("{:032x}", original_id)),
                    );
                Handled::Publish(response_msg)
            }
            Ok(Handled::Ack) => Handled::Ack,
            Err(error) => {
                warn!(error = %error, %peer_addr, "WebSocket direct handler failed");
                continue;
            }
        };

        if let Handled::Publish(reply) = handled {
            ws_stream
                .send(canonical_to_websocket_message(&reply))
                .await?;
        }
    }

    Ok(())
}

/// Peeks the start of the inbound stream and extracts the request-target path
/// from the HTTP request line, without consuming any bytes. Returns `None` when
/// the request line is not yet fully buffered (caller falls back to the normal
/// handshake) or the stream is empty.
async fn peek_request_path(stream: &TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = [0u8; 2048];
    let n = stream.peek(&mut buf).await?;
    let head = &buf[..n];
    let Some(line_end) = head.windows(2).position(|w| w == b"\r\n") else {
        return Ok(None);
    };
    // Request line: METHOD SP request-target SP HTTP/x.y
    let mut parts = head[..line_end].split(|&b| b == b' ');
    let target = parts.nth(1);
    Ok(target
        .and_then(|t| std::str::from_utf8(t).ok())
        .map(|t| t.split(['?', '#']).next().unwrap_or(t).to_string()))
}

/// Writes a minimal HTTP 404 response and flushes it, best-effort.
async fn respond_not_found(stream: &mut TcpStream) {
    use tokio::io::AsyncWriteExt;
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
        .await;
    let _ = stream.flush().await;
}

async fn accept_websocket_connection(
    mut stream: TcpStream,
    expected_path: Option<String>,
    message_id_header: String,
) -> anyhow::Result<Option<(WebSocketStream<TcpStream>, HandshakeMetadata)>> {
    // If a specific path is required, peek the HTTP request line and reject a
    // mismatch with a real 404 before completing the upgrade handshake. `accept`
    // below re-reads the same bytes, so peeking does not consume the request.
    if let Some(expected) = expected_path.as_deref() {
        if let Some(requested) = peek_request_path(&stream).await? {
            if normalize_websocket_path(&requested) != expected {
                respond_not_found(&mut stream).await;
                return Ok(None);
            }
        }
    }

    let (request, mut ws_stream) = ServerBuilder::new().accept(stream).await?;
    let actual_path = normalize_websocket_path(request.uri().path());
    if let Some(expected_path) = expected_path.as_deref() {
        // Fallback for the rare case where the request line was not fully
        // buffered for the peek above: close after the upgrade instead.
        if actual_path != expected_path {
            let _ = ws_stream
                .send(Message::close(None, "unexpected websocket path"))
                .await;
            return Ok(None);
        }
    }

    let mut metadata = HandshakeMetadata {
        path: request.uri().path().to_string(),
        message_id: request
            .headers()
            .get(message_id_header.as_str())
            .and_then(|value| value.to_str().ok())
            .and_then(parse_message_id),
        headers: HashMap::new(),
    };

    for (name, value) in request.headers() {
        let name_str = name.as_str();
        if matches!(
            name_str,
            "authorization"
                | "cookie"
                | "set-cookie"
                | "proxy-authorization"
                | "x-api-key"
                | "session"
        ) {
            continue;
        }

        if let Ok(value) = value.to_str() {
            metadata
                .headers
                .insert(format!("ws_header.{}", name_str), value.to_string());
        }
    }

    Ok(Some((ws_stream, metadata)))
}

fn canonical_from_websocket_frame(
    frame: Message,
    metadata: &HandshakeMetadata,
    peer_addr: SocketAddr,
) -> Option<CanonicalMessage> {
    let (payload, message_type) = if let Some(text) = frame.as_text() {
        (text.as_bytes().to_vec(), "text")
    } else if frame.is_binary() {
        (frame.as_payload().to_vec(), "binary")
    } else {
        return None;
    };

    let mut message = CanonicalMessage::new(payload, metadata.message_id);
    message
        .metadata
        .insert("ws_message_type".to_string(), message_type.to_string());
    message
        .metadata
        .insert("ws_path".to_string(), metadata.path.clone());
    message
        .metadata
        .insert("ws_peer_addr".to_string(), peer_addr.to_string());
    message.metadata.extend(metadata.headers.clone());
    Some(message)
}

fn websocket_commit(
    disposition: MessageDisposition,
    response_tx: WebSocketResponseTx,
) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        match disposition {
            MessageDisposition::Reply(message) => {
                let _ = response_tx
                    .send(canonical_to_websocket_message(&message))
                    .await;
            }
            MessageDisposition::Ack | MessageDisposition::Nack => {}
        }
        Ok(())
    })
}

fn normalize_websocket_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn parse_message_id(raw: &str) -> Option<u128> {
    if let Ok(uuid) = Uuid::parse_str(raw) {
        Some(uuid.as_u128())
    } else if raw.starts_with("0x") || raw.starts_with("0X") {
        u128::from_str_radix(raw.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
    } else {
        raw.parse::<u128>().ok()
    }
}

fn canonical_to_websocket_message(message: &CanonicalMessage) -> Message {
    let message_type = message.metadata.get("ws_message_type").map(String::as_str);
    match message_type {
        Some("binary") => Message::binary(message.payload.clone().to_vec()),
        Some("text") => Message::text(message.get_payload_str().into_owned()),
        _ => match std::str::from_utf8(&message.payload) {
            Ok(text) => Message::text(text.to_string()),
            Err(_) => Message::binary(message.payload.clone().to_vec()),
        },
    }
}

#[async_trait]
impl MessageConsumer for WebSocketConsumer {
    // Each frame is acked/replied independently (no shared cursor), so commits
    // can run concurrently and out of order.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let max_messages = max_messages.max(1);

        let mut batch: Vec<WebSocketSourceMessage> = Vec::with_capacity(max_messages);
        let received = self.request_rx.recv_many(&mut batch, max_messages);
        // Drain mode: a brief idle timeout yields an empty batch so --drain can fire.
        let Some(count) = crate::traits::drain_gated(self.exit_on_empty, received).await else {
            return Ok(ReceivedBatch::empty());
        };
        if count == 0 {
            return Err(ConsumerError::EndOfStream);
        }

        let (messages, commits): (Vec<_>, Vec<_>) = batch.into_iter().unzip();

        let batch_commit: crate::traits::BatchCommitFunc =
            Box::new(move |dispositions: Vec<MessageDisposition>| {
                Box::pin(async move {
                    for (commit, disposition) in commits.into_iter().zip(dispositions) {
                        commit(disposition).await?;
                    }
                    Ok(())
                })
            });

        Ok(ReceivedBatch {
            messages,
            commit: batch_commit,
        })
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        crate::traits::EndpointStatus {
            healthy: true,
            target: self.url.clone(),
            pending: Some(self.request_rx.len()),
            capacity: Some(self.queue_capacity),
            details: serde_json::json!({
                "bound_addr": self.bound_addr.to_string(),
                "routed_queue_capacity": self.queue_capacity,
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait]
impl MessagePublisher for WebSocketPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        trace!(url = %self.url, count = messages.len(), "Sending WebSocket batch");
        self.send_reusing_connection(messages).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let mut stream_guard = self.single_stream.lock().await;
        if let Some(stream) = stream_guard.as_mut() {
            stream.flush().await?;
        }
        Ok(())
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let mut stream_guard = self.single_stream.lock().await;
            if let Some(mut stream) = stream_guard.take() {
                let _ = stream.close().await;
            }
            Ok(())
        }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_websocket_consumer_publisher_integration() {
        let mut consumer =
            WebSocketConsumer::new(&WebSocketConfig::new("127.0.0.1:0").with_path("/test"))
                .await
                .expect("consumer should be created");
        let publisher = WebSocketPublisher::new(&WebSocketConfig::new(consumer.url().to_string()));

        publisher
            .send(CanonicalMessage::from_vec("hello").with_metadata_kv("ws_message_type", "text"))
            .await
            .expect("publisher should send");

        let mut batch = consumer
            .receive_batch(1)
            .await
            .expect("consumer should receive");
        assert_eq!(batch.messages.len(), 1);
        let message = batch.messages.pop().expect("one message");
        assert_eq!(message.get_payload_str(), "hello");
        assert_eq!(
            message.metadata.get("ws_message_type").map(String::as_str),
            Some("text")
        );
    }
}
