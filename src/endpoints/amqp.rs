use crate::models::AmqpConfig;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch,
    Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::TryStreamExt;
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
use tracing::{debug, info};
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
        let mut properties = if self.no_persistence {
            BasicProperties::default()
        } else {
            // Delivery mode 2 makes the message persistent
            BasicProperties::default().with_delivery_mode(2)
        };
        if !message.metadata.is_empty() {
            let mut table = FieldTable::default();
            for (key, value) in message.metadata {
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

    // This isn't a real bulk send, but the normal send is fast enough.
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct AmqpConsumer {
    consumer: Consumer,
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

        Ok(Self { consumer })
    }
}

pub struct AmqpSubscriber {
    consumer: Consumer,
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

        Ok(Self { consumer })
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
        let mut last_delivery = futures::StreamExt::next(&mut self.consumer)
            .await
            .ok_or(ConsumerError::EndOfStream)?
            .context("Failed to get message from AMQP subscriber stream")?;

        let mut messages = Vec::with_capacity(max_messages);
        messages.push(delivery_to_canonical_message(&last_delivery));

        // 2. Greedily consume more messages if they are already buffered, up to max_messages.
        while messages.len() < max_messages {
            match self.consumer.try_next().await {
                Ok(Some(delivery)) => {
                    messages.push(delivery_to_canonical_message(&delivery));
                    last_delivery = delivery;
                }
                Ok(None) => break, // No more messages in the buffer
                Err(e) => {
                    // An error occurred, but we have some messages. Process them and let the next call handle the error.
                    tracing::warn!("Error receiving subsequent AMQP message: {}", e);
                    break;
                }
            }
        }

        // 3. Create a commit function that acks all received messages.
        let messages_len = messages.len();
        let commit = Box::new(move |_response: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                let ack_options = BasicAckOptions {
                    // Use multiple: true only if we've consumed more than one message.
                    multiple: messages_len > 1,
                    ..Default::default()
                };
                if let Err(e) = last_delivery.ack(ack_options).await {
                    tracing::error!(last_delivery_tag = last_delivery.delivery_tag, error = %e, "Failed to bulk-ack AMQP messages");
                } else {
                    debug!(
                        last_delivery_tag = last_delivery.delivery_tag,
                        count = messages_len,
                        "Bulk-acknowledged AMQP messages"
                    );
                }
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
    let mut canonical_message =
        CanonicalMessage::new(delivery.data.clone(), Some(delivery.delivery_tag as u128));
    if let Some(headers) = delivery.properties.headers().as_ref() {
        if !headers.inner().is_empty() {
            let mut metadata = std::collections::HashMap::new();
            for (key, value) in headers.inner().iter() {
                let value_str = match value {
                    lapin::types::AMQPValue::LongString(s) => s.to_string(),
                    lapin::types::AMQPValue::ShortString(s) => s.to_string(),
                    lapin::types::AMQPValue::Boolean(b) => b.to_string(),
                    lapin::types::AMQPValue::LongInt(i) => i.to_string(),
                    _ => continue,
                };
                metadata.insert(key.to_string(), value_str);
            }
            if !metadata.is_empty() {
                canonical_message.metadata = metadata;
            }
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
        let mut last_delivery = futures::StreamExt::next(&mut self.consumer)
            .await
            .ok_or(ConsumerError::EndOfStream)?
            .context("Failed to get message from AMQP consumer stream")?;

        let mut messages = Vec::with_capacity(max_messages);
        messages.push(delivery_to_canonical_message(&last_delivery));

        // 2. Greedily consume more messages if they are already buffered, up to max_messages.
        while messages.len() < max_messages {
            match self.consumer.try_next().await {
                Ok(Some(delivery)) => {
                    messages.push(delivery_to_canonical_message(&delivery));
                    last_delivery = delivery;
                }
                Ok(None) => break, // No more messages in the buffer
                Err(e) => {
                    // An error occurred, but we have some messages. Process them and let the next call handle the error.
                    tracing::warn!("Error receiving subsequent AMQP message: {}", e);
                    break;
                }
            }
        }

        // 3. Create a commit function that acks all received messages.
        let messages_len = messages.len();
        let commit = Box::new(move |_response: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                let ack_options = BasicAckOptions {
                    // Use multiple: true only if we've consumed more than one message.
                    multiple: messages_len > 1,
                    ..Default::default()
                };
                if let Err(e) = last_delivery.ack(ack_options).await {
                    // Note: If ack fails, we log the error but cannot signal the caller to retry commit as the signature returns ().
                    tracing::error!(last_delivery_tag = last_delivery.delivery_tag, error = %e, "Failed to bulk-ack AMQP messages");
                } else {
                    debug!(
                        last_delivery_tag = last_delivery.delivery_tag,
                        count = messages_len,
                        "Bulk-acknowledged AMQP messages"
                    );
                }
            }) as BoxFuture<'static, ()>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
