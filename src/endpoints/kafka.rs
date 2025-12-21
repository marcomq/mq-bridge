use crate::models::KafkaConfig;
use crate::traits::{BatchCommitFunc, BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::message::OwnedHeaders;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use rdkafka::Offset;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    error::RDKafkaErrorCode,
    message::Headers,
    ClientConfig, Message, TopicPartitionList,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub struct KafkaPublisher {
    producer: FutureProducer,
    topic: String,
    delayed_ack: bool,
}

impl KafkaPublisher {
    pub async fn new(config: &KafkaConfig, topic: &str) -> anyhow::Result<Self> {
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &config.brokers)
            // --- Performance Tuning ---
            .set("linger.ms", "100") // Wait 100ms to batch messages for reliability
            .set("batch.num.messages", "10000") // Max messages per batch.
            .set("compression.type", "lz4") // Efficient compression.
            // --- Reliability ---
            .set("acks", "all") // Wait for all in-sync replicas (safer)
            .set("retries", "3") // Retry up to 3 times
            .set("request.timeout.ms", "30000"); // 30 second timeout

        if config.tls.required {
            client_config.set("security.protocol", "ssl");
            if let Some(ca_file) = &config.tls.ca_file {
                client_config.set("ssl.ca.location", ca_file);
            }
            if let Some(cert_file) = &config.tls.cert_file {
                client_config.set("ssl.certificate.location", cert_file);
            }
            if let Some(key_file) = &config.tls.key_file {
                client_config.set("ssl.key.location", key_file);
            }
            client_config.set(
                "enable.ssl.certificate.verification",
                (!config.tls.accept_invalid_certs).to_string(),
            );
        }

        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            client_config.set("sasl.mechanism", "PLAIN");
            client_config.set("sasl.username", username);
            client_config.set("sasl.password", password);
            client_config.set("security.protocol", "sasl_ssl");
        }

        // Apply custom producer options, allowing overrides of defaults
        if let Some(options) = &config.producer_options {
            for (key, value) in options {
                client_config.set(key, value);
            }
        }

        // Create the topic if it doesn't exist
        if !topic.is_empty() {
            let admin_client: AdminClient<_> = client_config.create()?;
            let new_topic = NewTopic::new(topic, 1, TopicReplication::Fixed(1));
            let results = admin_client
                .create_topics(&[new_topic], &AdminOptions::new())
                .await?;

            // Check the result of the topic creation.
            // It's okay if the topic already exists.
            for result in results {
                match result {
                    Ok(topic_name) => {
                        info!(topic = %topic_name, "Kafka topic created or already exists")
                    }
                    Err((topic_name, error_code)) => {
                        if error_code != RDKafkaErrorCode::TopicAlreadyExists {
                            return Err(anyhow!(
                                "Failed to create Kafka topic '{}': {}",
                                topic_name,
                                error_code
                            ));
                        }
                    }
                }
            }
        }

        let producer: FutureProducer = client_config
            .create()
            .context("Failed to create Kafka producer")?;
        Ok(Self {
            producer,
            topic: topic.to_string(),
            delayed_ack: config.delayed_ack,
        })
    }

    pub fn with_topic(&self, topic: &str) -> Self {
        Self {
            producer: self.producer.clone(),
            topic: topic.to_string(),
            delayed_ack: self.delayed_ack,
        }
    }
}

impl Drop for KafkaPublisher {
    /// On drop, attempt a non-blocking flush.
    /// This is a best-effort attempt. For guaranteed delivery, call `disconnect()` explicitly.
    fn drop(&mut self) {
        info!("KafkaPublisher dropped, attempting to flush remaining messages.");
        self.producer.flush(Duration::from_secs(5)).ok(); // Non-blocking flush
    }
}

#[async_trait]
impl MessagePublisher for KafkaPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let mut record = FutureRecord::to(&self.topic).payload(&message.payload[..]);

        if let Some(metadata) = &message.metadata {
            if !metadata.is_empty() {
                let mut headers = OwnedHeaders::new();
                for (key, value) in metadata {
                    headers = headers.insert(rdkafka::message::Header {
                        key,
                        value: Some(value.as_bytes()),
                    });
                }
                record = record.headers(headers);
            }
        }

        let key = if let Some(id) = message.message_id {
            id.to_be_bytes().to_vec()
        } else {
            uuid::Uuid::new_v4().as_bytes().to_vec()
        };
        record = record.key(&key);

        if !self.delayed_ack {
            // Await the delivery report from Kafka, providing at-least-once guarantees per message.
            self.producer
                .send(record, Duration::from_secs(0))
                .await
                .map_err(|(e, _)| anyhow!("Kafka message delivery failed: {}", e))?;
        } else {
            // "Fire and forget" send. This enqueues the message in the producer's buffer.
            // The `FutureProducer` will handle sending it in the background according to the
            // `linger.ms` and other batching settings. We don't await the delivery report
            // here to achieve high throughput. The `flush()` in `Drop` ensures all messages
            // are sent before shutdown.
            if let Err((e, _)) = self.producer.send_result(record) {
                return Err(anyhow!("Failed to enqueue Kafka message: {}", e));
            }
        }
        Ok(None)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.producer
            .flush(Duration::from_secs(10))
            .map_err(|e| anyhow!("Kafka flush error: {}", e))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
pub struct KafkaConsumer {
    // The consumer needs to be stored to keep the connection alive.
    consumer: Arc<StreamConsumer>,
}
use std::any::Any;

impl KafkaConsumer {
    pub async fn new(config: &KafkaConfig, topic: &str) -> anyhow::Result<Self> {
        use std::sync::Arc;
        let mut client_config = ClientConfig::new();
        if let Some(group_id) = &config.group_id {
            client_config.set("group.id", group_id);
        }
        client_config
            .set("bootstrap.servers", &config.brokers)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            // --- Performance Tuning for Consumers ---
            .set("fetch.min.bytes", "1") // Start fetching immediately
            .set("socket.connection.setup.timeout.ms", "30000"); // 30 seconds

        if config.tls.required {
            client_config.set("security.protocol", "ssl");
            if let Some(ca_file) = &config.tls.ca_file {
                client_config.set("ssl.ca.location", ca_file);
            }
            if let Some(cert_file) = &config.tls.cert_file {
                client_config.set("ssl.certificate.location", cert_file);
            }
            if let Some(key_file) = &config.tls.key_file {
                client_config.set("ssl.key.location", key_file);
            }
            client_config.set(
                "enable.ssl.certificate.verification",
                (!config.tls.accept_invalid_certs).to_string(),
            );
        }
        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            client_config.set("sasl.mechanism", "PLAIN");
            client_config.set("sasl.username", username);
            client_config.set("sasl.password", password);
            client_config.set("security.protocol", "sasl_ssl");
        }

        // Apply custom consumer options
        if let Some(options) = &config.consumer_options {
            for (key, value) in options {
                client_config.set(key, value);
            }
        }

        let consumer: StreamConsumer = client_config.create()?;
        if !topic.is_empty() {
            consumer.subscribe(&[topic])?;

            info!(topic = %topic, "Kafka source subscribed");
        }

        // Wrap the consumer in an Arc to allow it to be shared.
        let consumer = Arc::new(consumer);

        Ok(Self { consumer })
    }
}

impl Drop for KafkaConsumer {
    /// On drop, attempt a non-blocking flush.
    /// This is a best-effort attempt. For guaranteed delivery, call `disconnect()` explicitly.
    fn drop(&mut self) {
        self.consumer.unsubscribe();
    }
}

#[async_trait]
impl MessageConsumer for KafkaConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let message = self.consumer.recv().await?;
        let mut tpl = TopicPartitionList::new();
        let mut messages = Vec::new();
        process_message(message, &mut messages, &mut tpl)?;
        let canonical_message = messages.pop().unwrap();

        // The commit function for Kafka needs to commit the offset of the processed message.
        // We can't move `self.consumer` into the closure, but we can commit by position.
        let consumer = self.consumer.clone();
        let commit = Box::new(move |_response: Option<CanonicalMessage>| {
            Box::pin(async move {
                if let Err(e) = consumer.commit(&tpl, CommitMode::Async) {
                    tracing::error!("Failed to commit Kafka message: {:?}", e);
                }
            }) as BoxFuture<'static, ()>
        });

        Ok((canonical_message, commit))
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let mut messages = Vec::with_capacity(max_messages);
        let mut last_offset_tpl = TopicPartitionList::new();
        {
            // Create a stream from the consumer for this batch operation.
            let mut stream = self.consumer.stream();

            // Block and wait for the first message.
            if let Some(first_message_result) = stream.next().await {
                match first_message_result {
                    Ok(message) => {
                        process_message(message, &mut messages, &mut last_offset_tpl)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            // If we got one message, greedily consume any others that are already buffered.
            if !messages.is_empty() {
                for _ in 1..max_messages {
                    match stream.try_next().await {
                        Ok(Some(message)) => {
                            process_message(message, &mut messages, &mut last_offset_tpl)?;
                        }
                        _ => {
                            // Stream is not ready, an error occurred, or it ended.
                            // In any case, we stop trying to get more messages for this batch.
                            break;
                        }
                    }
                }
            }
        }
        let messages_len = messages.len();

        let consumer = self.consumer.clone();
        let commit = Box::new(move |_responses: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                // Only commit if there are offsets to commit.
                if messages_len > 0 {
                    if let Err(e) = consumer.commit(&last_offset_tpl, CommitMode::Async) {
                        tracing::error!("Failed to commit Kafka message batch: {:?}", e);
                    }
                }
            }) as BoxFuture<'static, ()>
        });
        Ok((messages, commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Helper function to process a Kafka message and add it to the batch.
fn process_message(
    message: rdkafka::message::BorrowedMessage,
    messages: &mut Vec<CanonicalMessage>,
    last_offset_tpl: &mut TopicPartitionList,
) -> anyhow::Result<()> {
    let payload = message
        .payload()
        .ok_or_else(|| anyhow!("Kafka message has no payload"))?;
    // Combine partition and offset for a unique ID within a topic.
    // A u128 is used to hold both values, with the partition in the high 64 bits
    // and the offset in the low 64 bits.
    let message_id = ((message.partition() as u32 as u128) << 64) | (message.offset() as u64 as u128);
    let mut canonical_message = CanonicalMessage::new(payload.to_vec(), Some(message_id));
    if let Some(headers) = message.headers() {
        if headers.count() > 0 {
            let mut metadata = std::collections::HashMap::new();
            for header in headers.iter() {
                metadata.insert(
                    header.key.to_string(),
                    String::from_utf8_lossy(header.value.unwrap_or_default()).to_string(),
                );
            }
            canonical_message.metadata = Some(metadata);
        }
    }
    messages.push(canonical_message);

    // Update the topic partition list with the latest offset
    last_offset_tpl
        .add_partition_offset(
            message.topic(),
            message.partition(),
            Offset::Offset(message.offset() + 1),
        )
        .map_err(|e| anyhow::anyhow!(e))
}
