use crate::models::KafkaConfig;
use crate::traits::{
    into_bulk_commit_func, BoxFuture, BulkCommitFunc, CommitFunc, MessageConsumer, MessagePublisher,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_stream::stream;
use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};
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
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

pub struct KafkaPublisher {
    producer: FutureProducer,
    topic: String,
    skip_ack: bool,
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
            skip_ack: config.skip_ack,
        })
    }

    pub fn with_topic(&self, topic: &str) -> Self {
        Self {
            producer: self.producer.clone(),
            topic: topic.to_string(),
            skip_ack: self.skip_ack,
        }
    }

    /// Flushes any buffered messages and disconnects the producer.
    /// This method takes ownership to ensure it's the final action.
    pub async fn disconnect(&self) {
        info!(
            topic = %self.topic,
            "Flushing and disconnecting Kafka producer..."
        );
        if let Err(e) = self.producer.flush(Duration::from_secs(10)) {
            tracing::warn!("Kafka producer flush failed during disconnect: {:?}", e);
        }
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

        let key = if let Some(id) = &message.message_id {
            id.to_string()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        record = record.key(&key);

        if !self.skip_ack {
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

    async fn send_bulk(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        crate::traits::send_bulk_helper(self, messages, |publisher, message| {
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

type KafkaMessageStream = Pin<
    Box<
        dyn Stream<Item = Result<rdkafka::message::OwnedMessage, rdkafka::error::KafkaError>>
            + Send,
    >,
>;

pub struct KafkaConsumer {
    // The consumer needs to be stored to keep the connection alive.
    consumer: Arc<StreamConsumer>,
    // The stream is created from the consumer and wrapped in a Mutex for safe access.
    stream: Mutex<KafkaMessageStream>,
}
use std::any::Any;

impl KafkaConsumer {
    pub fn new(config: &KafkaConfig, topic: &str) -> anyhow::Result<Self> {
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
        // Create a stream that loops, calling `recv()` on the consumer.
        // This avoids the complex lifetime issues with `consumer.stream()`.
        let stream = {
            let consumer = consumer.clone();
            stream! {
                loop {
                    yield consumer.recv().await.map(|m| m.detach());
                }
            }
        }
        .boxed();

        Ok(Self {
            consumer,
            stream: Mutex::new(stream),
        })
    }
    /// Unsubscribes the consumer from all topics.
    /// This method takes ownership to ensure it's the final action.
    pub fn disconnect(&self) {
        info!("Disconnecting Kafka consumer by unsubscribing...");
        self.consumer.unsubscribe();
    }
}

#[async_trait]
impl MessageConsumer for KafkaConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let message = self
            .stream
            .get_mut()
            .unwrap()
            .next()
            .await
            .ok_or_else(|| anyhow!("Kafka consumer stream ended"))??;

        let payload = message
            .payload()
            .ok_or_else(|| anyhow!("Kafka message has no payload"))?;
        let mut canonical_message = CanonicalMessage::new(payload.to_vec());

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

        // The commit function for Kafka needs to commit the offset of the processed message.
        // We can't move `self.consumer` into the closure, but we can commit by position.
        let consumer = self.consumer.clone();
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset(
            message.topic(),
            message.partition(),
            Offset::Offset(message.offset() + 1),
        )?;
        let commit = Box::new(move |_response: Option<CanonicalMessage>| {
            Box::pin(async move {
                if let Err(e) = consumer.commit(&tpl, CommitMode::Async) {
                    tracing::error!("Failed to commit Kafka message: {:?}", e);
                }
            }) as BoxFuture<'static, ()>
        });

        Ok((canonical_message, commit))
    }

    async fn receive_bulk(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BulkCommitFunc)> {
        let (msg, commit) = self.receive().await?;
        let commit = into_bulk_commit_func(commit);
        Ok((vec![msg], commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
