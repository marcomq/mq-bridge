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
use futures::{FutureExt, StreamExt, TryStreamExt};
use lapin::tcp::{OwnedIdentity, OwnedTLSConfig};
use lapin::{
    acker::Acker,
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::{FieldTable, ShortString},
    BasicProperties, Channel, Connection, ConnectionProperties, Consumer, ExchangeKind,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{any::Any, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info, trace};
use uuid::Uuid;

struct AmqpState {
    connection: Connection,
    channel: Channel,
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
        let queue = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP publisher"))?
            .to_string();

        Ok(Self {
            state: Arc::new(RwLock::new(state)),
            config: config.clone(),
            exchange: config.exchange.clone().unwrap_or_default(),
            queue,
            no_persistence: config.no_persistence,
            delayed_ack: config.delayed_ack,
        })
    }

    async fn connect(config: &AmqpConfig) -> anyhow::Result<AmqpState> {
        let queue = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP publisher"))?;
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;
        // Enable publisher confirms on this channel to allow waiting for acks.
        channel
            .confirm_select(lapin::options::ConfirmSelectOptions::default())
            .await?;

        if !config.no_declare_queue {
            // Ensure the queue exists before we try to publish to it. This is idempotent.
            info!(queue = %queue, "Declaring AMQP queue in sink");
            channel
                .queue_declare(
                    queue,
                    QueueDeclareOptions {
                        durable: !config.no_persistence,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await?;
        }

        Ok(AmqpState {
            connection: conn,
            channel,
        })
    }

    async fn get_channel(&self) -> Channel {
        self.state.read().await.channel.clone()
    }

    async fn reconnect(&self) {
        let mut state = self.state.write().await;
        if state.connection.status().connected() && state.channel.status().connected() {
            return;
        }
        info!("Reconnecting AMQP publisher...");
        match Self::connect(&self.config).await {
            Ok(new_state) => {
                *state = new_state;
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
        if !message.metadata.is_empty() {
            let mut table = FieldTable::default();
            for (key, value) in &message.metadata {
                // Skip reply_to and correlation_id since they are already set as native properties
                if key == "reply_to" || key == "correlation_id" {
                    continue;
                }
                table.insert(
                    ShortString::from(key.as_str()),
                    lapin::types::AMQPValue::LongString(value.clone().into()),
                );
            }
            properties = properties.with_headers(table);
        }

        let channel = self.get_channel().await;
        let confirmation_result = channel
            .basic_publish(
                &self.exchange,
                &self.queue,
                BasicPublishOptions::default(),
                &message.payload,
                properties,
            )
            .await;

        let confirmation = match confirmation_result {
            Ok(c) => c,
            Err(e) => {
                self.reconnect().await;
                return Err(PublisherError::Retryable(anyhow!(
                    "Failed to publish AMQP message: {}",
                    e
                )));
            }
        };

        if !self.delayed_ack {
            // Wait for the broker's publisher confirmation.
            let confirm = match confirmation.await {
                Ok(c) => c,
                Err(e) => {
                    self.reconnect().await;
                    return Err(PublisherError::Retryable(anyhow!(
                        "Failed to get AMQP publisher confirmation: {}",
                        e
                    )));
                }
            };
            if let lapin::publisher_confirm::Confirmation::Nack(_) = confirm {
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

        let channel = self.get_channel().await;
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

            if !message.metadata.is_empty() {
                let mut table = FieldTable::default();
                for (key, value) in &message.metadata {
                    // Skip reply_to and correlation_id since they are already set as native properties
                    if key == "reply_to" || key == "correlation_id" {
                        continue;
                    }
                    table.insert(
                        ShortString::from(key.clone()),
                        lapin::types::AMQPValue::LongString(value.clone().into()),
                    );
                }
                properties = properties.with_headers(table);
            }

            match channel
                .basic_publish(
                    &self.exchange,
                    &self.queue,
                    BasicPublishOptions::default(),
                    &message.payload,
                    properties,
                )
                .await
            {
                Ok(confirmation) => {
                    pending_confirms.push((message, confirmation));
                }
                Err(e) => {
                    failed_messages.push((
                        message,
                        PublisherError::Retryable(
                            anyhow!(e).context("Failed to publish message in batch"),
                        ),
                    ));
                }
            }
        }

        for (message, confirmation) in pending_confirms {
            match confirmation.await {
                Ok(confirm) => {
                    if let lapin::publisher_confirm::Confirmation::Nack(_) = confirm {
                        failed_messages.push((
                            message,
                            PublisherError::Retryable(anyhow::anyhow!("Broker Nacked the message")),
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

        if !failed_messages.is_empty() {
            self.reconnect().await;
        }

        if failed_messages.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
            })
        }
    }

    async fn status(&self) -> EndpointStatus {
        let state = self.state.read().await;
        let conn_status = state.connection.status();
        let chan_status = state.channel.status();
        let healthy = conn_status.connected() && chan_status.connected();
        let error = if !healthy {
            Some(format!(
                "Connection: '{:?}', Channel: '{:?}'",
                conn_status.state(),
                chan_status.state()
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
    prefetch: u16,
}

impl AmqpConsumer {
    pub async fn new(config: &AmqpConfig) -> anyhow::Result<Self> {
        let queue_or_exchange = config
            .queue
            .as_deref()
            .ok_or_else(|| anyhow!("Queue name is required for AMQP consumer"))?;
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;

        let is_subscriber = config.subscribe_mode;

        let queue_name = if is_subscriber {
            // Subscriber mode: Declare Fanout exchange and temporary queue
            let exchange_name = config.exchange.as_deref().unwrap_or(queue_or_exchange);
            info!(exchange = %exchange_name, "Declaring AMQP Fanout exchange for subscriber");
            channel
                .exchange_declare(
                    exchange_name,
                    ExchangeKind::Fanout,
                    ExchangeDeclareOptions {
                        durable: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await?;

            let id = fast_uuid_v7::gen_id_string();
            let queue_name_str = format!("{}-{}-{}", APP_NAME, queue_or_exchange, id);
            let queue = channel
                .queue_declare(
                    &queue_name_str,
                    QueueDeclareOptions {
                        exclusive: true,
                        auto_delete: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await?;
            let q_name = queue.name().as_str().to_string();

            info!(queue = %q_name, exchange = %exchange_name, "Binding temporary queue to exchange");
            channel
                .queue_bind(
                    &q_name,
                    exchange_name,
                    "",
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await?;
            q_name
        } else {
            // Consumer mode: Declare durable queue
            info!(queue = %queue_or_exchange, "Declaring AMQP queue");
            channel
                .queue_declare(
                    queue_or_exchange,
                    QueueDeclareOptions {
                        durable: !config.no_persistence,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await?;
            queue_or_exchange.to_string()
        };

        // Set prefetch count. This acts as a buffer and is crucial for concurrent processing.
        // We'll get the concurrency from the route config, but for now, let's use a reasonable default
        // that can be overridden by a new method.
        let prefetch_count = config.prefetch_count.unwrap_or(100);
        channel
            .basic_qos(prefetch_count, BasicQosOptions::default())
            .await?;

        let consumer_tag = if is_subscriber {
            format!("{}_sub_{}", APP_NAME, fast_uuid_v7::gen_id_str())
        } else {
            format!("{}_amqp_consumer", APP_NAME)
        };
        info!(queue = %queue_name, consumer_tag = %consumer_tag, "Starting AMQP consumer");

        let consumer = channel
            .basic_consume(
                &queue_name,
                &consumer_tag,
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await?;

        Ok(Self {
            _conn: conn,
            consumer,
            channel,
            queue: queue_name,
            is_poisoned: Arc::new(AtomicBool::new(false)),
            prefetch: prefetch_count,
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

    let mut last_error = None;
    for attempt in 1..=5 {
        // Avoid logging credentials embedded in URLs.
        info!(attempt = attempt, "Attempting to connect to AMQP broker");
        let conn_props = ConnectionProperties::default();
        let result = if config.tls.required {
            let tls_config = build_tls_config(config).await?;
            Connection::connect_with_config(&conn_uri, conn_props, tls_config).await
        } else {
            Connection::connect(&conn_uri, conn_props).await
        };

        match result {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                last_error = Some(e);
                tokio::time::sleep(Duration::from_secs(attempt * 2)).await; // Exponential backoff
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

fn delivery_to_canonical_message(delivery: &lapin::message::Delivery) -> CanonicalMessage {
    let mut message_id = Some(delivery.delivery_tag as u128);
    if let Some(amqp_id) = delivery.properties.message_id().as_ref() {
        if let Ok(uuid) = Uuid::parse_str(amqp_id.as_str()) {
            message_id = Some(uuid.as_u128());
        } else if let Ok(val) = amqp_id.as_str().parse::<u128>() {
            message_id = Some(val);
        }
    }

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

    if let Some(headers) = delivery.properties.headers().as_ref() {
        for (key, value) in headers.inner().iter() {
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

        // 1. Wait for the first message. This will block until a message is available.
        let first_delivery = match self.consumer.next().await {
            Some(Ok(delivery)) => delivery,
            Some(Err(e)) => return Err(ConsumerError::Connection(anyhow::anyhow!(e))),
            None => {
                return Err(ConsumerError::Connection(anyhow::anyhow!(
                    "AMQP consumer stream ended unexpectedly"
                )))
            }
        };

        let mut messages = Vec::with_capacity(max_messages);
        let mut ackers = Vec::with_capacity(max_messages);
        let mut reply_infos = Vec::with_capacity(max_messages);

        let msg = delivery_to_canonical_message(&first_delivery);
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
                    let msg = delivery_to_canonical_message(&delivery);
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
        let ackers = Arc::new(ackers);
        let reply_infos = Arc::new(reply_infos);
        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let channel = channel.clone();
            let is_poisoned = is_poisoned.clone();
            let ackers = ackers.clone();
            let reply_infos = reply_infos.clone();
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
                    handle_replies(&channel, &reply_infos, &dispositions).await;
                    handle_dispositions(ackers.to_vec(), dispositions).await
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
                &self.queue,
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
                conn_status.state(),
                chan_status.state()
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
    reply_infos: &[(Option<String>, Option<String>)],
    dispositions: &[MessageDisposition],
) {
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
            let mut props = BasicProperties::default();
            if let Some(cid) = correlation_id {
                props = props.with_correlation_id(cid.clone().into());
            }

            // Publish response to the default exchange with the routing key set to reply_to
            if let Err(e) = channel
                .basic_publish(
                    "", // Default exchange
                    rt,
                    BasicPublishOptions::default(),
                    &body,
                    props,
                )
                .await
            {
                tracing::error!(reply_to = %rt, error = %e, "Failed to publish AMQP reply");
            }
        }
    }
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
