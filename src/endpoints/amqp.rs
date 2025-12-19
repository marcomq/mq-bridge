use crate::models::AmqpConfig;
use crate::traits::{BatchCommitFunc, BoxFuture, MessageConsumer, MessagePublisher};
use crate::APP_NAME;
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::TryStreamExt;
use lapin::tcp::{OwnedIdentity, OwnedTLSConfig};
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        QueueDeclareOptions,
    },
    types::{FieldTable, ShortString},
    BasicProperties, Channel, Connection, ConnectionProperties, Consumer,
};
use std::any::Any;
use std::time::Duration;
use tracing::{debug, info};

pub struct AmqpPublisher {
    channel: Channel,
    exchange: String,
    routing_key: String,
    no_persistence: bool,
    delayed_ack: bool,
}

impl AmqpPublisher {
    pub async fn new(config: &AmqpConfig, routing_key: &str) -> anyhow::Result<Self> {
        let conn = create_amqp_connection(config).await?;
        let channel = conn.create_channel().await?;
        // Enable publisher confirms on this channel to allow waiting for acks.
        channel
            .confirm_select(lapin::options::ConfirmSelectOptions::default())
            .await?;

        // Ensure the queue exists before we try to publish to it. This is idempotent.
        info!(queue = %routing_key, "Declaring AMQP queue in sink");
        channel
            .queue_declare(
                routing_key,
                QueueDeclareOptions {
                    durable: !config.no_persistence,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await?;

        Ok(Self {
            channel,
            exchange: "".to_string(), // Default exchange
            routing_key: routing_key.to_string(),
            no_persistence: config.no_persistence,
            delayed_ack: config.delayed_ack,
        })
    }

    pub fn with_routing_key(&self, routing_key: &str) -> Self {
        Self {
            channel: self.channel.clone(),
            exchange: self.exchange.clone(),
            routing_key: routing_key.to_string(),
            no_persistence: self.no_persistence,
            delayed_ack: self.delayed_ack,
        }
    }
}

#[async_trait]
impl MessagePublisher for AmqpPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let mut properties = if self.no_persistence {
            BasicProperties::default()
        } else {
            // Delivery mode 2 makes the message persistent
            BasicProperties::default().with_delivery_mode(2)
        };
        if let Some(metadata) = message.metadata {
            if !metadata.is_empty() {
                let mut table = FieldTable::default();
                for (key, value) in metadata {
                    table.insert(
                        ShortString::from(key),
                        lapin::types::AMQPValue::LongString(value.into()),
                    );
                }
                properties = properties.with_headers(table);
            }
        }

        let confirmation = self
            .channel
            .basic_publish(
                &self.exchange,
                &self.routing_key,
                BasicPublishOptions::default(),
                &message.payload,
                properties,
            )
            .await?;

        if !self.delayed_ack {
            // Wait for the broker's publisher confirmation.
            confirmation.await?;
        }
        Ok(None)
    }

    // This isn't a real bulk send, but the normal send is fast enough.
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
        // that can be overridden by a new method. For now, we'll prepare for it.
        // Let's default to a higher value to allow for future concurrency.
        // The actual concurrency will be limited by the main bridge loop.
        // A value of 100 is a safe default for enabling parallelism.
        channel.basic_qos(100, BasicQosOptions::default()).await?;

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
    let mut canonical_message = CanonicalMessage::new(delivery.data.clone());
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
                canonical_message.metadata = Some(metadata);
            }
        }
    }
    canonical_message
}

#[async_trait]
impl MessageConsumer for AmqpConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        if max_messages == 0 {
            return Ok((Vec::new(), Box::new(|_| Box::pin(async {}))));
        }

        // 1. Wait for the first message. This will block until a message is available.
        let mut last_delivery = futures::StreamExt::next(&mut self.consumer)
            .await
            .ok_or_else(|| anyhow!("AMQP consumer stream ended"))??;

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

        Ok((messages, commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
