use crate::models::AmqpConfig;
use crate::traits::{BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use lapin::tcp::{OwnedIdentity, OwnedTLSConfig};
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, BasicQosOptions,
        QueueDeclareOptions,
    },
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties, Consumer,
};
use std::any::Any;
use std::time::Duration;
use tracing::{debug, info};

pub struct AmqpPublisher {
    channel: Channel,
    exchange: String,
    routing_key: String,
    await_ack: bool,
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
                QueueDeclareOptions::default(),
                FieldTable::default(),
            )
            .await?;

        Ok(Self {
            channel,
            exchange: "".to_string(), // Default exchange
            routing_key: routing_key.to_string(),
            await_ack: config.await_ack,
        })
    }

    pub fn with_routing_key(&self, routing_key: &str) -> Self {
        Self {
            channel: self.channel.clone(),
            exchange: self.exchange.clone(),
            routing_key: routing_key.to_string(),
            await_ack: self.await_ack,
        }
    }
}

#[async_trait]
impl MessagePublisher for AmqpPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let mut properties = BasicProperties::default();
        if let Some(metadata) = message.metadata {
            if !metadata.is_empty() {
                let mut table = FieldTable::default();
                for (key, value) in metadata {
                    table.insert(
                        key.into(),
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

        if self.await_ack {
            // Wait for the broker's publisher confirmation.
            confirmation.await?;
        }
        Ok(None)
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
            .queue_declare(queue, QueueDeclareOptions::default(), FieldTable::default())
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
                "hot_queue_amqp_consumer",
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
        info!(url = %conn_uri, attempt = attempt, "Attempting to connect to AMQP broker");
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

#[async_trait]
impl MessageConsumer for AmqpConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let delivery = futures::StreamExt::next(&mut self.consumer)
            .await
            .ok_or_else(|| anyhow!("AMQP consumer stream ended"))??;

        let mut message = CanonicalMessage::new(delivery.data.clone());
        if let Some(headers) = delivery.properties.headers().as_ref() {
            if !headers.inner().is_empty() {
                let mut metadata = std::collections::HashMap::new();
                for (key, value) in headers.inner().iter() {
                    if let lapin::types::AMQPValue::LongString(s) = value {
                        metadata.insert(key.to_string(), s.to_string());
                    }
                }
                if !metadata.is_empty() {
                    message.metadata = Some(metadata);
                }
            }
        }

        let commit = Box::new(move |_response| {
            Box::pin(async move {
                delivery
                    .ack(BasicAckOptions::default())
                    .await
                    .expect("Failed to ack AMQP message");
                debug!(
                    delivery_tag = delivery.delivery_tag,
                    "AMQP message acknowledged"
                );
            }) as BoxFuture<'static, ()>
        });

        Ok((message, commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
