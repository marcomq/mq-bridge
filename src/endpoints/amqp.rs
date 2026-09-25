use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::AmqpConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use futures::{future::join_all, FutureExt, StreamExt, TryStreamExt};
use lapin::tcp::{OwnedIdentity, OwnedTLSConfig};
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::{FieldTable, ShortString},
    Acker, BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, Consumer,
    DefaultConnectionBuilder, ExchangeKind,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{any::Any, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info, trace};

/// Maximum time to wait for a broker publisher confirmation before treating the
/// publish as failed. Prevents the producer from hanging indefinitely when the
/// connection drops and the confirm future never resolves.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the consumer wakes up while waiting for a message to re-check that
/// the broker connection is still alive. In lapin 4 a dropped connection does
/// not reliably terminate the consumer stream, so without this periodic health
/// check `Consumer::next()` can block forever after the broker goes away and the
/// route would never get the error it needs to reconnect.
const CONSUMER_HEALTH_POLL: Duration = Duration::from_secs(1);

/// Maximum time any single connection-setup step (connect, create_channel,
/// confirm_select, declares, qos, consume) may take. In lapin 4 a
/// `Connection::connect` can return `Ok` on a socket the broker immediately
/// force-closes; the io_loop then aborts recovery and any subsequent channel
/// operation on that connection never resolves. Without a bound, the
/// publisher's `reconnect` parks inside `connect` holding the state write lock
/// forever and the whole producer wedges. Bounding each step lets the attempt
/// fail and be retried against a fresh connection.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds a single lapin setup operation, turning a hang or error into a
/// retryable failure. See [`SETUP_TIMEOUT`].
async fn with_setup_timeout<T>(
    op: &str,
    fut: impl std::future::Future<Output = Result<T, lapin::Error>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(SETUP_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow!("AMQP {op} failed: {e}")),
        Err(_) => Err(anyhow!("AMQP {op} timed out after {SETUP_TIMEOUT:?}")),
    }
}

struct AmqpState {
    connection: Connection,
    channel: Channel,
    /// Bumped on every successful reconnect. Lets a worker that observed a
    /// failure ask for a reconnect while letting concurrent workers that share
    /// the same dead channel deduplicate, without trusting lapin's connection
    /// status (which in lapin 4 can keep reporting `connected` on a dead link).
    generation: u64,
}

pub struct AmqpPublisher {
    state: Arc<RwLock<AmqpState>>,
    config: AmqpConfig,
    exchange: String,
    queue: String,
    no_persistence: bool,
    delayed_ack: bool,
}

impl AmqpPublisher {
    pub async fn new(config: &AmqpConfig) -> anyhow::Result<Self> {
        let state = Self::connect(config).await?;
        let queue_or_exchange = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP publisher"))?;

        let (exchange, queue) = if config.subscribe_mode {
            (
                config
                    .exchange
                    .clone()
                    .unwrap_or_else(|| queue_or_exchange.to_string()),
                "".to_string(),
            )
        } else {
            (
                config.exchange.clone().unwrap_or_default(),
                queue_or_exchange.to_string(),
            )
        };

        Ok(Self {
            state: Arc::new(RwLock::new(state)),
            config: config.clone(),
            exchange,
            queue,
            no_persistence: config.no_persistence,
            delayed_ack: config.delayed_ack,
        })
    }

    async fn connect(config: &AmqpConfig) -> anyhow::Result<AmqpState> {
        let queue_or_exchange = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP publisher"))?;
        let conn = create_amqp_connection(config).await?;
        let channel = with_setup_timeout("create_channel", conn.create_channel()).await?;
        // Enable publisher confirms on this channel to allow waiting for acks.
        with_setup_timeout(
            "confirm_select",
            channel.confirm_select(lapin::options::ConfirmSelectOptions::default()),
        )
        .await?;

        if !config.no_declare_queue {
            if config.subscribe_mode {
                let exchange_name = config.exchange.as_deref().unwrap_or(queue_or_exchange);
                info!(exchange = %exchange_name, "Declaring AMQP Fanout exchange in sink");
                with_setup_timeout(
                    "exchange_declare",
                    channel.exchange_declare(
                        exchange_name.into(),
                        ExchangeKind::Fanout,
                        ExchangeDeclareOptions {
                            durable: !config.no_persistence,
                            ..Default::default()
                        },
                        FieldTable::default(),
                    ),
                )
                .await?;
            } else {
                // Ensure the queue exists before we try to publish to it. This is idempotent.
                info!(queue = %queue_or_exchange, "Declaring AMQP queue in sink");
                with_setup_timeout(
                    "queue_declare",
                    channel.queue_declare(
                        queue_or_exchange.into(),
                        QueueDeclareOptions {
                            durable: !config.no_persistence,
                            ..Default::default()
                        },
                        FieldTable::default(),
                    ),
                )
                .await?;
            }
        }

        Ok(AmqpState {
            connection: conn,
            channel,
            generation: 0,
        })
    }

    /// Returns the current channel together with the generation it belongs to.
    /// Callers pass the generation back to `reconnect` so a reconnect only fires
    /// for the channel that actually failed.
    async fn get_channel(&self) -> (Channel, u64) {
        let state = self.state.read().await;
        (state.channel.clone(), state.generation)
    }

    /// Reconnect after a worker observed a failure on the channel of generation
    /// `observed_generation`. If another worker has already reconnected since
    /// (generation moved on), this is a no-op. We deliberately do NOT consult
    /// `connection.status()` here: in lapin 4 a dead connection can keep
    /// reporting `connected`, which previously made this early-return and left
    /// the publisher wedged on a dead channel forever.
    async fn reconnect(&self, observed_generation: u64) {
        let mut state = self.state.write().await;
        if state.generation != observed_generation {
            // A concurrent worker already rebuilt the connection.
            return;
        }
        info!("Reconnecting AMQP publisher...");
        match Self::connect(&self.config).await {
            Ok(new_state) => {
                let next_generation = state.generation.wrapping_add(1);
                *state = new_state;
                state.generation = next_generation;
                info!("AMQP publisher reconnected.");
            }
            Err(e) => {
                error!("Failed to reconnect AMQP publisher: {}", e);
            }
        }
    }
}

#[async_trait]
impl MessagePublisher for AmqpPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            message_id = %format!("{:032x}", message.message_id),
            queue = %self.queue,
            payload_size = message.payload.len(),
            "Publishing AMQP message"
        );
        let mut properties = if self.no_persistence {
            BasicProperties::default()
        } else {
            // Delivery mode 2 makes the message persistent
            BasicProperties::default().with_delivery_mode(2)
        };
        if let Some(reply_to) = message.metadata.get("reply_to") {
            properties = properties.with_reply_to(reply_to.clone().into());
        }
        if let Some(correlation_id) = message.metadata.get("correlation_id") {
            properties = properties.with_correlation_id(correlation_id.clone().into());
        }
        // Carries identity across the hop; the consumer would otherwise fall back to a fresh id.
        properties = properties.with_message_id(
            crate::canonical_message::format_message_id(message.message_id).into(),
        );
        if !message.metadata.is_empty() {
            let mut table = FieldTable::default();
            for (key, value) in &message.metadata {
                // Skip reply_to and correlation_id since they are already set as native properties
                if key == "reply_to" || key == "correlation_id" {
                    continue;
                }
                if crate::canonical_message::is_source_metadata_key(key) {
                    continue; // source/provenance keys must not be forwarded
                }
                table.insert(
                    ShortString::from(key.as_str()),
                    lapin::types::AMQPValue::LongString(value.clone().into()),
                );
            }
            properties = properties.with_headers(table);
        }

        let (channel, generation) = self.get_channel().await;
        let publish_fut = channel.basic_publish(
            self.exchange.clone().into(),
            self.queue.clone().into(),
            BasicPublishOptions::default(),
            &message.payload,
            properties,
        );
        // Bound the publish submit. In lapin 4 a publish to a silently-dropped
        // connection can hang without surfacing an error, so cap the wait and
        // treat it as a retryable failure that triggers a reconnect.
        let confirmation_result = match tokio::time::timeout(CONFIRM_TIMEOUT, publish_fut).await {
            Ok(res) => res,
            Err(_) => {
                self.reconnect(generation).await;
                return Err(PublisherError::Retryable(anyhow!(
                    "Timed out submitting AMQP publish"
                )));
            }
        };

        let confirmation = match confirmation_result {
            Ok(c) => c,
            Err(e) => {
                self.reconnect(generation).await;
                return Err(PublisherError::Retryable(anyhow!(
                    "Failed to publish AMQP message: {}",
                    e
                )));
            }
        };

        if !self.delayed_ack {
            // Wait for the broker's publisher confirmation. Bound the wait so a
            // dropped connection (where the confirm future may never resolve)
            // can't hang the producer indefinitely; instead reconnect and retry.
            let confirm = match tokio::time::timeout(CONFIRM_TIMEOUT, confirmation).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    self.reconnect(generation).await;
                    return Err(PublisherError::Retryable(anyhow!(
                        "Failed to get AMQP publisher confirmation: {}",
                        e
                    )));
                }
                Err(_) => {
                    self.reconnect(generation).await;
                    return Err(PublisherError::Retryable(anyhow!(
                        "Timed out waiting for AMQP publisher confirmation"
                    )));
                }
            };
            if let Confirmation::Nack(_) = confirm {
                return Err(PublisherError::Retryable(anyhow::anyhow!(
                    "Broker Nacked the message"
                )));
            }
        }
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(count = messages.len(), queue = %self.queue, message_ids = ?LazyMessageIds(&messages), "Publishing batch of AMQP messages");
        if self.delayed_ack {
            return crate::traits::send_batch_helper(self, messages, |publisher, message| {
                Box::pin(publisher.send(message))
            })
            .await;
        }

        let (channel, generation) = self.get_channel().await;
        let mut pending_messages = Vec::with_capacity(messages.len());
        let mut pending_confirms = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();

        for message in messages {
            let mut properties = if self.no_persistence {
                BasicProperties::default()
            } else {
                BasicProperties::default().with_delivery_mode(2)
            };
            if let Some(reply_to) = message.metadata.get("reply_to") {
                properties = properties.with_reply_to(reply_to.clone().into());
            }
            if let Some(correlation_id) = message.metadata.get("correlation_id") {
                properties = properties.with_correlation_id(correlation_id.clone().into());
            }
            // Carries identity across the hop; the consumer would otherwise fall back to a fresh id.
            properties = properties.with_message_id(
                crate::canonical_message::format_message_id(message.message_id).into(),
            );

            if !message.metadata.is_empty() {
                let mut table = FieldTable::default();
                for (key, value) in &message.metadata {
                    // Skip reply_to and correlation_id since they are already set as native properties
                    if key == "reply_to" || key == "correlation_id" {
                        continue;
                    }
                    if crate::canonical_message::is_source_metadata_key(key) {
                        continue; // source/provenance keys must not be forwarded
                    }
                    table.insert(
                        ShortString::from(key.clone()),
                        lapin::types::AMQPValue::LongString(value.clone().into()),
                    );
                }
                properties = properties.with_headers(table);
            }

            let publish_fut = channel.basic_publish(
                self.exchange.clone().into(),
                self.queue.clone().into(),
                BasicPublishOptions::default(),
                &message.payload,
                properties,
            );
            match tokio::time::timeout(CONFIRM_TIMEOUT, publish_fut).await {
                Ok(Ok(confirmation)) => {
                    pending_messages.push(message);
                    pending_confirms.push(confirmation);
                }
                Ok(Err(e)) => {
                    failed_messages.push((
                        message,
                        PublisherError::Retryable(
                            anyhow!(e).context("Failed to publish message in batch"),
                        ),
                    ));
                }
                Err(_) => {
                    // In lapin 4 a publish submitted to a silently-dropped
                    // connection can hang without ever surfacing an error. Bound
                    // the submit so the batch fails fast, reconnects, and retries.
                    failed_messages.push((
                        message,
                        PublisherError::Retryable(anyhow!(
                            "Timed out submitting AMQP publish in batch"
                        )),
                    ));
                }
            }
        }

        if !pending_confirms.is_empty() {
            // Bound the whole batch, not each confirmation separately. A broker
            // reset can otherwise stall for batch_size * CONFIRM_TIMEOUT before
            // the route can reconnect and retry.
            match tokio::time::timeout(CONFIRM_TIMEOUT, join_all(pending_confirms)).await {
                Ok(confirmations) => {
                    for (message, confirmation) in pending_messages.into_iter().zip(confirmations) {
                        match confirmation {
                            Ok(confirm) => {
                                if let Confirmation::Nack(_) = confirm {
                                    failed_messages.push((
                                        message,
                                        PublisherError::Retryable(anyhow::anyhow!(
                                            "Broker Nacked the message"
                                        )),
                                    ));
                                }
                            }
                            Err(e) => {
                                failed_messages.push((
                                    message,
                                    PublisherError::Retryable(anyhow::anyhow!(
                                        "Publisher confirmation failed: {}",
                                        e
                                    )),
                                ));
                            }
                        }
                    }
                }
                Err(_) => {
                    failed_messages.extend(pending_messages.into_iter().map(|message| {
                        (
                            message,
                            PublisherError::Retryable(anyhow::anyhow!(
                                "Timed out waiting for AMQP batch publisher confirmations"
                            )),
                        )
                    }));
                }
            }
        }

        if !failed_messages.is_empty() {
            self.reconnect(generation).await;
        }

        Ok(SentBatch::from_failures(failed_messages))
    }

    async fn status(&self) -> EndpointStatus {
        let state = self.state.read().await;
        let conn_status = state.connection.status();
        let chan_status = state.channel.status();
        let healthy = conn_status.connected() && chan_status.connected();
        let error = if !healthy {
            Some(format!(
                "Connection: '{:?}', Channel: '{:?}'",
                conn_status, chan_status
            ))
        } else {
            None
        };
        EndpointStatus {
            healthy,
            error,
            target: if self.exchange.is_empty() {
                self.queue.clone()
            } else {
                self.exchange.clone()
            },
            details: serde_json::json!({ "queue": self.queue, "delayed_ack": self.delayed_ack }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct AmqpConsumer {
    _conn: Connection,
    consumer: Consumer,
    channel: Channel,
    queue: String,
    is_poisoned: Arc<AtomicBool>,
    reply_confirms_selected: Arc<tokio::sync::OnceCell<()>>,
    prefetch: u16,
    /// Resolved once at construction from endpoint config and the legacy fallback.
    source_metadata: bool,
    /// Drain mode: only then does an idle wait surface an empty batch.
    exit_on_empty: bool,
}

impl AmqpConsumer {
    pub async fn new(config: &AmqpConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, false).await
    }

    pub async fn new_with_source_metadata(
        config: &AmqpConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        let source_metadata = crate::canonical_message::source_metadata_enabled_for_endpoint(
            source_metadata || config.source_metadata,
        );
        let queue_or_exchange = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP consumer"))?;
        let conn = create_amqp_connection(config).await?;
        let channel = with_setup_timeout("create_channel", conn.create_channel()).await?;

        let is_subscriber = config.subscribe_mode;

        let queue_name = if is_subscriber {
            // Subscriber mode: Declare Fanout exchange and temporary queue
            let exchange_name = config.exchange.as_deref().unwrap_or(queue_or_exchange);
            info!(exchange = %exchange_name, "Declaring AMQP Fanout exchange for subscriber");
            with_setup_timeout(
                "exchange_declare",
                channel.exchange_declare(
                    exchange_name.into(),
                    ExchangeKind::Fanout,
                    ExchangeDeclareOptions {
                        durable: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                ),
            )
            .await?;

            let id = fast_uuid_v7::gen_id_string();
            let queue_name_str = format!("{}-{}-{}", APP_NAME, queue_or_exchange, id);
            let queue = with_setup_timeout(
                "queue_declare",
                channel.queue_declare(
                    queue_name_str.clone().into(),
                    QueueDeclareOptions {
                        exclusive: true,
                        auto_delete: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                ),
            )
            .await?;
            let q_name = queue.name().as_str().to_string();

            info!(queue = %q_name, exchange = %exchange_name, "Binding temporary queue to exchange");
            with_setup_timeout(
                "queue_bind",
                channel.queue_bind(
                    q_name.clone().into(),
                    exchange_name.into(),
                    "".into(),
                    QueueBindOptions::default(),
                    FieldTable::default(),
                ),
            )
            .await?;
            q_name
        } else {
            // Consumer mode: Declare durable queue
            info!(queue = %queue_or_exchange, "Declaring AMQP queue");
            with_setup_timeout(
                "queue_declare",
                channel.queue_declare(
                    queue_or_exchange.into(),
                    QueueDeclareOptions {
                        durable: !config.no_persistence,
                        ..Default::default()
                    },
                    FieldTable::default(),
                ),
            )
            .await?;
            queue_or_exchange.to_string()
        };

        // Prefetch buffers for concurrent processing; defaults to 100.
        let prefetch_count = config.prefetch_count.unwrap_or(100);
        with_setup_timeout(
            "basic_qos",
            channel.basic_qos(prefetch_count, BasicQosOptions::default()),
        )
        .await?;

        let consumer_tag = if is_subscriber {
            format!("{}_sub_{}", APP_NAME, fast_uuid_v7::gen_id_str())
        } else {
            format!("{}_amqp_consumer", APP_NAME)
        };
        info!(queue = %queue_name, consumer_tag = %consumer_tag, "Starting AMQP consumer");

        let consumer = with_setup_timeout(
            "basic_consume",
            channel.basic_consume(
                queue_name.clone().into(),
                consumer_tag.into(),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            ),
        )
        .await?;

        Ok(Self {
            _conn: conn,
            consumer,
            channel,
            queue: queue_name,
            is_poisoned: Arc::new(AtomicBool::new(false)),
            reply_confirms_selected: Arc::new(tokio::sync::OnceCell::new()),
            prefetch: prefetch_count,
            source_metadata,
            exit_on_empty: false,
        })
    }
}

async fn create_amqp_connection(config: &AmqpConfig) -> anyhow::Result<Connection> {
    info!(url = %config.url, "Connecting to AMQP broker");
    let mut url = url::Url::parse(&config.url).context("Failed to parse AMQP URL")?;

    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        url.set_username(user)
            .map_err(|_| anyhow!("Failed to set username on AMQP URL"))?;
        url.set_password(Some(pass))
            .map_err(|_| anyhow!("Failed to set password on AMQP URL"))?;
    }

    if !url.query_pairs().any(|(k, _)| k == "heartbeat") {
        url.query_pairs_mut().append_pair("heartbeat", "15");
    }
    let conn_uri = url.to_string();

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=5 {
        // Avoid logging credentials embedded in URLs.
        info!(attempt = attempt, "Attempting to connect to AMQP broker");
        let conn_props = ConnectionProperties::default();
        // Bound the connect itself: a hung TLS/AMQP handshake against a broker
        // that is restarting must not stall the reconnect indefinitely.
        let result = if config.tls.required {
            let tls_config = build_tls_config(config).await?;
            let builder = DefaultConnectionBuilder::new()?
                .with_uri_str(conn_uri.clone())
                .with_properties(conn_props)
                .with_tls_config(tls_config);
            with_setup_timeout("connect", builder.connect()).await
        } else {
            with_setup_timeout("connect", Connection::connect(&conn_uri, conn_props)).await
        };

        match result {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                last_error = Some(e);
                // No need to back off after the final attempt.
                if attempt < 5 {
                    tokio::time::sleep(Duration::from_secs(attempt * 2)).await; // Exponential backoff
                }
            }
        }
    }
    Err(anyhow!(
        "Failed to connect to AMQP after multiple attempts: {:?}",
        last_error.unwrap()
    ))
}

async fn build_tls_config(config: &AmqpConfig) -> anyhow::Result<OwnedTLSConfig> {
    // For AMQP, cert_chain is the CA file.
    let ca_file = config.tls.ca_file.clone();

    let identity = if let Some(cert_file) = &config.tls.cert_file {
        // For lapin, client identity is provided via a PKCS12 file.
        // The `cert_file` is assumed to be the PKCS12 bundle. The `key_file` is not used.
        let der = tokio::fs::read(cert_file).await?;
        let password = config.tls.cert_password.clone().unwrap_or_default();
        Some(OwnedIdentity::PKCS12 { der, password })
    } else {
        None
    };

    Ok(OwnedTLSConfig {
        identity,
        cert_chain: ca_file,
    })
}

fn delivery_to_canonical_message(
    delivery: &lapin::message::Delivery,
    source_metadata: bool,
) -> CanonicalMessage {
    // Never the delivery tag: it restarts at 1 on every channel, so it would hand a fresh
    // message the id of one already processed.
    let message_id = delivery
        .properties
        .message_id()
        .as_ref()
        .and_then(|id| crate::canonical_message::message_id_from_str(id.as_str()).ok());

    let mut canonical_message = CanonicalMessage::new(delivery.data.clone(), message_id);

    if let Some(amqp_id) = delivery.properties.message_id().as_ref() {
        canonical_message
            .metadata
            .insert("amqp_message_id".to_string(), amqp_id.to_string());
    }
    if let Some(correlation_id) = delivery.properties.correlation_id().as_ref() {
        canonical_message
            .metadata
            .insert("correlation_id".to_string(), correlation_id.to_string());
    }
    if let Some(reply_to) = delivery.properties.reply_to().as_ref() {
        canonical_message
            .metadata
            .insert("reply_to".to_string(), reply_to.to_string());
    }

    // Source-position cursor keys (useful for dlt-style pull consumers).
    if source_metadata {
        canonical_message.metadata.insert(
            "mqb.src.amqp_routing_key".to_string(),
            delivery.routing_key.to_string(),
        );
        canonical_message.metadata.insert(
            "mqb.src.amqp_exchange".to_string(),
            delivery.exchange.to_string(),
        );
        canonical_message.metadata.insert(
            "mqb.src.amqp_delivery_tag".to_string(),
            delivery.delivery_tag.to_string(),
        );
    }

    if let Some(headers) = delivery.properties.headers().as_ref() {
        for (key, value) in headers.inner().iter() {
            // Keep the framework-generated cursor keys above authoritative: never
            // let an inbound header spoof a reserved `mqb.src.*` value.
            if crate::canonical_message::is_source_metadata_key(key.as_str()) {
                continue;
            }
            let value_str = match value {
                lapin::types::AMQPValue::LongString(s) => s.to_string(),
                lapin::types::AMQPValue::ShortString(s) => s.to_string(),
                lapin::types::AMQPValue::Boolean(b) => b.to_string(),
                lapin::types::AMQPValue::LongInt(i) => i.to_string(),
                _ => continue,
            };
            canonical_message
                .metadata
                .insert(key.to_string(), value_str);
        }
    }
    canonical_message
}

#[async_trait]
impl MessageConsumer for AmqpConsumer {
    // AMQP acks each delivery individually (`BasicAckOptions::default()` =>
    // multiple: false; nacks via BasicNack), and already acks a batch's deliveries
    // concurrently via buffer_unordered. Out-of-order acks across batches are
    // therefore safe: un-acked deliveries are redelivered on reconnect, so this is
    // NOT a cumulative-ack transport despite historically sharing the sequencer.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if self.is_poisoned.load(Ordering::Relaxed) {
            return Err(ConsumerError::Connection(anyhow::anyhow!(
                "AMQP consumer is poisoned due to a previous commit failure."
            )));
        }

        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        // 1. Wait for the first message, bounded so we can periodically check the broker is
        //    alive: in lapin 4 a dropped connection doesn't terminate the stream, so an unbounded
        //    `next()` would block forever and the route would never see the Connection error.
        //    In drain mode the wait shortens to the drain timeout so `exit_on_empty` fires promptly.
        let poll_window = if self.exit_on_empty {
            CONSUMER_HEALTH_POLL.min(crate::traits::drain_idle_timeout())
        } else {
            CONSUMER_HEALTH_POLL
        };
        let first_delivery = loop {
            match tokio::time::timeout(poll_window, self.consumer.next()).await {
                Ok(Some(Ok(delivery))) => break delivery,
                Ok(Some(Err(e))) => return Err(ConsumerError::Connection(anyhow::anyhow!(e))),
                Ok(None) => {
                    return Err(ConsumerError::Connection(anyhow::anyhow!(
                        "AMQP consumer stream ended unexpectedly"
                    )))
                }
                Err(_) => {
                    // No message arrived within the poll window. If the connection
                    // or channel has dropped, surface a Connection error so the route
                    // recreates the consumer; otherwise keep waiting for a message.
                    if !self._conn.status().connected() || !self.channel.status().connected() {
                        return Err(ConsumerError::Connection(anyhow::anyhow!(
                            "AMQP connection lost while waiting for messages"
                        )));
                    }
                    // Drain mode: surface an empty batch instead of looping on an idle queue.
                    if self.exit_on_empty {
                        return Ok(ReceivedBatch::empty());
                    }
                }
            }
        };

        let mut messages = Vec::with_capacity(max_messages);
        let mut ackers = Vec::with_capacity(max_messages);
        let mut reply_infos = Vec::with_capacity(max_messages);

        let msg = delivery_to_canonical_message(&first_delivery, self.source_metadata);
        reply_infos.push((
            msg.metadata.get("reply_to").cloned(),
            msg.metadata.get("correlation_id").cloned(),
        ));
        messages.push(msg);
        ackers.push(first_delivery.acker);

        // 2. Greedily consume more messages if they are already buffered, up to max_messages.
        while messages.len() < max_messages {
            match self.consumer.try_next().now_or_never() {
                Some(Ok(Some(delivery))) => {
                    let msg = delivery_to_canonical_message(&delivery, self.source_metadata);
                    reply_infos.push((
                        msg.metadata.get("reply_to").cloned(),
                        msg.metadata.get("correlation_id").cloned(),
                    ));
                    messages.push(msg);
                    ackers.push(delivery.acker);
                }
                Some(Ok(None)) => break, // Stream ended
                Some(Err(e)) => {
                    // An error occurred. Propagate it immediately.
                    return Err(ConsumerError::Connection(anyhow::anyhow!(e)));
                }
                None => break, // Stream is pending (no messages ready immediately)
            }
        }

        // 3. Create a commit function that acks all received messages.
        let messages_len = messages.len();
        trace!(count = messages_len, queue = %self.queue, message_ids = ?LazyMessageIds(&messages), "Received batch of AMQP messages");
        let channel = self.channel.clone();
        let is_poisoned = self.is_poisoned.clone();
        let reply_confirms_selected = self.reply_confirms_selected.clone();
        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                if dispositions.len() != reply_infos.len() {
                    tracing::error!(
                        expected = reply_infos.len(),
                        actual = dispositions.len(),
                        "AMQP batch commit received mismatched disposition count"
                    );
                    return Err(anyhow::anyhow!(
                        "AMQP batch commit received mismatched disposition count: expected {}, got {}",
                        reply_infos.len(),
                        dispositions.len()
                    ));
                }

                let commit_op = async {
                    handle_replies(
                        &channel,
                        &reply_confirms_selected,
                        &reply_infos,
                        &dispositions,
                    )
                    .await?;
                    handle_dispositions(ackers, dispositions).await
                };

                let result = match tokio::time::timeout(Duration::from_secs(5), commit_op).await {
                    Ok(res) => res,
                    Err(_) => Err(anyhow::anyhow!("AMQP commit timed out")),
                };

                if result.is_err() {
                    is_poisoned.store(true, Ordering::Relaxed);
                }
                result
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let conn_status = self._conn.status();
        let chan_status = self.channel.status();
        let mut healthy = conn_status.connected() && chan_status.connected();
        let mut pending: Option<usize> = None;
        let mut error: Option<String> = None;

        if healthy {
            let passive_declare = self.channel.queue_declare(
                self.queue.clone().into(),
                lapin::options::QueueDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                lapin::types::FieldTable::default(),
            );
            match tokio::time::timeout(Duration::from_secs(2), passive_declare).await {
                Ok(Ok(q)) => pending = Some(q.message_count() as usize),
                Ok(Err(e)) => {
                    healthy = false;
                    error = Some(e.to_string());
                }
                Err(e) => {
                    healthy = false;
                    error = Some(e.to_string());
                }
            }
        } else {
            error = Some(format!(
                "Connection: '{:?}', Channel: '{:?}'",
                conn_status, chan_status
            ));
        }

        EndpointStatus {
            healthy,
            target: self.queue.clone(),
            pending,
            error,
            capacity: Some(self.prefetch as usize),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn handle_replies(
    channel: &Channel,
    reply_confirms_selected: &tokio::sync::OnceCell<()>,
    reply_infos: &[(Option<String>, Option<String>)],
    dispositions: &[MessageDisposition],
) -> anyhow::Result<()> {
    for ((reply_to, correlation_id), disposition) in reply_infos.iter().zip(dispositions.iter()) {
        let payload = match (disposition, reply_to) {
            (MessageDisposition::Reply(resp), Some(_)) => Some(resp.payload.clone()),
            (MessageDisposition::Reply(_), None) => {
                tracing::warn!("MessageDisposition::Reply received but no reply_to address found in original message");
                None
            }
            _ => None,
        };

        if let (Some(rt), Some(body)) = (reply_to, payload) {
            // Enable publisher confirms on the channel exactly once. Concurrent commits
            // share this cell and all await the first confirm_select, so none publishes a
            // reply before confirms are active. On error the cell stays uninitialized so a
            // later commit retries.
            reply_confirms_selected
                .get_or_try_init(|| async {
                    tokio::time::timeout(
                        SETUP_TIMEOUT,
                        channel.confirm_select(lapin::options::ConfirmSelectOptions::default()),
                    )
                    .await
                    .map_err(|_| anyhow!("Timed out enabling AMQP reply confirmations"))?
                    .context("Failed to enable AMQP reply confirmations")?;
                    Ok::<(), anyhow::Error>(())
                })
                .await?;

            let mut props = BasicProperties::default();
            if let Some(cid) = correlation_id {
                props = props.with_correlation_id(cid.clone().into());
            }

            // Publish response to the default exchange with the routing key set to reply_to
            let confirmation = tokio::time::timeout(
                CONFIRM_TIMEOUT,
                channel.basic_publish(
                    "".into(), // Default exchange
                    rt.clone().into(),
                    BasicPublishOptions {
                        mandatory: true,
                        ..Default::default()
                    },
                    &body,
                    props,
                ),
            )
            .await
            .map_err(|_| anyhow!("Timed out submitting AMQP reply to {}", rt))?
            .with_context(|| format!("Failed to publish AMQP reply to {}", rt))?;

            let confirmation = tokio::time::timeout(CONFIRM_TIMEOUT, confirmation)
                .await
                .map_err(|_| anyhow!("Timed out waiting for AMQP reply confirmation to {}", rt))?
                .with_context(|| format!("Failed to get AMQP reply confirmation to {}", rt))?;

            match confirmation {
                Confirmation::Ack(None) => {}
                Confirmation::Ack(Some(returned)) | Confirmation::Nack(Some(returned)) => {
                    bail!(
                        "AMQP reply to {} was returned by broker: {} {}",
                        rt,
                        returned.reply_code,
                        returned.reply_text
                    );
                }
                Confirmation::Nack(None) => {
                    bail!("Broker nacked AMQP reply to {}", rt);
                }
                Confirmation::NotRequested => {
                    bail!("AMQP reply confirmation was not requested for {}", rt);
                }
            }
        }
    }
    Ok(())
}

async fn handle_dispositions(
    ackers: Vec<Acker>,
    dispositions: Vec<MessageDisposition>,
) -> anyhow::Result<()> {
    let ackers_len = ackers.len();
    let mut futures = futures::stream::iter(ackers.into_iter().zip(dispositions).map(
        |(acker, disposition)| async move {
            match disposition {
                MessageDisposition::Ack | MessageDisposition::Reply(_) => {
                    acker.ack(BasicAckOptions::default()).await
                }
                MessageDisposition::Nack => {
                    // Nack with requeue. This will return the message to the front of the queue.
                    acker
                        .nack(lapin::options::BasicNackOptions {
                            requeue: true,
                            ..Default::default()
                        })
                        .await
                }
            }
        },
    ))
    .buffer_unordered(ackers_len);

    while let Some(res) = futures.next().await {
        if let Err(e) = res {
            bail!("Failed to ack/nack AMQP message: {}", e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery(tag: u64, message_id: Option<&str>) -> lapin::message::Delivery {
        let mut delivery =
            lapin::message::Delivery::mock(tag, "".into(), "q".into(), false, b"x".to_vec());
        if let Some(id) = message_id {
            delivery.properties = BasicProperties::default().with_message_id(id.into());
        }
        delivery
    }

    /// Delivery tags restart at 1 per channel; keying on them made a fresh message after a
    /// reconnect look like one already processed.
    #[test]
    fn the_delivery_tag_is_never_the_message_id() {
        let first = delivery_to_canonical_message(&delivery(1, None), false);
        let again = delivery_to_canonical_message(&delivery(1, None), false);
        assert_ne!(first.message_id, 1);
        assert_ne!(first.message_id, again.message_id);
    }

    #[test]
    fn any_message_id_property_is_a_stable_id() {
        for id in ["order-42", "42", "019fd574-0000-7000-8000-000000000001"] {
            let a = delivery_to_canonical_message(&delivery(1, Some(id)), false);
            let b = delivery_to_canonical_message(&delivery(7, Some(id)), false);
            assert_eq!(a.message_id, b.message_id, "{id}");
            assert_eq!(
                a.message_id,
                crate::canonical_message::message_id_from_str(id).unwrap()
            );
        }
    }

    #[test]
    fn a_published_id_round_trips() {
        let id = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdefu128;
        let wire = crate::canonical_message::format_message_id(id);
        let msg = delivery_to_canonical_message(&delivery(3, Some(&wire)), false);
        assert_eq!(msg.message_id, id);
    }
}
