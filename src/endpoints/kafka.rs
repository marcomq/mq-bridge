use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::KafkaConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, CommitDisposition, ConsumerError, EndpointStatus, MessageConsumer,
    MessageDisposition, MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
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
use tracing::{debug, info, trace};
use uuid::Uuid;

pub struct KafkaPublisher {
    producer: FutureProducer,
    topic: String,
    delayed_ack: bool,
}

impl KafkaPublisher {
    pub async fn new(config: &KafkaConfig) -> anyhow::Result<Self> {
        let topic = config.topic.as_deref().unwrap_or("");
        if config.delayed_ack {
            tracing::warn!("Kafka 'delayed_ack' is enabled. Messages are acknowledged before broker confirmation. This carries a risk of data loss in the event of a crash.");
        }

        let mut client_config = create_common_config(config);
        client_config
            // --- Performance Tuning ---
            .set("linger.ms", "100") // Wait 100ms to batch messages for reliability
            .set("batch.num.messages", "10000") // Max messages per batch.
            .set("compression.type", "lz4") // Efficient compression.
            // --- Reliability ---
            .set("acks", "all") // Wait for all in-sync replicas (safer)
            .set("retries", "3") // Retry up to 3 times
            .set("request.timeout.ms", "30000"); // 30 second timeout

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
                        info!(topic = %topic_name, "Kafka topic created successfully")
                    }
                    Err((topic_name, error_code)) => {
                        if error_code == RDKafkaErrorCode::TopicAlreadyExists {
                            debug!(topic = %topic_name, "Kafka topic already exists, skipping creation.");
                        } else {
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
}

impl Drop for KafkaPublisher {
    /// On drop, attempt a non-blocking flush.
    /// This is a best-effort attempt. For guaranteed delivery, call `disconnect()` explicitly.
    fn drop(&mut self) {
        debug!("KafkaPublisher dropped, attempting to flush remaining messages.");
        self.producer.flush(Duration::from_secs(5)).ok(); // Non-blocking flush
    }
}

#[async_trait]
impl MessagePublisher for KafkaPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            topic = %self.topic,
            message_id = %format!("{:032x}", message.message_id),
            payload_size = message.payload.len(),
            "Publishing Kafka message"
        );
        let mut record = FutureRecord::to(&self.topic).payload(&message.payload[..]);

        let mut headers = OwnedHeaders::new();
        headers = headers.insert(rdkafka::message::Header {
            key: "mq_bridge.message_id",
            value: Some(format!("{:032x}", message.message_id).as_bytes()),
        });

        if !message.metadata.is_empty() {
            for (key, value) in &message.metadata {
                headers = headers.insert(rdkafka::message::Header {
                    key,
                    value: Some(value.as_bytes()),
                });
            }
        }
        record = record.headers(headers);

        let key = message.message_id.to_be_bytes().to_vec();
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
            self.producer
                .send_result(record)
                .map_err(|(e, _)| anyhow!("Failed to enqueue Kafka message: {}", e))?;
        }
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(
            topic = %self.topic,
            count = messages.len(),
            message_ids = ?LazyMessageIds(&messages),
            "Publishing batch of Kafka messages"
        );
        if self.delayed_ack {
            return crate::traits::send_batch_helper(
                self,
                messages,
                MessageDisposition::Ack,
                |publisher, message| Box::pin(publisher.send(message)),
            )
            .await;
        }

        let mut delivery_futures = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();

        let mut iter = messages.into_iter();
        while let Some(message) = iter.next() {
            let mut record = FutureRecord::to(&self.topic).payload(&message.payload[..]);
            let key_bytes = message.message_id.to_be_bytes();
            record = record.key(&key_bytes);

            let mut headers = OwnedHeaders::new();
            headers = headers.insert(rdkafka::message::Header {
                key: "mq_bridge.message_id",
                value: Some(format!("{:032x}", message.message_id).as_bytes()),
            });

            if !message.metadata.is_empty() {
                for (key, value) in &message.metadata {
                    headers = headers.insert(rdkafka::message::Header {
                        key,
                        value: Some(value.as_bytes()),
                    });
                }
            }
            record = record.headers(headers);

            match self.producer.send_result(record) {
                Ok(fut) => delivery_futures.push((message, fut)),
                Err((e, _)) => {
                    failed_messages.push((
                        message,
                        PublisherError::Retryable(anyhow!("Kafka enqueue failed: {}", e)),
                    ));
                    // Abort the batch to preserve ordering.
                    // If we continued, subsequent messages might succeed while this one failed,
                    // causing out-of-order delivery on retry.
                    for skipped_msg in iter {
                        failed_messages.push((
                            skipped_msg,
                            PublisherError::Retryable(anyhow!(
                                "Batch aborted due to previous error"
                            )),
                        ));
                    }
                    break;
                }
            }
        }

        for (message, fut) in delivery_futures {
            match fut.await {
                Ok(Ok(_)) => {}
                Ok(Err((e, _))) => failed_messages.push((
                    message,
                    PublisherError::Retryable(anyhow!("Kafka delivery failed: {}", e)),
                )),
                Err(_) => failed_messages.push((
                    message,
                    PublisherError::Retryable(anyhow!("Kafka delivery future cancelled")),
                )),
            }
        }

        if failed_messages.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
                commit: None,
            })
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.producer
            .flush(Duration::from_secs(10))
            .map_err(|e| anyhow!("Kafka flush error: {}", e))
    }

    async fn status(&self) -> EndpointStatus {
        let producer = self.producer.clone();
        let topic = self.topic.clone();
        let (healthy, pending, error) = tokio::task::spawn_blocking(move || {
            let meta_topic = if topic.is_empty() {
                None
            } else {
                Some(topic.as_str())
            };
            let (healthy, error) = match producer
                .client()
                .fetch_metadata(meta_topic, Duration::from_secs(1))
            {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            };
            let pending = producer.in_flight_count() as usize;
            (healthy, pending, error)
        })
        .await
        .unwrap_or((false, 0, Some("status task panicked".to_string())));

        EndpointStatus {
            healthy,
            error,
            target: self.topic.clone(),
            pending: Some(pending),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
pub struct KafkaConsumer {
    // The consumer needs to be stored to keep the connection alive.
    consumer: Arc<StreamConsumer>,
    producer: Option<FutureProducer>,
    topic: String,
}
use std::any::Any;

impl KafkaConsumer {
    pub async fn new(config: &KafkaConfig) -> anyhow::Result<Self> {
        let topic = config.topic.as_deref().unwrap_or("");
        let mut client_config = create_common_config(config);

        let is_subscriber = config.group_id.is_none();

        if is_subscriber {
            // Subscriber mode: unique group ID, start from latest.
            let id = fast_uuid_v7::gen_id_string();
            let group_id = format!("event-sub-{}", id);
            client_config.set("group.id", &group_id);
            client_config.set("auto.offset.reset", "latest"); // Start reading from the latest message
            info!(topic = %topic, group_id = %group_id, "Kafka event subscriber started");
        } else if let Some(group_id) = &config.group_id {
            // Consumer mode: shared group ID, start from earliest.
            client_config.set("group.id", group_id);
            client_config.set("auto.offset.reset", "earliest");
            info!(topic = %topic, group_id = %group_id, "Kafka source subscribed");
        } else {
            return Err(anyhow!(
                "Kafka configuration must have either a 'group_id' (for consumer) or be configured as a subscriber"
            ));
        }

        client_config
            // good defaults
            .set("fetch.min.bytes", "1") // Start fetching immediately
            .set("socket.connection.setup.timeout.ms", "30000") // 30 seconds
            .set("enable.auto.commit", "false");

        // Apply custom consumer options
        if let Some(options) = &config.consumer_options {
            for (key, value) in options {
                client_config.set(key, value);
            }
        }

        let consumer: StreamConsumer = client_config.create()?;
        if !topic.is_empty() {
            consumer.subscribe(&[topic])?
        }

        // Wrap the consumer in an Arc to allow it to be shared.
        let consumer = Arc::new(consumer);

        // Create a producer for sending replies, but only for consumers, not subscribers.
        let producer = if !is_subscriber {
            let mut producer_config = create_common_config(config);
            // Apply similar defaults as KafkaPublisher for reliability
            producer_config
                .set("linger.ms", "100")
                .set("batch.num.messages", "10000")
                .set("compression.type", "lz4")
                .set("acks", "all")
                .set("retries", "3")
                .set("request.timeout.ms", "30000");
            // Apply custom producer options, allowing overrides of defaults
            if let Some(options) = &config.producer_options {
                for (key, value) in options {
                    producer_config.set(key, value);
                }
            }
            let producer: FutureProducer = producer_config.create()?;
            Some(producer)
        } else {
            None
        };

        Ok(Self {
            consumer,
            producer,
            topic: topic.to_string(),
        })
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
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let message = self
            .consumer
            .recv()
            .await
            .context("Failed to receive Kafka message")?;
        let mut tpl = TopicPartitionList::new();
        let mut messages = Vec::new();
        process_message(&message, &mut messages, &mut tpl)?;
        let canonical_message = messages.pop().unwrap();

        let reply_topic = canonical_message.metadata.get("reply_to").cloned();
        let correlation_id = canonical_message.metadata.get("correlation_id").cloned();

        // The commit function for Kafka needs to commit the offset of the processed message.
        // We can't move `self.consumer` into the closure, but we can commit by position.
        let tpl = Arc::new(std::sync::Mutex::new(Some(tpl)));
        let consumer_clone = self.consumer.clone();
        let producer_clone = self.producer.clone();

        let commit = Box::new(move |disposition: MessageDisposition| {
            let tpl = tpl.clone();
            let consumer_clone = consumer_clone.clone();
            let producer_clone = producer_clone.clone();
            let reply_topic = reply_topic.clone();
            let correlation_id = correlation_id.clone();
            Box::pin(async move {
                // Handle reply
                if matches!(disposition, MessageDisposition::Nack) {
                    return Ok(());
                }

                if let Some(producer) = producer_clone {
                    if let (MessageDisposition::Reply(resp), Some(rt)) = (&disposition, reply_topic)
                    {
                        let mut record: FutureRecord<'_, (), _> = // '
                            FutureRecord::to(&rt).payload(&resp.payload[..]);
                        let mut headers = OwnedHeaders::new();
                        if let Some(cid) = correlation_id {
                            headers = headers.insert(rdkafka::message::Header {
                                key: "correlation_id",
                                value: Some(cid.as_bytes()),
                            });
                        }
                        record = record.headers(headers);

                        if let Err((e, _)) = producer.send(record, Duration::from_secs(0)).await {
                            tracing::error!(topic = %rt, error = %e, "Failed to publish Kafka reply");
                        }
                    }
                }

                // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
                if let Some(tpl_data) = tpl.lock().unwrap().take() {
                    if let Err(e) = consumer_clone.commit(&tpl_data, CommitMode::Async) {
                        tracing::error!("Failed to commit Kafka message: {:?}", e);
                        return Err(anyhow!("Failed to commit Kafka message: {:?}", e));
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(Received {
            message: canonical_message,
            commit,
        })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        receive_batch_internal(
            &self.consumer,
            self.producer.as_ref(),
            max_messages,
            &self.topic,
        )
        .await
    }

    async fn status(&self) -> EndpointStatus {
        let consumer = self.consumer.clone();
        let topic = self.topic.clone();

        let (healthy, pending, error) = tokio::task::spawn_blocking(move || {
            let meta_topic = if topic.is_empty() {
                None
            } else {
                Some(topic.as_str())
            };
            let (mut healthy, mut error) = match consumer
                .client()
                .fetch_metadata(meta_topic, Duration::from_secs(1))
            {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            };

            let mut total_lag = 0;
            if healthy {
                if let Ok(tpl) = consumer.assignment() {
                    // Fetch local position (next offset to read)
                    match consumer.position() {
                        Ok(position_tpl) => {
                            for partition in tpl.elements() {
                                let p_id = partition.partition();
                                let t_name = partition.topic();

                                if let Some(pos_elem) = position_tpl.find_partition(t_name, p_id) {
                                    if let rdkafka::Offset::Offset(current) = pos_elem.offset() {
                                        // Fetch high watermark from broker (latest offset)
                                        match consumer.fetch_watermarks(
                                            t_name,
                                            p_id,
                                            Duration::from_secs(1),
                                        ) {
                                            Ok((_low, high)) => {
                                                if high > current {
                                                    total_lag += (high - current) as usize;
                                                }
                                            }
                                            Err(e) => {
                                                error = Some(format!(
                                                    "Failed to fetch watermarks: {}",
                                                    e
                                                ));
                                                healthy = false;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error = Some(format!("Failed to get consumer position: {}", e));
                            healthy = false;
                        }
                    }
                }
            }
            (healthy, total_lag, error)
        })
        .await
        .unwrap_or((false, 0, Some("status task panicked".to_string())));

        EndpointStatus {
            healthy,
            target: self.topic.clone(),
            pending: if healthy { Some(pending) } else { None },
            error,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Helper function to process a Kafka message and add it to the batch.
fn process_message<M: Message>(
    message: &M,
    messages: &mut Vec<CanonicalMessage>,
    last_offset_tpl: &mut TopicPartitionList,
) -> anyhow::Result<()> {
    let payload = message
        .payload()
        .ok_or_else(|| anyhow!("Kafka message has no payload"))?;

    // Try to extract message_id from the Kafka key first (where we store it when publishing).
    // The key is set to message_id.to_be_bytes() which is 16 bytes for a u128.
    let mut message_id: Option<u128> = None;
    if let Some(key) = message.key() {
        if key.len() == 16 {
            // Parse the key as a u128 (big-endian bytes)
            // unwrap is safe: length check guarantees exactly 16 bytes
            let bytes: [u8; 16] = key.try_into().unwrap();
            message_id = Some(u128::from_be_bytes(bytes));
        }
    }

    // If no message_id from key, check headers for a message_id
    if message_id.is_none() {
        if let Some(headers) = message.headers() {
            for header in headers.iter() {
                if header.key == "message_id" || header.key == "mq_bridge.message_id" {
                    if let Some(value) = header.value {
                        let id_str = String::from_utf8_lossy(value);
                        // Try to parse as UUID first
                        if let Ok(uuid) = Uuid::parse_str(&id_str) {
                            message_id = Some(uuid.as_u128());
                            break;
                        }
                        // Try to parse as hex string
                        else if let Ok(n) =
                            u128::from_str_radix(id_str.trim_start_matches("0x"), 16)
                        {
                            message_id = Some(n);
                            break;
                        }
                        // Try to parse as decimal string
                        else if let Ok(n) = id_str.parse::<u128>() {
                            message_id = Some(n);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Fall back to partition+offset if no message_id found
    // Combine partition and offset for a unique ID within a topic.
    // A u128 is used to hold both values, with the partition in the high 64 bits
    // and the offset in the low 64 bits.
    let message_id = message_id.unwrap_or_else(|| {
        ((message.partition() as u32 as u128) << 64) | (message.offset() as u64 as u128)
    });

    let mut canonical_message = CanonicalMessage::new(payload.to_vec(), Some(message_id));

    // Process headers into metadata
    if let Some(headers) = message.headers() {
        if headers.count() > 0 {
            let mut metadata = std::collections::HashMap::new();
            for header in headers.iter() {
                metadata.insert(
                    header.key.to_string(),
                    String::from_utf8_lossy(header.value.unwrap_or_default()).to_string(),
                );
            }
            canonical_message.metadata = metadata;
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
        .map_err(|e| anyhow!(e))
}

fn create_common_config(config: &KafkaConfig) -> ClientConfig {
    let mut client_config = ClientConfig::new();
    client_config.set("bootstrap.servers", &config.url);

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
    client_config
}

async fn receive_batch_internal(
    consumer: &Arc<StreamConsumer>,
    producer: impl Into<Option<&FutureProducer>>,
    max_messages: usize,
    topic: &str,
) -> Result<ReceivedBatch, ConsumerError> {
    let mut messages = Vec::with_capacity(max_messages);
    let mut last_offset_tpl = TopicPartitionList::new();
    let mut reply_infos = Vec::with_capacity(max_messages);

    {
        let stream = consumer.stream();
        // Use ready_chunks to efficiently fetch a batch of available messages.
        // This waits for at least one message, then consumes all currently available messages up to max_messages.
        let mut chunk_stream = stream.ready_chunks(max_messages);

        if let Some(chunk) = chunk_stream.next().await {
            for message_result in chunk {
                match message_result {
                    Ok(message) => {
                        process_message(&message, &mut messages, &mut last_offset_tpl)?;
                        // process_message pushes to messages, so we can peek the last one
                        if let Some(last_msg) = messages.last() {
                            reply_infos.push((
                                last_msg.metadata.get("reply_to").cloned(),
                                last_msg.metadata.get("correlation_id").cloned(),
                            ));
                        }
                    }
                    Err(e) => return Err(anyhow!(e).into()),
                }
            }
        } else {
            return Err(ConsumerError::EndOfStream);
        }
    }
    let messages_len = messages.len();
    trace!(count = messages_len, topic = %topic, message_ids = ?LazyMessageIds(&messages), "Received batch of Kafka messages");

    let consumer = consumer.clone();
    let producer = producer.into().cloned();

    let last_offset_tpl = Arc::new(std::sync::Mutex::new(Some(last_offset_tpl)));
    let reply_infos_mutex = Arc::new(std::sync::Mutex::new(Some(reply_infos)));

    let commit = Box::new(move |disposition: CommitDisposition| {
        let consumer = consumer.clone();
        let producer = producer.clone();
        let reply_infos_mutex = reply_infos_mutex.clone();
        let last_offset_tpl = last_offset_tpl.clone();
        Box::pin(async move {
            // Handle replies
            let reply_infos = reply_infos_mutex.lock().unwrap().take().unwrap_or_default();
            let dispositions = match disposition {
                CommitDisposition::All(d) => vec![d; reply_infos.len()],
                CommitDisposition::Individual(v) => v,
            };
            let any_nack = dispositions
                .iter()
                .any(|d| matches!(d, MessageDisposition::Nack));

            handle_kafka_replies(producer, &reply_infos, dispositions).await;

            // Only commit if there are offsets to commit AND no messages were Nacked.
            // If any message is Nacked, we skip the commit for the whole batch to ensure at-least-once delivery.
            if !any_nack && messages_len > 0 {
                // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
                if let Some(tpl) = last_offset_tpl.lock().unwrap().take() {
                    if let Err(e) = consumer.commit(&tpl, CommitMode::Async) {
                        tracing::error!("Failed to commit Kafka message batch: {:?}", e);
                        return Err(anyhow::anyhow!(
                            "Failed to commit Kafka message batch: {:?}",
                            e
                        ));
                    }
                }
            }
            Ok(())
        }) as BoxFuture<'static, anyhow::Result<()>>
    }) as BatchCommitFunc;
    Ok(ReceivedBatch { messages, commit })
}

async fn handle_kafka_replies(
    producer: Option<FutureProducer>,
    reply_infos: &[(Option<String>, Option<String>)],
    dispositions: Vec<MessageDisposition>,
) {
    if let Some(prod) = producer {
        if dispositions.len() != reply_infos.len() {
            tracing::warn!(
                expected = reply_infos.len(),
                actual = dispositions.len(),
                "Response count mismatch with received messages"
            );
        }
        for ((reply_topic, correlation_id), disposition) in
            reply_infos.iter().zip(dispositions.into_iter())
        {
            if let MessageDisposition::Reply(resp) = disposition {
                if let Some(rt) = reply_topic {
                    let mut record: FutureRecord<'_, (), _> =
                        FutureRecord::to(rt).payload(&resp.payload[..]);
                    let mut headers = OwnedHeaders::new();
                    if let Some(cid) = correlation_id {
                        headers = headers.insert(rdkafka::message::Header {
                            key: "correlation_id",
                            value: Some(cid.as_bytes()),
                        });
                    }
                    record = record.headers(headers);

                    if let Err((e, _)) = prod.send(record, Duration::from_secs(0)).await {
                        tracing::error!(topic = %rt, error = %e, "Failed to publish Kafka reply");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdkafka::message::{Header, OwnedMessage};

    // A helper to create a mock message for testing process_message
    fn create_mock_message(
        payload: Option<&[u8]>,
        key: Option<&[u8]>,
        headers: Option<OwnedHeaders>,
        offset: i64,
        partition: i32,
    ) -> OwnedMessage {
        OwnedMessage::new(
            payload.map(|p| p.to_vec()),
            key.map(|k| k.to_vec()),
            "test_topic".to_string(),
            rdkafka::Timestamp::now(),
            partition,
            offset,
            headers,
        )
    }

    #[test]
    fn test_process_message_id_from_key() {
        let message_id = 0x1234567890abcdef1234567890abcdef_u128;
        let key = message_id.to_be_bytes();
        let msg = create_mock_message(Some(b"payload"), Some(&key), None, 0, 0);

        let mut messages = Vec::new();
        let mut tpl = TopicPartitionList::new();
        process_message(&msg, &mut messages, &mut tpl).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, message_id);
    }

    #[test]
    fn test_process_message_id_from_header_uuid() {
        let uuid = fast_uuid_v7::gen_id();
        let headers = OwnedHeaders::new().insert(Header {
            key: "message_id",
            value: Some(fast_uuid_v7::format_uuid(uuid).to_string().as_bytes()),
        });
        let msg = create_mock_message(Some(b"payload"), None, Some(headers), 0, 0);

        let mut messages = Vec::new();
        let mut tpl = TopicPartitionList::new();
        process_message(&msg, &mut messages, &mut tpl).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, uuid);
    }

    #[test]
    fn test_process_message_id_fallback_to_offset() {
        // No key, no headers with message_id
        let msg = create_mock_message(Some(b"payload"), None, None, 123, 4);
        let partition = msg.partition();
        let offset = msg.offset();

        let mut messages = Vec::new();
        let mut tpl = TopicPartitionList::new();
        process_message(&msg, &mut messages, &mut tpl).unwrap();

        let expected_id = ((partition as u32 as u128) << 64) | (offset as u64 as u128);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, expected_id);
        // Check that the TPL was updated correctly
        let committed_offset = tpl.find_partition("test_topic", 4).unwrap().offset();
        assert_eq!(committed_offset, Offset::Offset(124));
    }
}
