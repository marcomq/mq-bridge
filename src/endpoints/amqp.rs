use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::AmqpConfig;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch,
    Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt, TryStreamExt};
use lapin::tcp::{OwnedIdentity, OwnedTLSConfig};
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::{FieldTable, ShortString},
    BasicProperties, Channel, Connection, ConnectionProperties, Consumer, ExchangeKind,
};
use std::any::Any;
use std::time::Duration;
use tracing::{info, trace, warn};
use uuid::Uuid;

pub struct AmqpPublisher {
    channel: Channel,
    exchange: String,
    queue: String,
    no_persistence: bool,
    delayed_ack: bool,
}

impl AmqpPublisher {
    pub async fn new(config: &AmqpConfig, queue: &str) -> anyhow::Result<Self> {
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;
        // Enable publisher confirms on this channel to allow waiting for acks.
        channel
            .confirm_select(lapin::options::ConfirmSelectOptions::default())
            .await?;

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

        Ok(Self {
            channel,
            exchange: config.exchange.clone().unwrap_or_default(),
            queue: queue.to_string(),
            no_persistence: config.no_persistence,
            delayed_ack: config.delayed_ack,
        })
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
            for (key, value) in message.metadata {
                // Skip reply_to and correlation_id since they're already set as native properties
                if key == "reply_to" || key == "correlation_id" {
                    continue;
                }
                table.insert(
                    ShortString::from(key),
                    lapin::types::AMQPValue::LongString(value.into()),
                );
            }
            properties = properties.with_headers(table);
        }

        let confirmation = self
            .channel
            .basic_publish(
                &self.exchange,
                &self.queue,
                BasicPublishOptions::default(),
                &message.payload,
                properties,
            )
            .await
            .context("Failed to publish AMQP message")?;

        if !self.delayed_ack {
            // Wait for the broker's publisher confirmation.
            confirmation
                .await
                .context("Failed to get AMQP publisher confirmation")?;
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
                    // Skip reply_to and correlation_id since they're already set as native properties
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

            match self
                .channel
                .basic_publish(
                    &self.exchange,
                    &self.queue,
                    BasicPublishOptions::default(),
                    &message.payload,
                    properties,
                )
                .await
            {
                Ok(confirmation) => pending_confirms.push((message, confirmation)),
                Err(e) => failed_messages.push((
                    message,
                    PublisherError::Retryable(anyhow::anyhow!("Failed to publish: {}", e)),
                )),
            }
        }

        for (message, confirmation) in pending_confirms {
            match confirmation.await {
                Ok(_) => {}
                Err(e) => failed_messages.push((
                    message,
                    PublisherError::Retryable(anyhow::anyhow!(
                        "Publisher confirmation failed: {}",
                        e
                    )),
                )),
            }
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct AmqpConsumer {
    consumer: Consumer,
    channel: Channel,
    queue: String,
}

impl AmqpConsumer {
    pub async fn new(config: &AmqpConfig, queue: &str) -> anyhow::Result<Self> {
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;

        info!(queue = %queue, "Declaring AMQP queue");
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

        // Set prefetch count. This acts as a buffer and is crucial for concurrent processing.
        // We'll get the concurrency from the route config, but for now, let's use a reasonable default
        // that can be overridden by a new method.
        let prefetch_count = config.prefetch_count.unwrap_or(100);
        channel
            .basic_qos(prefetch_count, BasicQosOptions::default())
            .await?;

        let consumer = channel
            .basic_consume(
                queue,
                &format!("{}_amqp_consumer", APP_NAME),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await?;

        Ok(Self {
            consumer,
            channel,
            queue: queue.to_string(),
        })
    }
}

pub struct AmqpSubscriber {
    consumer: Consumer,
    queue: String,
}

impl AmqpSubscriber {
    /// Creates a new AMQP subscriber.
    ///
    /// This method will create an AMQP connection, channel, declare a fanout exchange,
    /// declare a temporary, exclusive, auto-delete queue, bind the queue to the exchange,
    /// set the prefetch count, and start consuming messages from the queue.
    ///
    /// The `config` parameter is used to configure the connection and channel.
    /// The `queue_or_exchange` parameter is used to determine the exchange name: if the
    /// `config` has an `exchange` field present, that will be used; otherwise, the
    /// `queue_or_exchange` parameter will be used as the exchange name.
    ///
    /// The subscriber will be consuming messages from a temporary queue that is bound to the
    /// specified exchange. The queue will be deleted automatically when the subscriber is dropped.
    ///
    /// The prefetch count is set to the value of `config.prefetch_count`, or 100 if not present.
    ///
    /// The subscriber will be consuming messages with the tag
    /// `<uuid>_<app_name>_sub_<uuid>`.
    pub async fn new(
        config: &AmqpConfig,
        queue_or_exchange: &str,
        subscribe_id: Option<String>,
    ) -> anyhow::Result<Self> {
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;

        // Determine exchange name: use config if present, else use the passed queue/topic name.
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

        // Declare a temporary, exclusive, auto-delete queue
        let id = subscribe_id.unwrap_or_else(|| Uuid::new_v4().to_string());
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
        let queue_name = queue.name().as_str();
        let queue_name_owned = queue_name.to_string();

        info!(queue = %queue_name, exchange = %exchange_name, "Binding temporary queue to exchange");
        channel
            .queue_bind(
                queue_name,
                exchange_name,
                "",
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await?;

        let prefetch_count = config.prefetch_count.unwrap_or(100);
        channel
            .basic_qos(prefetch_count, BasicQosOptions::default())
            .await?;

        let consumer_tag = format!("{}_sub_{}", APP_NAME, id);
        let consumer = channel
            .basic_consume(
                queue_name,
                &consumer_tag,
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await?;

        Ok(Self {
            consumer,
            queue: queue_name_owned,
        })
    }
}

#[async_trait]
impl MessageConsumer for AmqpSubscriber {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async {})),
            });
        }

        // 1. Wait for the first message. This will block until a message is available.
        let first_delivery = self
            .consumer
            .next()
            .await
            .ok_or(ConsumerError::EndOfStream)?
            .context("Failed to get message from AMQP subscriber stream")?;

        let mut messages = Vec::with_capacity(max_messages);
        let mut ackers = Vec::with_capacity(max_messages);

        messages.push(delivery_to_canonical_message(&first_delivery));
        ackers.push(first_delivery.acker);

        // 2. Greedily consume more messages if they are already buffered, up to max_messages.
        while messages.len() < max_messages {
            match self.consumer.try_next().now_or_never() {
                Some(Ok(Some(delivery))) => {
                    messages.push(delivery_to_canonical_message(&delivery));
                    ackers.push(delivery.acker);
                }
                Some(Ok(None)) => break, // Stream ended
                Some(Err(e)) => {
                    // An error occurred, but we have some messages. Process them and let the next call handle the error.
                    tracing::warn!("Error receiving subsequent AMQP message: {}", e);
                    break;
                }
                None => break, // Stream is pending (no messages ready immediately)
            }
        }

        // 3. Create a commit function that acks all received messages.
        let messages_len = messages.len();
        trace!(count = messages_len, queue = %self.queue, message_ids = ?LazyMessageIds(&messages), "Received batch of AMQP subscriber messages");
        let commit = Box::new(move |_response: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                futures::stream::iter(ackers)
                    .for_each_concurrent(None, |acker| async move {
                        if let Err(e) = acker.ack(BasicAckOptions::default()).await {
                            tracing::error!(error = %e, "Failed to ack AMQP message");
                        }
                    })
                    .await;
            }) as BoxFuture<'static, ()>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn create_amqp_connection(config: &AmqpConfig) -> anyhow::Result<Connection> {
    info!(url = %config.url, "Connecting to AMQP broker");
    let mut conn_uri = config.url.clone();

    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        let mut url = url::Url::parse(&conn_uri)?;
        url.set_username(user)
            .map_err(|_| anyhow!("Failed to set username on AMQP URL"))?;
        url.set_password(Some(pass))
            .map_err(|_| anyhow!("Failed to set password on AMQP URL"))?;
        conn_uri = url.to_string();
    }

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
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async {})),
            });
        }

        // 1. Wait for the first message. This will block until a message is available.
        let first_delivery = self
            .consumer
            .next()
            .await
            .ok_or(ConsumerError::EndOfStream)?
            .context("Failed to get message from AMQP consumer stream")?;

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
                    // An error occurred, but we have some messages. Process them and let the next call handle the error.
                    warn!("Error receiving subsequent AMQP message: {}", e);
                    break;
                }
                None => break, // Stream is pending (no messages ready immediately)
            }
        }

        // 3. Create a commit function that acks all received messages.
        let messages_len = messages.len();
        trace!(count = messages_len, queue = %self.queue, message_ids = ?LazyMessageIds(&messages), "Received batch of AMQP messages");
        let channel = self.channel.clone();
        let commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                // Handle replies if responses are provided
                if let Some(resps) = responses {
                    for ((reply_to, correlation_id), resp) in reply_infos.iter().zip(resps) {
                        if let Some(rt) = reply_to {
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
                                    &resp.payload,
                                    props,
                                )
                                .await
                            {
                                tracing::error!(reply_to = %rt, error = %e, "Failed to publish AMQP reply");
                            }
                        }
                    }
                }

                futures::stream::iter(ackers)
                    .for_each_concurrent(None, |acker| async move {
                        if let Err(e) = acker.ack(BasicAckOptions::default()).await {
                            tracing::error!(error = %e, "Failed to ack AMQP message");
                        }
                    })
                    .await;
            }) as BoxFuture<'static, ()>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
