//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
use super::memory_transport::MemoryTransport;
use super::transport::{TransportChannel, TransportUrl};
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::event_store::{
    event_store_exists, get_or_create_event_store, EventStore, EventStoreConsumer,
};
use crate::models::MemoryConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tracing::{info, trace, warn};

#[cfg(unix)]
use super::ipc_unix::UnixIpcTransport;
#[cfg(windows)]
use super::ipc_windows::WindowsIpcTransport;

/// A map to hold memory channels for the duration of the bridge setup.
/// This allows a consumer and publisher in different routes to connect to the same in-memory topic.
static RUNTIME_MEMORY_CHANNELS: Lazy<Mutex<HashMap<String, MemoryChannel>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// A map to hold memory response channels.
static RUNTIME_RESPONSE_CHANNELS: Lazy<Mutex<HashMap<String, MemoryResponseChannel>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// A shareable, thread-safe, in-memory channel for testing.
///
/// This struct holds the sender and receiver for an in-memory queue.
/// It can be cloned and shared between your test code and the bridge's endpoints. It transports batches of messages.
#[derive(Debug, Clone)]
pub struct MemoryChannel {
    pub sender: Sender<Vec<CanonicalMessage>>,
    pub receiver: Receiver<Vec<CanonicalMessage>>,
}

impl MemoryChannel {
    /// Creates a new batch channel with a specified capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self { sender, receiver }
    }

    /// Helper function for tests to easily send a message to the channel.
    pub async fn send_message(&self, message: CanonicalMessage) -> anyhow::Result<()> {
        self.sender.send(vec![message]).await?;
        tracing::debug!("Message sent to memory {} channel", self.sender.len());
        Ok(())
    }

    /// Helper function for tests to easily fill in messages.
    pub async fn fill_messages(&self, messages: Vec<CanonicalMessage>) -> anyhow::Result<()> {
        // Send the entire vector as a single batch.
        self.sender
            .send(messages)
            .await
            .map_err(|e| anyhow!("Memory channel was closed while filling messages: {}", e))?;
        Ok(())
    }

    /// Closes the sender part of the channel.
    pub fn close(&self) {
        self.sender.close();
    }

    /// Helper function for tests to drain all messages from the channel.
    pub fn drain_messages(&self) -> Vec<CanonicalMessage> {
        let mut messages = Vec::new();
        // Drain all batches from the channel and flatten them into a single Vec.
        while let Ok(batch) = self.receiver.try_recv() {
            messages.extend(batch);
        }
        messages
    }

    /// Returns the number of bulk messages in the channel.
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// Returns the number of messages currently in the channel.
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

type WaiterMap = HashMap<String, oneshot::Sender<CanonicalMessage>>;

/// A shareable, thread-safe, in-memory channel for responses.
#[derive(Debug, Clone)]
pub struct MemoryResponseChannel {
    pub sender: Sender<CanonicalMessage>,
    pub receiver: Receiver<CanonicalMessage>,
    waiters: Arc<Mutex<WaiterMap>>,
}

impl MemoryResponseChannel {
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self {
            sender,
            receiver,
            waiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn close(&self) {
        self.sender.close();
    }

    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    pub async fn wait_for_response(&self) -> anyhow::Result<CanonicalMessage> {
        self.receiver
            .recv()
            .await
            .map_err(|e| anyhow!("Error receiving response: {}", e))
    }

    pub async fn register_waiter(
        &self,
        correlation_id: &str,
        sender: oneshot::Sender<CanonicalMessage>,
    ) -> anyhow::Result<()> {
        let mut waiters = self.waiters.lock().unwrap();
        if waiters.contains_key(correlation_id) {
            return Err(anyhow!(
                "Correlation ID {} already registered",
                correlation_id
            ));
        }
        waiters.insert(correlation_id.to_string(), sender);
        Ok(())
    }

    pub async fn remove_waiter(
        &self,
        correlation_id: &str,
    ) -> Option<oneshot::Sender<CanonicalMessage>> {
        self.waiters.lock().unwrap().remove(correlation_id)
    }
}

/// Removes a registered waiter unless disarmed, so a `send()` future dropped by a
/// shutdown or an outer timeout cannot leave the correlation id wedged --
/// `register_waiter` rejects duplicates.
struct WaiterGuard {
    waiters: Arc<Mutex<WaiterMap>>,
    correlation_id: String,
    armed: bool,
}

impl WaiterGuard {
    fn new(channel: &MemoryResponseChannel, correlation_id: String) -> Self {
        Self {
            waiters: channel.waiters.clone(),
            correlation_id,
            armed: true,
        }
    }

    /// The response arrived and the responder already took the waiter.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut waiters) = self.waiters.lock() {
                waiters.remove(&self.correlation_id);
            }
        }
    }
}

/// Gets a shared `MemoryChannel` for a given topic, creating it if it doesn't exist.
pub fn get_or_create_channel(config: &MemoryConfig) -> MemoryChannel {
    let topic = memory_namespace(config).unwrap_or_else(|_| config.topic.clone());
    let mut channels = RUNTIME_MEMORY_CHANNELS.lock().unwrap();
    channels
        .entry(topic.clone()) // Use the HashMap's entry API
        .or_insert_with(|| {
            info!(topic = %topic, "Creating new runtime memory channel");
            MemoryChannel::new(config.capacity.unwrap_or(100))
        })
        .clone()
}

/// Gets a shared `MemoryResponseChannel` for a given topic, creating it if it doesn't exist.
pub fn get_or_create_response_channel(topic: &str) -> MemoryResponseChannel {
    let mut channels = RUNTIME_RESPONSE_CHANNELS.lock().unwrap();
    channels
        .entry(topic.to_string())
        .or_insert_with(|| {
            info!(topic = %topic, "Creating new runtime memory response channel");
            MemoryResponseChannel::new(100)
        })
        .clone()
}

fn memory_channel_exists(topic: &str) -> bool {
    let channels = RUNTIME_MEMORY_CHANNELS.lock().unwrap();
    channels.contains_key(topic)
}

fn resolved_transport(config: &MemoryConfig) -> anyhow::Result<TransportUrl> {
    let identifier = config.get_transport_identifier()?;
    TransportUrl::parse(&identifier)
}

fn memory_namespace(config: &MemoryConfig) -> anyhow::Result<String> {
    match resolved_transport(config)? {
        TransportUrl::Memory { namespace } => Ok(namespace),
        other => Err(anyhow!(
            "MemoryConfig uses IPC transport '{}', which requires async endpoint construction",
            other.display_name()
        )),
    }
}

fn normalized_memory_config(config: &MemoryConfig) -> anyhow::Result<MemoryConfig> {
    let mut normalized = config.clone();
    normalized.topic = memory_namespace(config)?;
    normalized.url = None;
    Ok(normalized.with_smart_defaults())
}

/// Create a transport based on the URL scheme
#[allow(dead_code)]
async fn create_transport_from_url(
    url: &TransportUrl,
    capacity: usize,
    is_server: bool,
) -> anyhow::Result<Arc<dyn TransportChannel>> {
    match url {
        TransportUrl::Memory { namespace } => {
            info!(namespace = %namespace, "Creating in-process memory transport");
            Ok(Arc::new(MemoryTransport::new(capacity)))
        }
        #[cfg(unix)]
        TransportUrl::Unix { path } => {
            if is_server {
                info!(path = %path, "Creating Unix IPC server transport");
                let transport = UnixIpcTransport::new_server(path, capacity).await?;
                Ok(Arc::new(transport))
            } else {
                info!(path = %path, "Creating Unix IPC client transport");
                let transport = UnixIpcTransport::new_client(path, capacity).await?;
                Ok(Arc::new(transport))
            }
        }
        #[cfg(windows)]
        TransportUrl::Pipe { name } => {
            if is_server {
                info!(pipe = %name, "Creating Windows Named Pipe server transport");
                let transport = WindowsIpcTransport::new_server(name, capacity).await?;
                Ok(Arc::new(transport))
            } else {
                info!(pipe = %name, "Creating Windows Named Pipe client transport");
                let transport = WindowsIpcTransport::new_client(name, capacity).await?;
                Ok(Arc::new(transport))
            }
        }
        #[cfg(not(any(unix, windows)))]
        _ => Err(anyhow!("IPC transport not supported on this platform")),
    }
}

/// A sink that sends messages to an in-memory channel.
#[derive(Debug, Clone)]
pub struct MemoryPublisher {
    topic: String,
    backend: PublisherBackend,
    request_reply: bool,
    request_timeout: std::time::Duration,
}

#[derive(Clone)]
enum PublisherBackend {
    Queue(Sender<Vec<CanonicalMessage>>),
    Log(Arc<EventStore>),
    Transport(Arc<dyn TransportChannel>),
}

impl fmt::Debug for PublisherBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queue(_) => f.write_str("Queue(..)"),
            Self::Log(_) => f.write_str("Log(..)"),
            Self::Transport(_) => f.write_str("Transport(..)"),
        }
    }
}

impl MemoryPublisher {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let config = normalized_memory_config(config)?;
        let channel_exists = memory_channel_exists(&config.topic);
        let store_exists = event_store_exists(&config.topic);

        let backend = if config.subscribe_mode {
            if channel_exists {
                return Err(anyhow!("Topic '{}' is already active as a Queue (MemoryChannel), but Subscriber mode (EventStore) was requested.", config.topic));
            }
            let store = get_or_create_event_store(&config.topic);
            PublisherBackend::Log(store)
        } else if store_exists {
            // Adaptive behavior: If an EventStore already exists, we publish to it even if
            // subscribe_mode wasn't explicitly set. This prevents split-brain scenarios.
            tracing::debug!(topic = %config.topic, "Adapting publisher to Log mode due to existing EventStore");
            let store = get_or_create_event_store(&config.topic);
            PublisherBackend::Log(store)
        } else {
            let channel = get_or_create_channel(&config);
            PublisherBackend::Queue(channel.sender)
        };

        Ok(Self {
            topic: config.topic.clone(),
            backend,
            request_reply: config.request_reply,
            request_timeout: std::time::Duration::from_millis(
                config.request_timeout_ms.unwrap_or(30000),
            ),
        })
    }

    pub async fn new_async(config: &MemoryConfig) -> anyhow::Result<Self> {
        let url = resolved_transport(config)?;
        match &url {
            TransportUrl::Memory { .. } => Self::new(config),
            _ => {
                if config.subscribe_mode {
                    return Err(anyhow!(
                        "IPC memory publishers do not support subscribe_mode"
                    ));
                }
                if config.request_reply {
                    return Err(anyhow!(
                        "IPC memory publishers do not yet support request_reply"
                    ));
                }
                let capacity = config.capacity.unwrap_or(100);
                let transport = create_transport_from_url(&url, capacity, false).await?;
                Ok(Self {
                    topic: url.display_name(),
                    backend: PublisherBackend::Transport(transport),
                    request_reply: false,
                    request_timeout: std::time::Duration::from_millis(
                        config.request_timeout_ms.unwrap_or(30000),
                    ),
                })
            }
        }
    }

    /// Creates a new local memory publisher.
    ///
    /// This method creates a new in-memory publisher with the specified topic and capacity.
    /// The publisher will send messages to the in-memory channel for the specified topic.
    pub fn new_local(topic: &str, capacity: usize) -> Self {
        Self::new(&MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(capacity),
            ..Default::default()
        })
        .expect("Failed to create local memory publisher")
    }

    /// Note: This helper is primarily for tests expecting a Queue.    
    /// If used on a broadcast publisher, it will create a separate Queue channel.
    pub fn channel(&self) -> MemoryChannel {
        get_or_create_channel(&MemoryConfig {
            topic: self.topic.clone(),
            capacity: None,
            ..Default::default()
        })
    }
}

#[async_trait]
impl MessagePublisher for MemoryPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match &self.backend {
            PublisherBackend::Log(store) => {
                store.append(message).await;
                Ok(Sent::Ack)
            }
            PublisherBackend::Queue(sender) => {
                if self.request_reply {
                    let cid = message
                        .metadata
                        .entry("correlation_id".to_string())
                        .or_insert_with(fast_uuid_v7::gen_id_string)
                        .clone();

                    let (tx, rx) = oneshot::channel();

                    // Register waiter before sending
                    let response_channel = get_or_create_response_channel(&self.topic);
                    response_channel
                        .register_waiter(&cid, tx)
                        .await
                        .map_err(PublisherError::NonRetryable)?;
                    // Covers every exit below, including this future being dropped.
                    let mut waiter = WaiterGuard::new(&response_channel, cid.clone());

                    // Send the message
                    // We use the internal sender directly to avoid recursion or cloning issues
                    if let Err(e) = sender.send(vec![message]).await {
                        return Err(anyhow!("Failed to send to memory channel: {}", e).into());
                    }

                    // Wait for the response
                    let response = match tokio::time::timeout(self.request_timeout, rx).await {
                        Ok(Ok(resp)) => resp,
                        Ok(Err(e)) => {
                            return Err(anyhow!(
                                "Failed to receive response for correlation_id {}: {}",
                                cid,
                                e
                            )
                            .into());
                        }
                        Err(_) => {
                            return Err(PublisherError::Retryable(anyhow!(
                                "Request timed out waiting for response for correlation_id {}",
                                cid
                            )));
                        }
                    };
                    waiter.disarm();

                    Ok(Sent::Response(response))
                } else {
                    sender
                        .send(vec![message])
                        .await
                        .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;
                    Ok(Sent::Ack)
                }
            }
            PublisherBackend::Transport(transport) => {
                transport
                    .send_batch(vec![message])
                    .await
                    .map_err(|e| anyhow!("Failed to send via memory transport: {}", e))?;
                Ok(Sent::Ack)
            }
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        match &self.backend {
            PublisherBackend::Log(store) => {
                trace!(
                    topic = %self.topic,
                    message_ids = ?LazyMessageIds(&messages),
                    "Appending batch to event store"
                );
                store.append_batch(messages).await;
                Ok(SentBatch::Ack)
            }
            PublisherBackend::Queue(sender) => {
                trace!(
                    topic = %self.topic,
                    message_ids = ?LazyMessageIds(&messages),
                    "Sending batch to memory channel. Current batch count: {}",
                    sender.len()
                );
                sender
                    .send(messages)
                    .await
                    .map_err(|e| anyhow!("Failed to send to memory channel: {}", e))?;
                Ok(SentBatch::Ack)
            }
            PublisherBackend::Transport(transport) => {
                trace!(
                    topic = %self.topic,
                    message_ids = ?LazyMessageIds(&messages),
                    "Sending batch to memory transport"
                );
                transport
                    .send_batch(messages)
                    .await
                    .map_err(|e| anyhow!("Failed to send batch via memory transport: {}", e))?;
                Ok(SentBatch::Ack)
            }
        }
    }

    async fn status(&self) -> EndpointStatus {
        match &self.backend {
            PublisherBackend::Queue(sender) => EndpointStatus {
                healthy: !sender.is_closed(),
                target: self.topic.clone(),
                pending: Some(sender.len()),
                capacity: Some(sender.capacity().unwrap_or(0)),
                ..Default::default()
            },
            PublisherBackend::Log(_store) => EndpointStatus {
                healthy: true,
                target: self.topic.clone(),
                details: serde_json::json!({
                    "mode": "event_store"
                }),
                ..Default::default()
            },
            PublisherBackend::Transport(transport) => EndpointStatus {
                healthy: !transport.is_closed(),
                target: self.topic.clone(),
                pending: Some(transport.len()),
                capacity: transport.capacity(),
                details: serde_json::json!({
                    "mode": "transport"
                }),
                ..Default::default()
            },
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A queue-based consumer (legacy behavior).
#[derive(Debug)]
pub struct MemoryQueueConsumer {
    topic: String,
    receiver: Receiver<Vec<CanonicalMessage>>,
    // Internal buffer to hold messages from a received batch.
    buffer: Vec<CanonicalMessage>,
    enable_nack: bool,
    /// Drain mode: only then does an idle recv time out into an empty batch.
    exit_on_empty: bool,
}

#[derive(Clone)]
pub struct TransportQueueConsumer {
    topic: String,
    transport: Arc<dyn TransportChannel>,
    buffer: Vec<CanonicalMessage>,
    /// Nacked messages awaiting redelivery.
    ///
    /// IPC transports are unidirectional (publisher -> consumer), so a requeue
    /// cannot go back down the socket: the publisher never reads, so those bytes
    /// would strand and eventually block the commit on a full socket buffer.
    /// Redelivery is therefore consumer-local, and does not survive a consumer
    /// crash. Shared with the commit closure, which has no access to `&mut self`.
    requeue: Arc<Mutex<Vec<CanonicalMessage>>>,
    enable_nack: bool,
    /// Drain mode: only then does an idle recv time out into an empty batch.
    exit_on_empty: bool,
}

impl fmt::Debug for TransportQueueConsumer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportQueueConsumer")
            .field("topic", &self.topic)
            .field("enable_nack", &self.enable_nack)
            .finish_non_exhaustive()
    }
}

/// A source that reads messages from an in-memory channel or event store.
#[derive(Debug)]
pub enum MemoryConsumer {
    Queue(MemoryQueueConsumer),
    Transport(TransportQueueConsumer),
    Log {
        consumer: EventStoreConsumer,
        topic: String,
    },
}

impl MemoryConsumer {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let config = normalized_memory_config(config)?;
        let channel_exists = memory_channel_exists(&config.topic);
        let store_exists = event_store_exists(&config.topic);

        if config.subscribe_mode {
            if channel_exists {
                return Err(anyhow!("Topic '{}' is already active as a Queue (MemoryChannel), but Subscriber mode (EventStore) was requested.", config.topic));
            }
            let store = get_or_create_event_store(&config.topic);
            // For subscriber mode, we generate a unique ID if one isn't implicit in the usage.
            // However, MemorySubscriber struct usually handles the ID.
            // If MemoryConsumer is used directly with subscribe_mode=true, we assume a default ID or ephemeral.
            let subscriber_id = format!("{}-consumer", config.topic);
            info!(topic = %config.topic, subscriber_id = %subscriber_id, "Memory consumer (Log mode) connected");
            let consumer = store.consumer(subscriber_id);
            Ok(Self::Log {
                consumer,
                topic: config.topic.clone(),
            })
        } else {
            if store_exists {
                // Unlike the Publisher, we cannot silently adapt to Log mode here.
                // The EventStore implementation currently supports Pub/Sub (broadcast) only.
                // Adapting would result in this consumer receiving all messages, violating
                // the expected Queue (competing consumer) semantics requested by `subscribe_mode: false`.
                return Err(anyhow!("Topic '{}' is already active as a Subscriber Log (EventStore), but Queue mode (MemoryChannel) was requested.", config.topic));
            }
            let queue = MemoryQueueConsumer::new(&config)?;
            Ok(Self::Queue(queue))
        }
    }

    pub async fn new_async(config: &MemoryConfig) -> anyhow::Result<Self> {
        let url = resolved_transport(config)?;
        match &url {
            TransportUrl::Memory { .. } => Self::new(config),
            _ => {
                if config.subscribe_mode {
                    return Err(anyhow!(
                        "IPC memory consumers do not support subscribe_mode"
                    ));
                }
                let config = config.clone().with_smart_defaults();
                let capacity = config.capacity.unwrap_or(100);
                let transport = create_transport_from_url(&url, capacity, true).await?;
                Ok(Self::Transport(TransportQueueConsumer {
                    topic: url.display_name(),
                    transport,
                    buffer: Vec::new(),
                    requeue: Arc::new(Mutex::new(Vec::new())),
                    enable_nack: config.enable_nack,
                    exit_on_empty: false,
                }))
            }
        }
    }
}

impl Drop for MemoryQueueConsumer {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let mut messages = std::mem::take(&mut self.buffer);
            messages.reverse();

            let channel = get_or_create_channel(&MemoryConfig {
                topic: self.topic.clone(),
                capacity: None,
                ..Default::default()
            });

            match channel.sender.try_send(messages) {
                Ok(_) => {
                    info!(topic = %self.topic, "Requeued buffered messages on consumer drop");
                }
                Err(e) => {
                    let msgs = match e {
                        async_channel::TrySendError::Full(m) => m,
                        async_channel::TrySendError::Closed(m) => m,
                    };
                    warn!(topic = %self.topic, "Channel full on drop, spawning async requeue");
                    let sender = channel.sender.clone();
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        handle.spawn(async move {
                            if let Err(e) = sender.send(msgs).await {
                                tracing::error!(
                                    "Failed to requeue buffered messages in background: {}",
                                    e
                                );
                            }
                        });
                    } else {
                        tracing::error!(topic = %self.topic, "No active runtime found, could not requeue buffered messages on consumer drop");
                    }
                }
            }
        }
    }
}

impl MemoryQueueConsumer {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let channel = get_or_create_channel(config);
        let buffer = if let Some(capacity) = config.capacity {
            Vec::with_capacity(capacity)
        } else {
            Vec::new()
        };
        Ok(Self {
            topic: config.topic.clone(),
            receiver: channel.receiver.clone(),
            buffer,
            enable_nack: config.enable_nack,
            exit_on_empty: false,
        })
    }

    async fn get_buffered_msgs(
        &mut self,
        max_messages: usize,
    ) -> Result<Vec<CanonicalMessage>, ConsumerError> {
        // If the internal buffer has messages, return them first.
        if self.buffer.is_empty() {
            // Buffer is empty. Wait for a new batch from the channel.
            // Drain mode: a brief idle timeout returns empty so --drain can fire.
            let Some(recv) =
                crate::traits::drain_gated(self.exit_on_empty, self.receiver.recv()).await
            else {
                return Ok(Vec::new());
            };
            self.buffer = match recv {
                Ok(batch) => batch,
                Err(_) => return Err(ConsumerError::EndOfStream),
            };
            // Reverse the buffer so we can efficiently pop from the end.
            self.buffer.reverse();
        }

        // Determine the number of messages to take from the buffer.
        let num_to_take = self.buffer.len().min(max_messages);
        let split_at = self.buffer.len() - num_to_take;

        // `split_off` is highly efficient. It splits the Vec in two at the given
        // index and returns the part after the index, leaving the first part.
        let mut messages = self.buffer.split_off(split_at);
        messages.reverse(); // Reverse back to original order.
        Ok(messages)
    }
}

/// Requeues messages onto a topic's channel without ever blocking the caller.
///
/// Tries a non-blocking send first; if the channel is momentarily full, the
/// blocking send is finished on a detached task. This matters when called from a
/// commit, which holds a route dispatch permit: a blocking `send().await` there
/// would stall the whole commit dispatcher (and thus the consumer that drains
/// this very channel) into a deadlock. Messages are never dropped on a full
/// channel — they are requeued in the background. The only loss is a *closed*
/// channel (or no runtime), which is logged as an error.
fn requeue_messages(topic: &str, messages: Vec<CanonicalMessage>) {
    if messages.is_empty() {
        return;
    }
    let count = messages.len();
    let channel = get_or_create_channel(&MemoryConfig {
        topic: topic.to_string(),
        capacity: None,
        ..Default::default()
    });
    match channel.sender.try_send(messages) {
        Ok(_) => {}
        Err(async_channel::TrySendError::Closed(_)) => {
            tracing::error!(topic = %topic, count, "Dropped messages: memory channel closed during requeue");
        }
        Err(async_channel::TrySendError::Full(msgs)) => match tokio::runtime::Handle::try_current()
        {
            Ok(handle) => {
                let sender = channel.sender.clone();
                let topic = topic.to_string();
                handle.spawn(async move {
                    if let Err(e) = sender.send(msgs).await {
                        tracing::error!(topic = %topic, count, "Dropped messages: background requeue failed: {}", e);
                    }
                });
            }
            Err(_) => {
                tracing::error!(topic = %topic, count, "Dropped messages: no runtime to complete requeue");
            }
        },
    }
}

struct RequeueGuard {
    topic: String,
    messages: Vec<CanonicalMessage>,
}

impl Drop for RequeueGuard {
    fn drop(&mut self) {
        requeue_messages(&self.topic, std::mem::take(&mut self.messages));
    }
}

#[async_trait]
impl MessageConsumer for MemoryQueueConsumer {
    // Channel-backed: commit only requeues this batch's own nacks (no cursor),
    // so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // If the internal buffer has messages, return them first.

        let mut messages = self.get_buffered_msgs(max_messages).await?;
        while messages.len() < max_messages / 2 {
            if let Ok(mut next_batch) = self.receiver.try_recv() {
                if next_batch.len() + messages.len() > max_messages {
                    let needed = max_messages - messages.len();
                    let mut to_buffer = next_batch.split_off(needed);
                    messages.append(&mut next_batch);
                    self.buffer.append(&mut to_buffer);
                    self.buffer.reverse();
                    break;
                } else {
                    messages.append(&mut next_batch);
                }
            } else {
                break;
            }
        }
        trace!(count = messages.len(), topic = %self.topic, message_ids = ?LazyMessageIds(&messages), "Received batch of memory messages");
        if messages.is_empty() {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| {
                    Box::pin(async move { Ok(()) }) as BoxFuture<'static, anyhow::Result<()>>
                }),
            });
        }

        let topic = self.topic.clone();
        let expected_count = messages.len();
        let correlation_ids: Vec<Option<String>> = messages
            .iter()
            .map(|m| m.metadata.get("correlation_id").cloned())
            .collect();

        // Guard to requeue messages if the batch is dropped without commit/nack.
        let mut guard = if self.enable_nack {
            Some(RequeueGuard {
                topic: self.topic.clone(),
                messages: messages.clone(),
            })
        } else {
            None
        };

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                if dispositions.len() != expected_count {
                    return Err(anyhow::anyhow!(
                        "Memory batch commit received mismatched disposition count: expected {}, got {}",
                        expected_count,
                        dispositions.len()
                    ));
                }

                // Clone messages from guard to keep it armed during async operations
                let messages_for_retry = if let Some(g) = &guard {
                    g.messages.clone()
                } else {
                    Vec::new()
                };

                let response_channel = get_or_create_response_channel(&topic);
                let mut to_requeue = Vec::new();

                for (i, disposition) in dispositions.into_iter().enumerate() {
                    match disposition {
                        MessageDisposition::Reply(resp) => {
                            handle_memory_reply(resp, i, &correlation_ids, &response_channel).await;
                        }
                        MessageDisposition::Nack => {
                            if let Some(msg) = messages_for_retry.get(i) {
                                warn!("Requeueing nacked message {}", i);
                                to_requeue.push(msg.clone());
                            } else {
                                warn!("Nack for index {} but no message in retry buffer!", i);
                            }
                        }
                        MessageDisposition::Ack => {}
                    }
                }

                // Requeue nacked messages without blocking the commit: this runs
                // while holding a dispatch permit, so a blocking send into a full
                // channel would deadlock the route. Messages are not dropped.
                requeue_messages(&topic, to_requeue);

                // Disarm the guard after all awaits are finished.
                if let Some(g) = &mut guard {
                    std::mem::take(&mut g.messages);
                }

                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        }) as BatchCommitFunc;
        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let pending = self.receiver.len();
        let capacity = self.receiver.capacity().unwrap_or(0);
        EndpointStatus {
            healthy: !self.receiver.is_closed(),
            target: self.topic.clone(),
            pending: Some(pending),
            capacity: Some(capacity),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait]
impl MessageConsumer for TransportQueueConsumer {
    // Channel-backed: no cursor, commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = Vec::with_capacity(max_messages);

        // Nacked messages get redelivered ahead of anything new.
        {
            let mut requeue = self.requeue.lock().unwrap();
            let take = requeue.len().min(max_messages);
            if take > 0 {
                messages.extend(requeue.drain(..take));
            }
        }

        if messages.len() < max_messages && !self.buffer.is_empty() {
            let buffered = self.buffer.len().min(max_messages - messages.len());
            messages.extend(self.buffer.drain(..buffered));
        }

        // Only block on the transport when nothing was already pending, so a
        // redelivery is never held up waiting for a new frame to arrive.
        if messages.is_empty() {
            // Drain mode: a brief idle timeout leaves the batch empty so --drain can fire.
            if let Some(r) =
                crate::traits::drain_gated(self.exit_on_empty, self.transport.recv_batch()).await
            {
                let mut received = r.map_err(|e| {
                    ConsumerError::Connection(anyhow!(
                        "Failed to receive via memory transport: {}",
                        e
                    ))
                })?;
                messages.append(&mut received);
                if messages.len() > max_messages {
                    self.buffer = messages.split_off(max_messages);
                }
            }
        }

        trace!(count = messages.len(), topic = %self.topic, message_ids = ?LazyMessageIds(&messages), "Received batch from memory transport");

        let topic = self.topic.clone();
        let requeue = self.requeue.clone();
        let enable_nack = self.enable_nack;
        let expected_count = messages.len();
        let messages_for_retry = if enable_nack {
            messages.clone()
        } else {
            Vec::new()
        };

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let requeue = requeue.clone();
            let topic = topic.clone();
            let messages_for_retry = messages_for_retry.clone();
            Box::pin(async move {
                if dispositions.len() != expected_count {
                    return Err(anyhow::anyhow!(
                        "Memory transport batch commit received mismatched disposition count: expected {}, got {}",
                        expected_count,
                        dispositions.len()
                    ));
                }

                let mut to_requeue = Vec::new();
                for (i, disposition) in dispositions.into_iter().enumerate() {
                    match disposition {
                        MessageDisposition::Nack if enable_nack => {
                            if let Some(msg) = messages_for_retry.get(i) {
                                to_requeue.push(msg.clone());
                            }
                        }
                        MessageDisposition::Reply(_) => {
                            tracing::warn!(topic = %topic, "IPC memory transport does not support reply dispositions");
                        }
                        MessageDisposition::Ack | MessageDisposition::Nack => {}
                    }
                }

                if !to_requeue.is_empty() {
                    // Redeliver locally. Sending back down the socket would push
                    // these at a publisher that never reads.
                    let count = to_requeue.len();
                    requeue.lock().unwrap().extend(to_requeue);
                    tracing::debug!(topic = %topic, count, "Requeued nacked IPC messages for local redelivery");
                }

                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        }) as BatchCommitFunc;

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: !self.transport.is_closed(),
            target: self.topic.clone(),
            // Everything readable without waiting on the peer: messages held
            // locally plus whole frames already buffered by the transport.
            pending: Some(
                self.buffer.len()
                    + self.requeue.lock().map(|q| q.len()).unwrap_or(0)
                    + self.transport.len(),
            ),
            capacity: self.transport.capacity(),
            details: serde_json::json!({
                "mode": "transport"
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn handle_memory_reply(
    mut resp: CanonicalMessage,
    index: usize,
    correlation_ids: &[Option<String>],
    response_channel: &MemoryResponseChannel,
) {
    if !resp.metadata.contains_key("correlation_id") {
        if let Some(Some(cid)) = correlation_ids.get(index) {
            resp.metadata
                .insert("correlation_id".to_string(), cid.clone());
        }
    }

    if let Some(cid) = resp.metadata.get("correlation_id") {
        if let Some(tx) = response_channel.remove_waiter(cid).await {
            let _ = tx.send(resp);
            return;
        }
    }
    // No waiter: deliver best-effort. A forward route (no `reply_to`) still gets a
    // publisher response per message but nothing drains this channel; a blocking
    // send would fill the buffer and, since this runs inside a commit holding a
    // dispatch permit, deadlock the route. Drop on overflow instead.
    if let Err(async_channel::TrySendError::Full(_)) = response_channel.sender.try_send(resp) {
        trace!("Dropping unconsumed memory response (response channel full, no waiter)");
    }
}

#[async_trait]
impl MessageConsumer for MemoryConsumer {
    // Delegate to the active backend: channel backends commit order-independently;
    // the Log (event-store) backend keeps the conservative default because its
    // per-subscriber cursor is position-based.
    fn commit_requires_order(&self) -> bool {
        match self {
            Self::Queue(q) => q.commit_requires_order(),
            Self::Transport(t) => t.commit_requires_order(),
            Self::Log { consumer, .. } => consumer.commit_requires_order(),
        }
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        match self {
            Self::Queue(q) => q.set_exit_on_empty(exit_on_empty),
            Self::Transport(t) => t.set_exit_on_empty(exit_on_empty),
            Self::Log { consumer, .. } => consumer.set_exit_on_empty(exit_on_empty),
        }
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        match self {
            Self::Queue(q) => q.receive_batch(max_messages).await,
            Self::Transport(t) => t.receive_batch(max_messages).await,
            Self::Log { consumer, .. } => consumer.receive_batch(max_messages).await,
        }
    }

    async fn status(&self) -> EndpointStatus {
        match self {
            Self::Queue(q) => q.status().await,
            Self::Transport(t) => t.status().await,
            Self::Log { consumer, .. } => consumer.status().await,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MemoryConsumer {
    pub fn new_local(topic: &str, capacity: usize) -> Self {
        Self::new(&MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(capacity),
            ..Default::default()
        })
        .expect("Failed to create local memory consumer")
    }
    pub fn channel(&self) -> MemoryChannel {
        let topic = match self {
            Self::Queue(q) => &q.topic,
            Self::Transport(t) => &t.topic,
            Self::Log { topic, .. } => topic,
        };
        get_or_create_channel(&MemoryConfig {
            topic: topic.clone(),
            ..Default::default()
        })
    }
}

pub struct MemorySubscriber {
    consumer: MemoryConsumer,
}

impl MemorySubscriber {
    pub fn new(config: &MemoryConfig, id: &str) -> anyhow::Result<Self> {
        let mut sub_config = config.clone();
        // If subscribe_mode is true, we use EventStore with the original topic but unique subscriber ID.
        // If false (legacy), we use the suffixed topic queue.
        let consumer = if config.subscribe_mode {
            let store = get_or_create_event_store(&config.topic);
            MemoryConsumer::Log {
                consumer: store.consumer(id.to_string()),
                topic: config.topic.clone(),
            }
        } else {
            sub_config.topic = format!("{}-{}", config.topic, id);
            MemoryConsumer::new(&sub_config)?
        };
        Ok(Self { consumer })
    }
}

#[async_trait]
impl MessageConsumer for MemorySubscriber {
    fn commit_requires_order(&self) -> bool {
        self.consumer.commit_requires_order()
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.consumer.set_exit_on_empty(exit_on_empty);
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.consumer.receive_batch(max_messages).await
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.consumer.receive().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, Route};
    use crate::traits::Handled;
    use crate::{msg, CanonicalMessage};
    use serde_json::json;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_memory_channel_integration() {
        let mut consumer = MemoryConsumer::new_local("test-mem1", 10);
        let publisher = MemoryPublisher::new_local("test-mem1", 10);

        let msg = msg!(json!({"hello": "memory"}));

        // Send a message via the publisher
        publisher.send(msg.clone()).await.unwrap();

        sleep(std::time::Duration::from_millis(10)).await;
        // Receive it with the consumer
        let received = consumer.receive().await.unwrap();
        let _ = (received.commit)(MessageDisposition::Ack).await;
        assert_eq!(received.message.payload, msg.payload);
        assert_eq!(consumer.channel().len(), 0);
    }

    #[tokio::test]
    async fn test_memory_url_alias_uses_same_channel_as_legacy_topic() {
        let mut consumer = MemoryConsumer::new(&MemoryConfig::new("test-memory-url", Some(10)))
            .expect("legacy topic consumer should be in-process memory");
        let publisher = MemoryPublisher::new_async(&MemoryConfig::new_with_url(
            "memory://test-memory-url",
            Some(10),
        ))
        .await
        .expect("memory URL publisher should be in-process memory");

        let msg = msg!(json!({"hello": "memory-url"}));
        publisher.send(msg.clone()).await.unwrap();

        let received = consumer.receive().await.unwrap();
        let _ = (received.commit)(MessageDisposition::Ack).await;
        assert_eq!(received.message.payload, msg.payload);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_unix_ipc_endpoint_constructors_roundtrip() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("endpoint.sock");
        let url = format!("unix://{}", socket_path.display());
        let config = MemoryConfig::new_with_url(url, Some(10));

        assert!(config.clone().with_smart_defaults().enable_nack);

        let mut consumer = MemoryConsumer::new_async(&config)
            .await
            .expect("IPC consumer should create a Unix socket server");
        let publisher = MemoryPublisher::new_async(&config)
            .await
            .expect("IPC publisher should connect to the Unix socket server");

        let msg = CanonicalMessage::from_vec(b"endpoint-ipc");
        publisher.send(msg.clone()).await.unwrap();

        let received = consumer.receive().await.unwrap();
        (received.commit)(MessageDisposition::Ack).await.unwrap();
        assert_eq!(received.message.payload.as_ref(), b"endpoint-ipc");
    }

    /// Regression: nacking over IPC used to call `send_batch` on the consumer's
    /// own transport, writing the messages back down the socket at a publisher
    /// that never reads. They were never redelivered, and once the peer's
    /// receive buffer filled the commit blocked while holding a dispatch permit.
    /// `enable_nack` defaults to true for IPC, so this was the default path.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_unix_ipc_nack_redelivers_locally() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("nack.sock");
        let url = format!("unix://{}", socket_path.display());
        let config = MemoryConfig::new_with_url(url, Some(10));

        // Nack support is on by default for IPC transports.
        assert!(config.clone().with_smart_defaults().enable_nack);

        let mut consumer = MemoryConsumer::new_async(&config).await.unwrap();
        let publisher = MemoryPublisher::new_async(&config).await.unwrap();

        publisher
            .send(CanonicalMessage::from_vec(b"to_be_nacked"))
            .await
            .unwrap();

        // Receive and nack.
        let first = consumer.receive().await.unwrap();
        assert_eq!(first.message.get_payload_str(), "to_be_nacked");
        (first.commit)(MessageDisposition::Nack).await.unwrap();

        // Must come back without the publisher resending anything.
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), consumer.receive())
            .await
            .expect("nacked message should be redelivered")
            .unwrap();
        assert_eq!(second.message.get_payload_str(), "to_be_nacked");

        (second.commit)(MessageDisposition::Ack).await.unwrap();

        // After the ack it must not come back again.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(200), consumer.receive()).await;
        assert!(result.is_err(), "acked message must not be redelivered");
    }

    /// A nack must not block even when nothing is draining the socket, and the
    /// commit must not wedge the route.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_unix_ipc_nack_commit_does_not_block() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("nack_block.sock");
        let url = format!("unix://{}", socket_path.display());
        let config = MemoryConfig::new_with_url(url, Some(1));

        let mut consumer = MemoryConsumer::new_async(&config).await.unwrap();
        let publisher = MemoryPublisher::new_async(&config).await.unwrap();

        // Large enough to exceed the socket buffer (8 KiB on macOS), so the send
        // only completes once the consumer drains it. It has to run concurrently
        // with the receive: the consumer accepts the connection inside
        // `receive_batch`, so a blocking send here would deadlock the test.
        let total = 200usize;
        let msgs: Vec<CanonicalMessage> = (0..total)
            .map(|i| CanonicalMessage::from_vec(format!("m{i}").as_bytes()))
            .collect();
        let send_task = tokio::spawn(async move { publisher.send_batch(msgs).await });

        let batch = consumer.receive_batch(total).await.unwrap();
        let n = batch.messages.len();
        assert_eq!(n, total);
        send_task.await.unwrap().unwrap();

        // Nack the whole batch; this must return promptly rather than blocking
        // on a socket write.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            (batch.commit)(vec![MessageDisposition::Nack; n]),
        )
        .await
        .expect("nack commit must not block")
        .unwrap();

        // All of them are available again.
        let requeued = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            consumer.receive_batch(total),
        )
        .await
        .expect("requeued messages should be readable")
        .unwrap();
        assert_eq!(requeued.messages.len(), n);
        (requeued.commit)(vec![MessageDisposition::Ack; n])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_memory_publisher_and_consumer_integration() {
        let mut consumer = MemoryConsumer::new_local("test-mem2", 10);
        let publisher = MemoryPublisher::new_local("test-mem2", 10);

        let msg1 = msg!(json!({"message": "one"}));
        let msg2 = msg!(json!({"message": "two"}));
        let msg3 = msg!(json!({"message": "three"}));

        publisher
            .send_batch(vec![msg1.clone(), msg2.clone()])
            .await
            .unwrap();
        publisher.send(msg3.clone()).await.unwrap();

        // Verify the channel has the messages
        assert_eq!(publisher.channel().len(), 2);

        // Receive the messages and verify them
        let received1 = consumer.receive().await.unwrap();
        let _ = (received1.commit)(MessageDisposition::Ack).await;
        assert_eq!(received1.message.payload, msg1.payload);

        let batch2 = consumer.receive_batch(1).await.unwrap();
        let (received_msg2, commit2) = (batch2.messages, batch2.commit);
        let _ = commit2(vec![MessageDisposition::Ack; received_msg2.len()]).await;
        assert_eq!(received_msg2.len(), 1);
        assert_eq!(received_msg2.first().unwrap().payload, msg2.payload);
        let batch3 = consumer.receive_batch(2).await.unwrap();
        let (received_msg3, commit3) = (batch3.messages, batch3.commit);
        let _ = commit3(vec![MessageDisposition::Ack; received_msg3.len()]).await;
        assert_eq!(received_msg3.first().unwrap().payload, msg3.payload);

        // Verify the channel is empty
        assert_eq!(publisher.channel().len(), 0);

        // Verify that reading again results in an error because the channel is empty and we are not closing it
        // In a real scenario with a closed channel, this would error out. Here we can just check it's empty.
        // A `receive` call would just hang, waiting for a message.
    }

    #[tokio::test]
    async fn test_memory_subscriber_structure() {
        let cfg = MemoryConfig {
            topic: "base_topic".to_string(),
            capacity: Some(10),
            ..Default::default()
        };
        let subscriber_id = "sub1";
        let mut subscriber = MemorySubscriber::new(&cfg, subscriber_id).unwrap();

        // The subscriber should be listening on "base_topic-sub1"
        // We can verify this by creating a publisher for that specific topic.
        let pub_cfg = MemoryConfig {
            topic: format!("base_topic-{}", subscriber_id),
            capacity: Some(10),
            ..Default::default()
        };
        let publisher = MemoryPublisher::new(&pub_cfg).unwrap();

        publisher.send("hello subscriber".into()).await.unwrap();

        let received = subscriber.receive().await.unwrap();
        assert_eq!(received.message.get_payload_str(), "hello subscriber");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_memory_request_reply_mode() {
        let topic = format!("mem_rr_topic_{}", fast_uuid_v7::gen_id_str());
        let input_endpoint = Endpoint::new_memory(&topic, 10);
        let output_endpoint = Endpoint::new_response();
        let handler = |mut msg: CanonicalMessage| async move {
            let request_payload = msg.get_payload_str();
            let response_payload = format!("reply to {}", request_payload);
            msg.set_payload_str(response_payload);
            Ok(Handled::Publish(msg))
        };

        let route = Route::new(input_endpoint, output_endpoint).with_handler(handler);
        route.deploy("mem_rr_test").await.unwrap();

        // Create a publisher with request_reply = true
        let publisher = MemoryPublisher::new(&MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            request_reply: true,
            request_timeout_ms: Some(2000),
            ..Default::default()
        })
        .unwrap();

        let result = publisher.send("direct request".into()).await.unwrap();

        if let Sent::Response(response_msg) = result {
            assert_eq!(response_msg.get_payload_str(), "reply to direct request");
        } else {
            panic!("Expected Sent::Response, got {:?}", result);
        }

        // Clean up
        Route::stop("mem_rr_test").await;
    }

    #[tokio::test]
    async fn test_memory_request_reply_timeout_cleans_waiter() {
        let topic = format!("mem_rr_timeout_{}", fast_uuid_v7::gen_id_str());
        let correlation_id = fast_uuid_v7::gen_id_string();
        let publisher = MemoryPublisher::new(&MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            request_reply: true,
            request_timeout_ms: Some(25),
            ..Default::default()
        })
        .unwrap();

        let mut message = CanonicalMessage::from("request with no responder");
        message
            .metadata
            .insert("correlation_id".to_string(), correlation_id.clone());

        let err = publisher.send(message).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Request timed out waiting for response"));

        let response_channel = get_or_create_response_channel(&topic);
        assert!(
            response_channel
                .remove_waiter(&correlation_id)
                .await
                .is_none(),
            "timed out request should clean up the registered waiter"
        );
    }

    /// A `send()` future dropped mid-flight (route shutdown, an outer timeout) must not
    /// leave the waiter behind: `register_waiter` rejects duplicates, so a caller-supplied
    /// correlation id would stay wedged for the process lifetime.
    #[tokio::test]
    async fn test_memory_request_reply_cancelled_send_cleans_waiter() {
        let topic = format!("mem_rr_cancel_{}", fast_uuid_v7::gen_id_str());
        let correlation_id = fast_uuid_v7::gen_id_string();
        let publisher = MemoryPublisher::new(&MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            request_reply: true,
            request_timeout_ms: Some(60_000),
            ..Default::default()
        })
        .unwrap();

        let mut message = CanonicalMessage::from("request that gets cancelled");
        message
            .metadata
            .insert("correlation_id".to_string(), correlation_id.clone());

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(25),
            publisher.send(message),
        )
        .await;
        assert!(outcome.is_err(), "send should still be awaiting a response");

        let response_channel = get_or_create_response_channel(&topic);
        assert!(
            response_channel
                .remove_waiter(&correlation_id)
                .await
                .is_none(),
            "cancelled request should clean up the registered waiter"
        );

        let (tx, _rx) = oneshot::channel();
        assert!(
            response_channel
                .register_waiter(&correlation_id, tx)
                .await
                .is_ok(),
            "correlation id must be reusable after cancellation"
        );
    }

    #[tokio::test]
    async fn test_memory_nack_requeue() {
        let topic = format!("test_nack_requeue_{}", fast_uuid_v7::gen_id_str());
        let config = MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            enable_nack: true,
            ..Default::default()
        };
        let mut consumer = MemoryConsumer::new(&config).unwrap();
        let publisher = MemoryPublisher::new_local(&topic, 10);

        publisher.send("to_be_nacked".into()).await.unwrap();

        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.get_payload_str(), "to_be_nacked");
        (received1.commit)(crate::traits::MessageDisposition::Nack)
            .await
            .unwrap();

        let received2 = tokio::time::timeout(std::time::Duration::from_secs(1), consumer.receive())
            .await
            .expect("Timed out waiting for re-queued message")
            .unwrap();
        assert_eq!(received2.message.get_payload_str(), "to_be_nacked");

        (received2.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), consumer.receive()).await;
        assert!(result.is_err(), "Channel should be empty");
    }

    #[tokio::test]
    async fn test_memory_dropped_batch_requeues_messages() {
        let topic = format!("drop_requeue_{}", fast_uuid_v7::gen_id_str());
        let config = MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            enable_nack: true,
            ..Default::default()
        };
        let mut consumer = MemoryConsumer::new(&config).unwrap();
        let publisher = MemoryPublisher::new_local(&topic, 10);

        publisher
            .send_batch(vec!["first".into(), "second".into()])
            .await
            .unwrap();

        let batch = consumer.receive_batch(2).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        drop(batch);

        let requeued =
            tokio::time::timeout(std::time::Duration::from_secs(1), consumer.receive_batch(2))
                .await
                .expect("Timed out waiting for dropped batch to be re-queued")
                .unwrap();

        assert_eq!(
            requeued
                .messages
                .iter()
                .map(CanonicalMessage::get_payload_str)
                .collect::<Vec<_>>(),
            vec!["first".to_string(), "second".to_string()]
        );

        (requeued.commit)(vec![MessageDisposition::Ack, MessageDisposition::Ack])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_memory_batch_commit_rejects_mismatched_dispositions() {
        let topic = format!("commit_mismatch_{}", fast_uuid_v7::gen_id_str());
        let config = MemoryConfig {
            topic: topic.clone(),
            capacity: Some(10),
            enable_nack: true,
            ..Default::default()
        };
        let mut consumer = MemoryConsumer::new(&config).unwrap();
        let publisher = MemoryPublisher::new_local(&topic, 10);

        publisher
            .send_batch(vec!["one".into(), "two".into()])
            .await
            .unwrap();

        let batch = consumer.receive_batch(2).await.unwrap();
        let err = (batch.commit)(vec![MessageDisposition::Ack])
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Memory batch commit received mismatched disposition count"));

        let retried =
            tokio::time::timeout(std::time::Duration::from_secs(1), consumer.receive_batch(2))
                .await
                .expect("Timed out waiting for mismatched commit batch to be re-queued")
                .unwrap();
        assert_eq!(retried.messages.len(), 2);
        (retried.commit)(vec![MessageDisposition::Ack, MessageDisposition::Ack])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_memory_nack_requeue_does_not_block_and_loses_nothing() {
        // Regression: nacked messages are requeued from inside a commit (which holds
        // a route dispatch permit). If the input channel is full, a blocking send
        // there would deadlock the route. The requeue must return immediately AND
        // not drop the nacked messages — they get requeued in the background.
        let topic = format!("nack_requeue_{}", fast_uuid_v7::gen_id_str());
        let config = MemoryConfig {
            topic: topic.clone(),
            capacity: Some(1), // one batch slot, so the channel is easily full
            enable_nack: true,
            ..Default::default()
        };
        let mut consumer = MemoryConsumer::new(&config).unwrap();
        let publisher = MemoryPublisher::new_local(&topic, 1);

        publisher.send_batch(vec!["A".into()]).await.unwrap();
        let batch_a = consumer.receive_batch(4).await.unwrap();
        assert_eq!(batch_a.messages.len(), 1);

        // Fill the (capacity-1) channel so A's nack-requeue cannot fit immediately.
        publisher.send_batch(vec!["B".into()]).await.unwrap();

        // Commit A with a Nack. The channel is full, so the requeue must defer to a
        // background task rather than block the commit.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            (batch_a.commit)(vec![MessageDisposition::Nack]),
        )
        .await
        .expect("nack commit blocked requeuing into a full input channel")
        .unwrap();

        // Both A (requeued) and B must still be delivered — nothing dropped.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.len() < 2 && std::time::Instant::now() < deadline {
            if let Ok(Ok(batch)) = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                consumer.receive_batch(4),
            )
            .await
            {
                for m in &batch.messages {
                    seen.insert(m.get_payload_str().into_owned());
                }
                let n = batch.messages.len();
                (batch.commit)(vec![MessageDisposition::Ack; n])
                    .await
                    .unwrap();
            }
        }
        assert!(seen.contains("A"), "nacked message A was lost");
        assert!(seen.contains("B"), "message B was lost");
    }

    #[tokio::test]
    async fn test_memory_reply_overflow_does_not_block_commit() {
        // Regression: a forward route (memory -> a publisher that returns responses,
        // e.g. HTTP) still produces a publisher response per message, mapped to a
        // Reply disposition. Nothing drains the per-topic response channel and no
        // waiter is registered, so committing must not block once that bounded
        // channel fills — otherwise the commit wedges while holding a dispatch
        // permit and deadlocks the whole route. See `handle_memory_reply`.
        let topic = format!("reply_overflow_{}", fast_uuid_v7::gen_id_str());
        let config = MemoryConfig {
            topic: topic.clone(),
            capacity: Some(1000),
            ..Default::default()
        };
        let mut consumer = MemoryConsumer::new(&config).unwrap();
        let publisher = MemoryPublisher::new_local(&topic, 1000);

        // Far more replies than the response channel capacity (100), nothing draining.
        let total = 500usize;
        let msgs: Vec<CanonicalMessage> = (0..total).map(|i| format!("m{i}").into()).collect();
        publisher.send_batch(msgs).await.unwrap();

        let mut handled = 0usize;
        while handled < total {
            let batch = consumer.receive_batch(16).await.unwrap();
            let n = batch.messages.len();
            if n == 0 {
                break;
            }
            let dispositions: Vec<MessageDisposition> = (0..n)
                .map(|i| MessageDisposition::Reply(format!("r{i}").into()))
                .collect();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                (batch.commit)(dispositions),
            )
            .await
            .expect("commit blocked delivering replies to a full, undrained response channel")
            .unwrap();
            handled += n;
        }
        assert_eq!(handled, total);
    }

    #[tokio::test]
    async fn test_memory_event_store_integration() {
        let topic = "event_store_test";
        // Publisher with subscribe_mode=true enables EventStore writing
        let pub_config = MemoryConfig {
            topic: topic.to_string(),
            subscribe_mode: true,
            ..Default::default()
        };
        let publisher = MemoryPublisher::new(&pub_config).unwrap();

        // Subscriber 1
        let mut sub1 = MemorySubscriber::new(&pub_config, "sub1").unwrap();
        // Subscriber 2
        let mut sub2 = MemorySubscriber::new(&pub_config, "sub2").unwrap();

        publisher.send("event1".into()).await.unwrap();

        let msg1 = sub1.receive().await.unwrap();
        assert_eq!(msg1.message.get_payload_str(), "event1");
        (msg1.commit)(MessageDisposition::Ack).await.unwrap();

        let msg2 = sub2.receive().await.unwrap();
        assert_eq!(msg2.message.get_payload_str(), "event1");
    }

    #[tokio::test]
    async fn test_memory_no_subscribers_persistence() {
        let topic = format!("no_subs_{}", fast_uuid_v7::gen_id_str());
        let pub_config = MemoryConfig {
            topic: topic.clone(),
            subscribe_mode: true,
            ..Default::default()
        };

        let publisher = MemoryPublisher::new(&pub_config).unwrap();

        publisher.send("msg1".into()).await.unwrap();
        publisher.send("msg2".into()).await.unwrap();

        let sub_config = MemoryConfig {
            topic: topic.clone(),
            subscribe_mode: true,
            ..Default::default()
        };
        let mut subscriber = MemorySubscriber::new(&sub_config, "late_sub").unwrap();

        let received1 = subscriber.receive().await.unwrap();
        assert_eq!(received1.message.get_payload_str(), "msg1");
        (received1.commit)(MessageDisposition::Ack).await.unwrap();

        let received2 = subscriber.receive().await.unwrap();
        assert_eq!(received2.message.get_payload_str(), "msg2");
        (received2.commit)(MessageDisposition::Ack).await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_mixed_mode_error() {
        let topic_q = format!("mixed_q_{}", fast_uuid_v7::gen_id_str());
        let topic_l = format!("mixed_l_{}", fast_uuid_v7::gen_id_str());

        // Case 1: Active Queue, try to create Log Consumer
        let _pub_q = MemoryPublisher::new_local(&topic_q, 10); // Creates Queue backend

        let log_conf = MemoryConfig {
            topic: topic_q.clone(),
            subscribe_mode: true,
            ..Default::default()
        };
        let err = MemoryConsumer::new(&log_conf);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("already active as a Queue"));

        // Case 2: Active Log, try to create Queue Consumer
        let log_pub_conf = MemoryConfig {
            topic: topic_l.clone(),
            subscribe_mode: true,
            ..Default::default()
        };
        let _pub_l = MemoryPublisher::new(&log_pub_conf).unwrap(); // Creates Log backend

        let queue_conf = MemoryConfig {
            topic: topic_l.clone(),
            subscribe_mode: false,
            ..Default::default()
        };
        let err = MemoryConsumer::new(&queue_conf);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("already active as a Subscriber Log"));
    }

    #[tokio::test]
    async fn test_memory_publisher_mixed_mode_error() {
        let topic_q = format!("pub_mixed_q_{}", fast_uuid_v7::gen_id_str());

        // Create a Queue Consumer to establish the channel
        let _cons_q = MemoryConsumer::new_local(&topic_q, 10);

        // Try to create a Log Publisher on the same topic
        let log_conf = MemoryConfig {
            topic: topic_q.clone(),
            subscribe_mode: true,
            ..Default::default()
        };
        let err = MemoryPublisher::new(&log_conf);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("already active as a Queue"));
    }

    #[tokio::test]
    async fn test_memory_publisher_adaptive_behavior() {
        let topic = format!("adaptive_{}", fast_uuid_v7::gen_id_str());

        // Create a Log Consumer (Subscriber) to establish the EventStore
        let sub_config = MemoryConfig {
            topic: topic.clone(),
            subscribe_mode: true,
            ..Default::default()
        };
        let mut subscriber = MemorySubscriber::new(&sub_config, "sub1").unwrap();

        // Create a Publisher WITHOUT subscribe_mode explicitly set
        let pub_config = MemoryConfig {
            topic: topic.clone(),
            subscribe_mode: false, // Default is false
            ..Default::default()
        };
        // This should succeed and adapt to Log mode because the store exists
        let publisher = MemoryPublisher::new(&pub_config).unwrap();

        // Verify it publishes to the store (subscriber receives it)
        publisher.send("adaptive_msg".into()).await.unwrap();

        let received = subscriber.receive().await.unwrap();
        assert_eq!(received.message.get_payload_str(), "adaptive_msg");
    }
}
