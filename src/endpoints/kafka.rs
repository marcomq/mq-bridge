use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::KafkaConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
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
    consumer::{BaseConsumer, CommitMode, Consumer, ConsumerContext, Rebalance},
    error::{KafkaError, RDKafkaErrorCode},
    message::Headers,
    ClientConfig, ClientContext, Message, TopicPartitionList,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, trace};
use uuid::Uuid;

/// How long a `delayed_ack` send waits for room in a full local producer queue.
const QUEUE_FULL_BACKOFF: Duration = Duration::from_millis(10);

/// After this, a message that still finds the queue full fails as retryable. Longer than the
/// default `message.timeout.ms` (300 s), so a broker outage only waits.
const QUEUE_FULL_TIMEOUT: Duration = Duration::from_secs(330);

/// Shared rdkafka producer. The flush on `Drop` runs only when the last holder is
/// dropped, so publishers sharing this producer don't flush each other's buffers early.
pub struct SharedKafkaProducer {
    producer: FutureProducer,
}

impl std::ops::Deref for SharedKafkaProducer {
    type Target = FutureProducer;
    fn deref(&self) -> &FutureProducer {
        &self.producer
    }
}

impl Drop for SharedKafkaProducer {
    fn drop(&mut self) {
        debug!("Shared Kafka producer dropped, attempting to flush remaining messages.");
        self.producer.flush(Duration::from_secs(5)).ok();
    }
}

pub struct KafkaPublisher {
    producer: Arc<SharedKafkaProducer>,
    topic: String,
    delayed_ack: bool,
    // Metadata field whose value is used as the record key; falls back to message_id.
    partition_key: Option<String>,
}

impl KafkaPublisher {
    pub async fn new(config: &KafkaConfig) -> anyhow::Result<Self> {
        let topic = config.topic.as_deref().unwrap_or("");
        if topic.contains(',') || is_topic_pattern(topic) {
            return Err(anyhow!(
                "A Kafka output writes to one topic; '{topic}' is a list or pattern, which only a source accepts"
            ));
        }
        if config.delayed_ack {
            tracing::warn!("Kafka 'delayed_ack' is enabled. Messages are acknowledged before broker confirmation. This carries a risk of data loss in the event of a crash.");
        }

        let mut client_config = create_common_config(config);
        client_config
            // --- Performance Tuning ---
            // An awaited batch waits out the linger before it is sent: 1 ms keeps it in one
            // request per partition (0 splits it). Nothing waits with `delayed_ack`.
            .set("linger.ms", if config.delayed_ack { "5" } else { "1" })
            .set("batch.num.messages", "10000") // Max messages per batch.
            .set("compression.type", "lz4") // Efficient compression.
            // --- Reliability ---
            // Idempotent producer: keeps acks=all but lets librdkafka pipeline up to 5
            // in-flight requests per connection while the broker dedupes retries via
            // sequence numbers. Strictly safer than plain retries (no dup/reorder on retry)
            // *and* faster, because batches no longer have to be sent strictly one-at-a-time.
            .set("enable.idempotence", "true")
            .set("acks", "all") // Required by idempotence; waits for all in-sync replicas
            .set("request.timeout.ms", "30000"); // 30 second timeout

        // Apply custom producer options, allowing overrides of defaults
        if let Some(options) = &config.producer_options {
            for (key, value) in options {
                client_config.set(key, value);
            }
            // Idempotence requires acks=all; if the caller weakened acks, relax
            // idempotence too (unless they set it explicitly) so librdkafka accepts the config.
            let weakened_acks = options
                .iter()
                .any(|(k, v)| k == "acks" && v != "all" && v != "-1");
            let set_idempotence = options.iter().any(|(k, _)| k == "enable.idempotence");
            if weakened_acks && !set_idempotence {
                client_config.set("enable.idempotence", "false");
            }
        }

        // Create the topic if it doesn't exist
        if !topic.is_empty() && config.create_topic.unwrap_or(true) {
            let admin_client: AdminClient<_> = client_config.create()?;
            let partitions = config
                .partitions
                .unwrap_or(crate::models::DEFAULT_KAFKA_PARTITIONS);
            let new_topic = NewTopic::new(topic, partitions, BROKER_DEFAULT_REPLICATION);
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
                        } else if topic_may_exist(error_code) {
                            tracing::warn!(topic = %topic_name, error = %error_code, "Not allowed to create the Kafka topic; assuming it exists");
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

        // Share one producer across publishers with matching connection settings: the topic is
        // per-record, so one producer serves all topics and consolidates connections, the poll
        // thread, and batching. Cache key = producer-level settings (creds, TLS, producer_options);
        // sort producer_options first so order-different-but-equivalent configs still share.
        let producer_options = config.producer_options.as_ref().map(|opts| {
            let mut sorted = opts.clone();
            sorted.sort();
            sorted
        });
        let identity = crate::support::connection_registry::connection_identity((
            &config.url,
            &config.username,
            &config.password,
            config.tls.required,
            &config.tls.ca_file,
            &config.tls.cert_file,
            &config.tls.key_file,
            &config.tls.cert_password,
            config.tls.accept_invalid_certs,
            &producer_options,
            // Sets the default `linger.ms`, so the two modes cannot share a producer.
            config.delayed_ack,
        ));
        let shared = config.shared.unwrap_or(true);
        let producer = crate::support::connection_registry::get_or_create(
            "kafka-producer",
            identity,
            shared,
            move || async move {
                let producer: FutureProducer = client_config
                    .create()
                    .context("Failed to create Kafka producer")?;
                Ok(SharedKafkaProducer { producer })
            },
        )
        .await?;

        Ok(Self {
            producer,
            topic: topic.to_string(),
            delayed_ack: config.delayed_ack,
            partition_key: config.partition_key.clone(),
        })
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

        record = record.headers(message_headers(&message));

        // Key on the configured metadata field when set (and present on this message);
        // otherwise fall back to message_id, which the consumer also recovers from the
        // mq_bridge.message_id header set above.
        let key = record_key(self.partition_key.as_deref(), &message);
        record = record.key(&key);

        // A tombstone must go out with a null value, not a zero-length one, or the broker
        // stores an empty record and the key is never compacted away.
        if is_tombstone(&message) {
            record.payload = None;
        }

        if !self.delayed_ack {
            // Await the delivery report from Kafka, providing at-least-once guarantees per message.
            self.producer
                .send(record, Duration::from_secs(0))
                .await
                .map_err(|(e, _)| delivery_error("Kafka message delivery failed", e))?;
        } else {
            // "Fire and forget" send. This enqueues the message in the producer's buffer.
            // The `FutureProducer` will handle sending it in the background according to the
            // `linger.ms` and other batching settings. We don't await the delivery report
            // here to achieve high throughput. The `flush()` in `Drop` ensures all messages
            // are sent before shutdown. A full queue is backpressure: wait for room.
            let deadline = tokio::time::Instant::now() + QUEUE_FULL_TIMEOUT;
            loop {
                match self.producer.send_result(record) {
                    Ok(_) => break,
                    Err((e @ KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull), _))
                        if tokio::time::Instant::now() >= deadline =>
                    {
                        return Err(delivery_error("Kafka producer queue stayed full", e));
                    }
                    Err((KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull), back)) => {
                        record = back;
                        tokio::time::sleep(QUEUE_FULL_BACKOFF).await;
                    }
                    Err((e, _)) => {
                        return Err(delivery_error("Failed to enqueue Kafka message", e));
                    }
                }
            }
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
            return crate::traits::send_batch_helper(self, messages, |publisher, message| {
                Box::pin(publisher.send(message))
            })
            .await;
        }

        let mut delivery_futures = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();

        let mut iter = messages.into_iter();
        while let Some(message) = iter.next() {
            let mut record = FutureRecord::to(&self.topic).payload(&message.payload[..]);
            // Key on the configured metadata field when set (and present on this message);
            // otherwise fall back to message_id, which the consumer also recovers from the
            // mq_bridge.message_id header set below.
            let key_bytes = record_key(self.partition_key.as_deref(), &message);
            record = record.key(&key_bytes);

            record = record.headers(message_headers(&message));
            // See `send`: a tombstone needs a null value, not an empty one.
            if is_tombstone(&message) {
                record.payload = None;
            }

            match self.producer.send_result(record) {
                Ok(fut) => delivery_futures.push((message, fut)),
                Err((e, _)) => {
                    let permanent = is_permanent_delivery_error(&e);
                    failed_messages.push((message, delivery_error("Kafka enqueue failed", e)));
                    // Never retried, so skipping it cannot reorder anything.
                    if permanent {
                        continue;
                    }
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
                Ok(Err((e, _))) => {
                    failed_messages.push((message, delivery_error("Kafka delivery failed", e)))
                }
                Err(_) => failed_messages.push((
                    message,
                    PublisherError::Retryable(anyhow!("Kafka delivery future cancelled")),
                )),
            }
        }

        Ok(SentBatch::from_failures(failed_messages))
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

/// `-1`: the cluster's `default.replication.factor` (Kafka 2.4 or newer).
const BROKER_DEFAULT_REPLICATION: TopicReplication<'static> = TopicReplication::Fixed(-1);

/// Whether a failed topic creation still lets the publisher start.
///
/// A client without the right to create topics is refused before the broker looks whether
/// the topic exists, so that refusal says nothing about the topic.
fn topic_may_exist(code: RDKafkaErrorCode) -> bool {
    matches!(
        code,
        RDKafkaErrorCode::TopicAlreadyExists
            | RDKafkaErrorCode::TopicAuthorizationFailed
            | RDKafkaErrorCode::ClusterAuthorizationFailed
    )
}

/// A record the broker will refuse however often it is sent.
fn is_permanent_delivery_error(error: &KafkaError) -> bool {
    matches!(
        error.rdkafka_error_code(),
        Some(RDKafkaErrorCode::MessageSizeTooLarge | RDKafkaErrorCode::InvalidMessageSize)
    )
}

fn delivery_error(what: &str, error: KafkaError) -> PublisherError {
    let permanent = is_permanent_delivery_error(&error);
    let error = anyhow!("{what}: {error}");
    if permanent {
        PublisherError::NonRetryable(error)
    } else {
        PublisherError::Retryable(error)
    }
}

/// Below the route's 5 s start window, so the cause is reported with the start failure.
const BROKER_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Keeps the end-of-partition event this consumer asks for out of the error log.
#[derive(Default)]
struct QuietEofContext {
    subscriber_starts: Option<Arc<SubscriberStarts>>,
}

/// Where a `latest` subscriber starts each partition: the high watermark read before the
/// assignment is applied, so it is never past the offset librdkafka resolves.
#[derive(Default)]
struct SubscriberStarts {
    /// Only a drain needs them; others skip the blocking lookups.
    drain: std::sync::atomic::AtomicBool,
    offsets: std::sync::Mutex<PartitionOffsets>,
}

impl ClientContext for QuietEofContext {
    fn error(&self, error: KafkaError, reason: &str) {
        if matches!(error, KafkaError::Global(RDKafkaErrorCode::PartitionEOF)) {
            trace!("librdkafka: {}: {}", error, reason);
        } else {
            error!("librdkafka: {}: {}", error, reason);
        }
    }
}

impl ConsumerContext for QuietEofContext {
    fn pre_rebalance(&self, base_consumer: &BaseConsumer<Self>, rebalance: &Rebalance<'_>) {
        let Some(starts) = &self.subscriber_starts else {
            return;
        };
        if !starts.drain.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        match rebalance {
            Rebalance::Assign(tpl) => {
                for elem in tpl.elements() {
                    let lookup = base_consumer.fetch_watermarks(
                        elem.topic(),
                        elem.partition(),
                        Duration::from_secs(1),
                    );
                    // A failed lookup records nothing, so the partition reads as not drained.
                    if let Ok((_low, high)) = lookup {
                        starts
                            .offsets
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert((elem.topic().to_string(), elem.partition()), high);
                    }
                }
            }
            Rebalance::Revoke(tpl) => {
                let mut offsets = starts.offsets.lock().unwrap_or_else(|e| e.into_inner());
                for elem in tpl.elements() {
                    offsets.remove(&(elem.topic().to_string(), elem.partition()));
                }
            }
            Rebalance::Error(_) => {}
        }
    }
}

type StreamConsumer = rdkafka::consumer::StreamConsumer<QuietEofContext>;

/// Owns a `StreamConsumer` so its close never runs on a caller's thread.
///
/// rdkafka closes the consumer in `Drop` with `while !closed() { poll(100ms) }`, which
/// never returns if the broker disappears mid-close. The last handle can be dropped from
/// anywhere — a tokio worker, runtime shutdown — so the close is handed to its own thread.
struct ClosingStreamConsumer(Option<StreamConsumer>);

impl std::ops::Deref for ClosingStreamConsumer {
    type Target = StreamConsumer;

    fn deref(&self) -> &StreamConsumer {
        self.0.as_ref().expect("consumer is taken only in Drop")
    }
}

impl Drop for ClosingStreamConsumer {
    fn drop(&mut self) {
        if let Some(consumer) = self.0.take() {
            std::thread::spawn(move || drop(consumer));
        }
    }
}

pub struct KafkaConsumer {
    // The consumer needs to be stored to keep the connection alive.
    consumer: Arc<ClosingStreamConsumer>,
    producer: Option<FutureProducer>,
    /// The subscription as configured, for logs and status.
    topic: String,
    topics: Vec<String>,
    /// Drain mode: only then does an idle fetch time out into an empty batch.
    exit_on_empty: bool,
    /// Started on first read, once the batch size is known — see [`Prefetcher`].
    prefetcher: Option<Prefetcher>,
    /// Resolved once at construction from endpoint config and the legacy fallback.
    source_metadata: bool,
    /// What the drain knows about its partitions — see [`DrainState`].
    drain_state: DrainState,
    /// Set by the first nack; see [`NackLatch`].
    nacked: NackLatch,
}

/// Kafka commits are cumulative: committing a later batch also commits every offset before
/// it, including a nacked one, which is then never redelivered. So after a nack this
/// consumer stops committing, and its next read fails, making the route reconnect and resume
/// from the last committed offset. Later batches are replayed too — duplicates, not loss.
#[derive(Clone, Default)]
struct NackLatch(Arc<NackLatchInner>);

#[derive(Default)]
struct NackLatchInner {
    tripped: std::sync::atomic::AtomicBool,
    /// Wakes a read blocked on an idle topic, so the replay doesn't wait for new traffic.
    notify: tokio::sync::Notify,
}

impl NackLatch {
    fn trip(&self) {
        self.0
            .tripped
            .store(true, std::sync::atomic::Ordering::Release);
        self.0.notify.notify_waiters();
    }

    fn is_tripped(&self) -> bool {
        self.0.tripped.load(std::sync::atomic::Ordering::Acquire)
    }

    fn check(&self) -> Result<(), ConsumerError> {
        if self.is_tripped() {
            return Err(Self::error());
        }
        Ok(())
    }

    fn error() -> ConsumerError {
        ConsumerError::Connection(anyhow!(
            "Kafka batch was nacked; reconnecting to replay from the last committed offset"
        ))
    }

    async fn tripped(&self) {
        let notified = self.0.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_tripped() {
            notified.await;
        }
    }
}

impl KafkaConsumer {
    fn prefetcher(&mut self, max_messages: usize) -> &Prefetcher {
        self.prefetcher.get_or_insert_with(|| {
            spawn_prefetcher(
                self.consumer.clone(),
                prefetch_capacity(max_messages),
                self.source_metadata,
                subscribes_many_topics(&self.topics),
            )
        })
    }
}
use std::any::Any;

impl KafkaConsumer {
    pub async fn new(config: &KafkaConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, false).await
    }

    pub async fn new_with_source_metadata(
        config: &KafkaConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        let source_metadata = crate::canonical_message::source_metadata_enabled_for_endpoint(
            source_metadata || config.source_metadata,
        );
        let topic = config.topic.as_deref().unwrap_or("");
        let topics = topic_list(topic);
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
        // Drain completion needs an event that has passed through the same ordered stream as
        // the records. `position()` alone can advance while a fetched record is still waiting
        // in librdkafka or the prefetch task, which used to make a drain silently lose its tail.
        client_config.set("enable.partition.eof", "true");
        let session_timeout = session_timeout(&client_config);
        let subscriber_starts = is_subscriber.then(Arc::<SubscriberStarts>::default);

        let consumer: StreamConsumer = client_config.create_with_context(QuietEofContext {
            subscriber_starts: subscriber_starts.clone(),
        })?;
        // librdkafka connects in the background and retries forever; probe once so an
        // unreachable broker fails the start instead of looking like an empty topic.
        let consumer = tokio::task::spawn_blocking(move || {
            consumer
                .client()
                .fetch_metadata(None, BROKER_PROBE_TIMEOUT)
                .map(|_| consumer)
        })
        .await?
        .map_err(|e| anyhow!("Kafka broker not reachable: {e}"))?;
        if !topics.is_empty() {
            consumer.subscribe(&topics.iter().map(String::as_str).collect::<Vec<_>>())?
        }

        // Wrap the consumer in an Arc to allow it to be shared.
        let consumer = Arc::new(ClosingStreamConsumer(Some(consumer)));

        // Create a producer for sending replies, but only for consumers, not subscribers.
        let producer = if !is_subscriber {
            let mut producer_config = create_common_config(config);
            // Apply similar defaults as KafkaPublisher for reliability
            producer_config
                // Replies are awaited one at a time and each waits out the linger: keep it short.
                .set("linger.ms", "1")
                .set("batch.num.messages", "10000")
                .set("compression.type", "lz4")
                .set("enable.idempotence", "true")
                .set("acks", "all")
                .set("request.timeout.ms", "30000");
            // Apply custom producer options, allowing overrides of defaults
            if let Some(options) = &config.producer_options {
                for (key, value) in options {
                    producer_config.set(key, value);
                }
                // Idempotence requires acks=all; if the caller weakened acks, relax
                // idempotence too (unless they set it explicitly) so librdkafka accepts the config.
                let weakened_acks = options
                    .iter()
                    .any(|(k, v)| k == "acks" && v != "all" && v != "-1");
                let set_idempotence = options.iter().any(|(k, _)| k == "enable.idempotence");
                if weakened_acks && !set_idempotence {
                    producer_config.set("enable.idempotence", "false");
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
            topics,
            exit_on_empty: false,
            prefetcher: None,
            source_metadata,
            drain_state: DrainState {
                session_timeout,
                subscriber_starts,
                ..DrainState::default()
            },
            nacked: NackLatch::default(),
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
        self.nacked.check()?;
        let latch = self.nacked.clone();
        // Through the prefetcher, not the consumer directly: two readers would race for
        // the same records and each would see only some of them.
        let item = tokio::select! {
            _ = latch.tripped() => return Err(NackLatch::error()),
            item = self.prefetcher(1).rx.recv() => item,
        }
        .context("Failed to receive Kafka message")?;
        let item = match item {
            Ok(item) => item,
            Err(terminal) => return Err(terminal_to_consumer_error(terminal)),
        };
        let mut last_offsets = BatchOffsets::new();
        record_offset(&mut last_offsets, &item.topic, item.partition, item.offset);
        let tpl = offsets_to_tpl(&last_offsets)?;
        let canonical_message = item.message;

        let reply_topic = canonical_message.metadata.get("reply_to").cloned();
        let correlation_id = canonical_message.metadata.get("correlation_id").cloned();

        // The commit function for Kafka needs to commit the offset of the processed message.
        // We can't move `self.consumer` into the closure, but we can commit by position.
        let consumer_clone = self.consumer.clone();
        let producer_clone = self.producer.clone();
        let nacked = self.nacked.clone();

        let commit = Box::new(move |disposition: MessageDisposition| {
            Box::pin(async move {
                if matches!(disposition, MessageDisposition::Nack) {
                    nacked.trip();
                    return Ok(());
                }
                if nacked.is_tripped() {
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
                if let Err(e) = consumer_clone.commit(&tpl, CommitMode::Async) {
                    tracing::error!("Failed to commit Kafka message: {:?}", e);
                    return Err(anyhow!("Failed to commit Kafka message: {:?}", e));
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(Received {
            message: canonical_message,
            commit,
        })
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
        if let Some(starts) = &self.drain_state.subscriber_starts {
            starts
                .drain
                .store(exit_on_empty, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.nacked.check()?;
        if self.exit_on_empty && self.topics.iter().any(|t| is_topic_pattern(t)) {
            return Err(ConsumerError::Permanent(anyhow!(
                "A Kafka drain needs explicit topics; '{}' contains a pattern, whose topics it cannot know",
                self.topic
            )));
        }
        let nacked = self.nacked.clone();
        let (consumer, producer, topic, topics, exit_on_empty) = (
            self.consumer.clone(),
            self.producer.clone(),
            self.topic.clone(),
            self.topics.clone(),
            self.exit_on_empty,
        );
        // Taken out and put back because `prefetcher()` borrows self mutably too.
        let mut drain_state = std::mem::take(&mut self.drain_state);
        let latch = nacked.clone();
        // Cancel-safe: the only await before records leave the channel is the first recv.
        let result = tokio::select! {
            _ = latch.tripped() => Err(NackLatch::error()),
            result = receive_batch_internal(
                self.prefetcher(max_messages),
                &consumer,
                producer.as_ref(),
                max_messages,
                &topic,
                &topics,
                exit_on_empty,
                &mut drain_state,
                nacked,
            ) => result,
        };
        self.drain_state = drain_state;
        result
    }

    async fn status(&self) -> EndpointStatus {
        let consumer = self.consumer.clone();
        let topics = self.topics.clone();

        let (healthy, pending, error) = tokio::task::spawn_blocking(move || {
            let meta_topic = match topics.as_slice() {
                [topic] if !is_topic_pattern(topic) => Some(topic.as_str()),
                _ => None,
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

/// The headers to publish with a message: its id, then its metadata.
///
/// The id header is what lets `to_canonical` recover the same `message_id` on the way back
/// in, so the two must agree on the format. `mqb.src.*` keys describe where a message was
/// read from and are never forwarded.
fn message_headers(message: &CanonicalMessage) -> OwnedHeaders {
    let mut headers = OwnedHeaders::new().insert(rdkafka::message::Header {
        key: "mq_bridge.message_id",
        value: Some(format!("{:032x}", message.message_id).as_bytes()),
    });
    for (key, value) in &message.metadata {
        // The tombstone flag is our own marker for a null value; it is re-expressed by
        // omitting the payload, not by shipping a header the next consumer would keep.
        if key == KAFKA_TOMBSTONE_KEY || crate::canonical_message::is_source_metadata_key(key) {
            continue;
        }
        headers = headers.insert(rdkafka::message::Header {
            key,
            value: Some(value.as_bytes()),
        });
    }
    headers
}

/// Choose the Kafka record key for a message: the value of the configured metadata field
/// when set and present, otherwise the message_id as big-endian bytes.
fn record_key(partition_key: Option<&str>, message: &CanonicalMessage) -> Vec<u8> {
    partition_key
        .and_then(|f| message.metadata.get(f))
        .map(|v| v.as_bytes().to_vec())
        .unwrap_or_else(|| message.message_id.to_be_bytes().to_vec())
}

/// True when the consumer flagged this message as a compacted-topic tombstone. Publishing it
/// with an empty payload would write a zero-length value, which does not delete the key.
fn is_tombstone(message: &CanonicalMessage) -> bool {
    message
        .metadata
        .get(KAFKA_TOMBSTONE_KEY)
        .is_some_and(|v| v == "true")
}

/// The topics of a subscription. Kafka topic names cannot contain a comma; a comma inside
/// `{}`, `[]` or `()` belongs to a `^regex` entry and does not split it.
fn topic_list(topic: &str) -> Vec<String> {
    let mut topics = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in topic.char_indices() {
        match c {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                topics.push(&topic[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    topics.push(&topic[start..]);
    topics
        .into_iter()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// More than one topic can reach this source, so partition+offset alone is not unique.
fn subscribes_many_topics(topics: &[String]) -> bool {
    topics.len() > 1 || topics.iter().any(|t| t.starts_with('^'))
}

/// librdkafka subscribes to every topic matching a name that starts with `^`, a character
/// no topic name may contain.
fn is_topic_pattern(topic: &str) -> bool {
    topic.trim_start().starts_with('^')
}

/// Helper function to process a Kafka message and add it to the batch.
/// One record pulled ahead of the pipeline, with the position needed to commit it.
struct Prefetched {
    message: CanonicalMessage,
    topic: String,
    partition: i32,
    offset: i64,
    /// Released when this record leaves the channel — see [`PrefetchBudget`].
    _slot: BudgetSlot,
}

/// Byte bound on the prefetch channel, on top of its slot count.
///
/// Slots alone do not bound memory: the channel holds owned payloads, so the 8192-slot floor
/// is ~8 GB at Kafka's default 1 MB `message.max.bytes`. A paused sink used to be able to
/// grow the buffer that far before any backpressure applied.
struct PrefetchBudget {
    limit: usize,
    used: std::sync::atomic::AtomicUsize,
    released: tokio::sync::Notify,
}

impl PrefetchBudget {
    /// Holds the reader back while the queued payloads exceed the budget. One record is
    /// always admitted, so a payload larger than the whole budget cannot wedge the stream.
    async fn acquire(&self, bytes: usize) {
        use std::sync::atomic::Ordering;
        loop {
            // Register before the check: a release between the two would otherwise be missed.
            let released = self.released.notified();
            if self.used.load(Ordering::Acquire) < self.limit {
                break;
            }
            released.await;
        }
        self.used.fetch_add(bytes, Ordering::AcqRel);
    }
}

/// One record's share of the prefetch budget, returned when the record is taken off the
/// channel (or when a dropped channel discards it).
struct BudgetSlot {
    budget: Arc<PrefetchBudget>,
    bytes: usize,
}

impl Drop for BudgetSlot {
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::AcqRel);
        self.budget.released.notify_waiters();
    }
}

/// Why the prefetch task stopped producing records.
enum PrefetchError {
    EndOfStream,
    Connection(String),
}

fn terminal_to_consumer_error(terminal: PrefetchError) -> ConsumerError {
    match terminal {
        PrefetchError::EndOfStream => ConsumerError::EndOfStream,
        PrefetchError::Connection(e) => ConsumerError::Connection(anyhow!(e)),
    }
}

/// Reads librdkafka continuously into a bounded channel, for the consumer's whole life.
///
/// librdkafka only keeps requesting records while its queue is being drained; let the queue
/// sit and it stops fetching and has to re-prime, which costs far more than the pause that
/// caused it. Reading inside `receive_batch` meant every pause the pipeline took — a
/// transform, a slow sink — was a pause in fetching too, and the fetch rate collapsed to
/// well under what the broker could serve. This task owns one stream and never stops
/// reading; the channel, not librdkafka's queue, is what absorbs a downstream pause.
struct Prefetcher {
    rx: async_channel::Receiver<Result<Prefetched, PrefetchError>>,
    /// Latest EOF position observed through the ordered consumer stream, per partition.
    eof_positions: Arc<std::sync::Mutex<PartitionOffsets>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How many records may sit between librdkafka and the pipeline. Only has to cover what
/// arrives while one batch is being processed, so a few batches' worth is plenty; the cost
/// is memory, one payload copy per slot. Override via `MQ_BRIDGE_KAFKA_PREFETCH`.
fn prefetch_capacity(max_messages: usize) -> usize {
    static V: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let configured = *V.get_or_init(|| {
        std::env::var("MQ_BRIDGE_KAFKA_PREFETCH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
    });
    configured.unwrap_or_else(|| (max_messages * 4).clamp(8192, 262_144))
}

/// Payload bytes the prefetch channel may hold, whatever the slot count allows. Override via
/// `MQ_BRIDGE_KAFKA_PREFETCH_BYTES`. The default covers several full fetches per partition
/// while keeping a paused sink's buffer in the tens of megabytes rather than the gigabytes.
fn prefetch_byte_budget() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MQ_BRIDGE_KAFKA_PREFETCH_BYTES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(64 * 1024 * 1024)
    })
}

fn spawn_prefetcher(
    consumer: Arc<ClosingStreamConsumer>,
    capacity: usize,
    source_metadata: bool,
    topic_in_id: bool,
) -> Prefetcher {
    if source_metadata {
        spawn_prefetcher_with_source_metadata(consumer, capacity, topic_in_id)
    } else {
        spawn_prefetcher_without_source_metadata(consumer, capacity, topic_in_id)
    }
}

fn spawn_prefetcher_without_source_metadata(
    consumer: Arc<ClosingStreamConsumer>,
    capacity: usize,
    topic_in_id: bool,
) -> Prefetcher {
    let (tx, rx) = async_channel::bounded(capacity);
    let budget = new_budget();
    let eof_positions = Arc::new(std::sync::Mutex::new(PartitionOffsets::new()));
    let task_eof_positions = eof_positions.clone();
    let task = tokio::spawn(async move {
        // One stream for the task's whole life. Rebuilding it per batch is what starved the
        // fetch pipeline; `ready_chunks` still batches whatever is already waiting.
        let stream = consumer
            .stream()
            .map(|result| capture_partition_eof_position(result, || consumer.position()));
        let mut chunks = stream.ready_chunks(1024);
        while let Some(chunk) = chunks.next().await {
            let mut chunk_eofs = Vec::new();
            for (result, eof_position) in chunk {
                let item = match result {
                    Ok(message) => match to_canonical_with_topic_id(&message, topic_in_id) {
                        Ok(canonical) => Ok(prefetched_record(&budget, canonical, &message).await),
                        Err(e) => Err(PrefetchError::Connection(e.to_string())),
                    },
                    Err(KafkaError::PartitionEOF(partition)) => {
                        if let Some(position) = eof_position {
                            chunk_eofs.push((partition, position));
                        }
                        continue;
                    }
                    Err(e) => Err(PrefetchError::Connection(e.to_string())),
                };
                // Receiver gone: the consumer was dropped, so stop reading.
                if tx.send(item).await.is_err() {
                    return;
                }
            }
            // Do not expose a drained position until every later record that
            // `ready_chunks` polled with the EOF is visible to the receiver.
            for (partition, position) in chunk_eofs {
                record_partition_eof(&position, partition, &task_eof_positions);
            }
        }
        let _ = tx.send(Err(PrefetchError::EndOfStream)).await;
    });
    Prefetcher {
        rx,
        eof_positions,
        task,
    }
}

fn new_budget() -> Arc<PrefetchBudget> {
    Arc::new(PrefetchBudget {
        limit: prefetch_byte_budget(),
        used: std::sync::atomic::AtomicUsize::new(0),
        released: tokio::sync::Notify::new(),
    })
}

/// Charges the payload against the byte budget — waiting if the channel is already full of
/// bytes — and packages the record for the channel.
async fn prefetched_record<M: Message>(
    budget: &Arc<PrefetchBudget>,
    message: CanonicalMessage,
    raw: &M,
) -> Prefetched {
    let bytes = message.payload.len();
    budget.acquire(bytes).await;
    Prefetched {
        message,
        topic: raw.topic().to_string(),
        partition: raw.partition(),
        offset: raw.offset() + 1,
        _slot: BudgetSlot {
            budget: budget.clone(),
            bytes,
        },
    }
}

fn spawn_prefetcher_with_source_metadata(
    consumer: Arc<ClosingStreamConsumer>,
    capacity: usize,
    topic_in_id: bool,
) -> Prefetcher {
    let (tx, rx) = async_channel::bounded(capacity);
    let budget = new_budget();
    let eof_positions = Arc::new(std::sync::Mutex::new(PartitionOffsets::new()));
    let task_eof_positions = eof_positions.clone();
    let task = tokio::spawn(async move {
        let stream = consumer
            .stream()
            .map(|result| capture_partition_eof_position(result, || consumer.position()));
        let mut chunks = stream.ready_chunks(1024);
        while let Some(chunk) = chunks.next().await {
            let mut chunk_eofs = Vec::new();
            for (result, eof_position) in chunk {
                let item = match result {
                    Ok(message) => match to_canonical_with_source_metadata(&message, topic_in_id) {
                        Ok(canonical) => Ok(prefetched_record(&budget, canonical, &message).await),
                        Err(e) => Err(PrefetchError::Connection(e.to_string())),
                    },
                    Err(KafkaError::PartitionEOF(partition)) => {
                        if let Some(position) = eof_position {
                            chunk_eofs.push((partition, position));
                        }
                        continue;
                    }
                    Err(e) => Err(PrefetchError::Connection(e.to_string())),
                };
                if tx.send(item).await.is_err() {
                    return;
                }
            }
            // Do not expose a drained position until every later record that
            // `ready_chunks` polled with the EOF is visible to the receiver.
            for (partition, position) in chunk_eofs {
                record_partition_eof(&position, partition, &task_eof_positions);
            }
        }
        let _ = tx.send(Err(PrefetchError::EndOfStream)).await;
    });
    Prefetcher {
        rx,
        eof_positions,
        task,
    }
}

fn capture_partition_eof_position<T>(
    result: Result<T, KafkaError>,
    position: impl FnOnce() -> rdkafka::error::KafkaResult<TopicPartitionList>,
) -> (Result<T, KafkaError>, Option<TopicPartitionList>) {
    let eof_position = matches!(result, Err(KafkaError::PartitionEOF(_)))
        .then(position)
        .and_then(Result::ok);
    (result, eof_position)
}

/// Records an EOF only after it has travelled through the same stream as all preceding
/// records. The position was captured when the stream yielded the EOF, before
/// `ready_chunks` could fetch later records.
/// Marks a partition that reported its end before any record was consumed from it.
const EOF_BEFORE_ANY_RECORD: i64 = -1;

fn record_partition_eof(
    positions: &TopicPartitionList,
    partition: i32,
    eof_positions: &std::sync::Mutex<PartitionOffsets>,
) {
    let mut observed = eof_positions.lock().unwrap_or_else(|e| e.into_inner());
    let candidates: Vec<_> = positions
        .elements()
        .into_iter()
        .filter(|elem| elem.partition() == partition)
        .collect();
    // The EOF names no topic, so with several topics on this partition number only a
    // consumed position, true for whichever topic it is, can be recorded.
    let topic_known = candidates.len() == 1;
    for elem in candidates {
        let key = (elem.topic().to_string(), partition);
        if let Offset::Offset(offset) = elem.offset() {
            let entry = observed.entry(key).or_insert(offset);
            *entry = (*entry).max(offset);
        } else if topic_known {
            // librdkafka has no position until a record is consumed: the partition was
            // already at its end when the consumer was assigned.
            observed.entry(key).or_insert(EOF_BEFORE_ANY_RECORD);
        }
    }
}

/// The offset to commit for each `(topic, partition)` a batch touched.
///
/// Not a `TopicPartitionList`: that one *appends* on `add_partition_offset` and then scans
/// the list it just grew, so recording every message made a batch cost O(n²) and allocated
/// two `CString`s per message. A batch spans a handful of partitions, so a linear scan over
/// this stays cheap — and it stays that small.
type BatchOffsets = Vec<(String, i32, i64)>;

/// Records the offset to commit for one message, keeping only the latest per partition.
fn record_offset(offsets: &mut BatchOffsets, topic: &str, partition: i32, offset: i64) {
    match offsets
        .iter_mut()
        .find(|(t, p, _)| *p == partition && t == topic)
    {
        Some(entry) => entry.2 = offset,
        None => offsets.push((topic.to_string(), partition, offset)),
    }
}

/// Builds the list to hand to `commit`, one entry per partition.
fn offsets_to_tpl(offsets: &BatchOffsets) -> anyhow::Result<TopicPartitionList> {
    let mut tpl = TopicPartitionList::new();
    for (topic, partition, offset) in offsets {
        tpl.add_partition_offset(topic, *partition, Offset::Offset(*offset))
            .map_err(|e| anyhow!(e))?;
    }
    Ok(tpl)
}

/// The two steps the prefetch task now does inline, kept together for the tests that cover
/// id recovery and offset recording as one contract.
#[cfg(test)]
fn process_message<M: Message>(
    message: &M,
    messages: &mut Vec<CanonicalMessage>,
    last_offsets: &mut BatchOffsets,
) -> anyhow::Result<()> {
    messages.push(to_canonical(message)?);
    // Keep only the latest offset per partition; the list is built once per batch.
    record_offset(
        last_offsets,
        message.topic(),
        message.partition(),
        message.offset() + 1,
    );
    Ok(())
}

/// Metadata flag marking a compacted-topic tombstone (a record with no value).
pub const KAFKA_TOMBSTONE_KEY: &str = "mqb.kafka.tombstone";

/// Converts one Kafka record into a `CanonicalMessage`, recovering its id and headers.
///
/// A record with no value is a tombstone, which is ordinary traffic on a compacted topic —
/// it carries the delete. Rejecting it used to fail the whole batch, and since the offset
/// was never committed the route reconnected onto the same record forever, so a single
/// tombstone wedged the consumer. It becomes an empty payload flagged in metadata instead.
#[cfg(test)]
fn to_canonical<M: Message>(message: &M) -> anyhow::Result<CanonicalMessage> {
    to_canonical_with_topic_id(message, false)
}

/// `topic_in_id` makes the fallback id a 128-bit hash of topic, partition and offset;
/// single-topic ids keep the old partition+offset value.
fn to_canonical_with_topic_id<M: Message>(
    message: &M,
    topic_in_id: bool,
) -> anyhow::Result<CanonicalMessage> {
    let tombstone = message.payload().is_none();
    let payload = message.payload().unwrap_or(&[]);

    // Recover message_id, preferring the mq_bridge.message_id header (always written by this
    // publisher). Checking the header before the key means an explicit partition key that
    // happens to be 16 bytes is never mistaken for an id.
    let mut message_id: Option<u128> = None;
    if let Some(headers) = message.headers() {
        for header in headers.iter() {
            if header.key == "message_id" || header.key == "mq_bridge.message_id" {
                if let Some(value) = header.value {
                    let id_str = String::from_utf8_lossy(value);
                    // Try to parse as UUID first
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        message_id = Some(uuid.as_u128());
                        break;
                    } else if let Some(hex) = id_str
                        .strip_prefix("0x")
                        .or_else(|| id_str.strip_prefix("0X"))
                    {
                        if let Ok(n) = u128::from_str_radix(hex, 16) {
                            message_id = Some(n);
                            break;
                        }
                    }
                    // Try to parse as legacy 32-char hex string
                    else if id_str.len() == 32 && id_str.chars().all(|c| c.is_ascii_hexdigit()) {
                        if let Ok(n) = u128::from_str_radix(&id_str, 16) {
                            message_id = Some(n);
                            break;
                        }
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

    // Legacy fallback: older messages carried the id as a 16-byte big-endian Kafka key.
    if message_id.is_none() {
        if let Some(key) = message.key() {
            if key.len() == 16 {
                // unwrap is safe: length check guarantees exactly 16 bytes
                let bytes: [u8; 16] = key.try_into().unwrap();
                message_id = Some(u128::from_be_bytes(bytes));
            }
        }
    }

    // Fall back to partition+offset if no message_id found
    // Combine partition and offset for a unique ID within a topic.
    // A u128 is used to hold both values, with the partition in the high 64 bits
    // and the offset in the low 64 bits.
    let message_id = message_id.unwrap_or_else(|| {
        if topic_in_id {
            return crate::canonical_message::hashed_id(
                crate::canonical_message::string_id_hash(),
                &[
                    message.topic().as_bytes(),
                    b"\0",
                    &message.partition().to_be_bytes(),
                    &message.offset().to_be_bytes(),
                ],
            );
        }
        ((message.partition() as u32 as u128) << 64) | (message.offset() as u64 as u128)
    });

    let mut canonical_message = CanonicalMessage::new(payload.to_vec(), Some(message_id));

    // Process headers into metadata
    if let Some(headers) = message.headers() {
        if headers.count() > 0 {
            let mut metadata = std::collections::HashMap::new();
            for header in headers.iter() {
                // Never let an inbound header spoof a reserved `mqb.src.*` value;
                // the authoritative cursor keys are injected below.
                if crate::canonical_message::is_source_metadata_key(header.key) {
                    continue;
                }
                metadata.insert(
                    header.key.to_string(),
                    String::from_utf8_lossy(header.value.unwrap_or_default()).to_string(),
                );
            }
            canonical_message.metadata = metadata;
        }
    }

    if tombstone {
        canonical_message
            .metadata
            .insert(KAFKA_TOMBSTONE_KEY.to_string(), "true".to_string());
    }

    Ok(canonical_message)
}

fn to_canonical_with_source_metadata<M: Message>(
    message: &M,
    topic_in_id: bool,
) -> anyhow::Result<CanonicalMessage> {
    let mut canonical_message = to_canonical_with_topic_id(message, topic_in_id)?;
    canonical_message.metadata.insert(
        "mqb.src.kafka_topic".to_string(),
        message.topic().to_string(),
    );
    canonical_message.metadata.insert(
        "mqb.src.kafka_partition".to_string(),
        message.partition().to_string(),
    );
    canonical_message.metadata.insert(
        "mqb.src.kafka_offset".to_string(),
        message.offset().to_string(),
    );
    Ok(canonical_message)
}

fn create_common_config(config: &KafkaConfig) -> ClientConfig {
    let mut client_config = ClientConfig::new();
    client_config.set("bootstrap.servers", &config.url);

    // SASL credentials force `sasl_ssl` below, so the `tls` block applies with them too.
    let sasl = config.username.is_some() && config.password.is_some();
    if config.tls.required || sasl {
        client_config.set("security.protocol", "ssl");
        if let Some(ca_file) = &config.tls.ca_file {
            client_config.set("ssl.ca.location", ca_file);
        } else {
            // A vendored OpenSSL has no CA directory of its own; search the system's.
            client_config.set("ssl.ca.location", "probe");
        }
        if let Some(cert_file) = &config.tls.cert_file {
            client_config.set("ssl.certificate.location", cert_file);
        }
        if let Some(key_file) = &config.tls.key_file {
            client_config.set("ssl.key.location", key_file);
        }
        if let Some(password) = &config.tls.cert_password {
            client_config.set("ssl.key.password", password);
        }
        client_config.set(
            "enable.ssl.certificate.verification",
            (!config.tls.accept_invalid_certs).to_string(),
        );
        crate::support::tls_check::warn_unverified("kafka", config.tls.accept_invalid_certs);
    }

    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        client_config.set("sasl.mechanism", "PLAIN");
        client_config.set("sasl.username", username);
        client_config.set("sasl.password", password);
        client_config.set("security.protocol", "sasl_ssl");
    }
    client_config
}

/// Whether the group has told this consumer which partitions it owns. Empty until the
/// join/rebalance completes, which is the state a drain must not mistake for "no data".
fn has_assignment(consumer: &StreamConsumer) -> bool {
    consumer.assignment().is_ok_and(|tpl| tpl.count() > 0)
}

/// How long a draining consumer waits to be told what it owns before accepting that it
/// owns nothing. The counterpart to `traits::drain_idle_timeout`, which answers a
/// different question: that one is "is the source idle", this one is "has it started".
///
/// A fresh consumer group needs a join/rebalance before any record can arrive, and that
/// routinely outlasts the idle timeout. A member that crashed holds the group until its
/// session times out, so the wait is at least 30s and outlasts `session_timeout` by
/// [`JOIN_AFTER_SESSION_MARGIN`]. Override via `MQ_BRIDGE_DRAIN_JOIN_TIMEOUT_MS`.
fn drain_join_timeout(session_timeout: Duration) -> Duration {
    static V: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    let configured = *V.get_or_init(|| {
        std::env::var("MQ_BRIDGE_DRAIN_JOIN_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
    });
    configured
        .unwrap_or_else(|| Duration::from_secs(30).max(session_timeout + JOIN_AFTER_SESSION_MARGIN))
}

/// Time for the rebalance that follows a dead member's eviction.
const JOIN_AFTER_SESSION_MARGIN: Duration = Duration::from_secs(15);

/// librdkafka's `session.timeout.ms`, with its default when not set.
fn session_timeout(config: &ClientConfig) -> Duration {
    let ms = config
        .get("session.timeout.ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(45_000);
    Duration::from_millis(ms)
}

/// What an idle wait means in drain mode. An idle timeout on its own says nothing: the
/// three states below look identical from the channel, and only one of them is a drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainReadiness {
    /// No assignment yet — the group join has not finished, so nothing *could* have
    /// arrived. Not a drain.
    NotStarted,
    /// Assigned, but some partition has not reached the offset the drain is aiming for:
    /// a fetch is still in flight. Not a drain.
    Starting,
    /// This member's partitions are done, but partitions it does not hold still have rows
    /// the group has not committed. A peer that left hands them over in a rebalance this
    /// member may not have heard of yet; a live peer is still copying them.
    PeersBehind,
    /// Assigned and there is nothing left to read.
    Drained,
}

/// The offset each assigned partition must reach before the drain is complete.
///
/// Resolved once per partition and then reused, which fixes what the drain is aiming at:
/// everything the topic held when the drain began. Re-reading it would chase a live
/// producer's tip, and would put a blocking broker round-trip on every idle wait.
type PartitionOffsets = std::collections::HashMap<(String, i32), i64>;

/// What a drain knows about the partitions it was given.
#[derive(Default)]
struct DrainState {
    /// Where each partition has to get to. See [`PartitionOffsets`].
    targets: PartitionOffsets,
    /// How far the pipeline has actually taken each partition.
    ///
    /// Unlike `position()`, this advances only after mq-bridge takes a record from its
    /// prefetch channel. What was handed downstream is known here exactly and immediately.
    delivered: PartitionOffsets,
    /// The prefetch stream's terminal, held back because the batch it arrived with still
    /// had records in it. Returned by the next `receive_batch`, so a stopped or panicking
    /// reader ends the route instead of looking like a drain.
    pending_terminal: Option<PrefetchError>,
    /// When the next watermark lookup may block again — see [`drain_target`].
    watermark_retry_at: Option<std::time::Instant>,
    /// The group's `session.timeout.ms`; sizes the join wait, see [`drain_join_timeout`].
    session_timeout: Duration,
    /// Every partition of the subscribed topics, looked up once — see [`peers_behind`].
    partitions: Option<Vec<(String, i32)>>,
    /// Subscriber mode only — see [`SubscriberStarts`].
    subscriber_starts: Option<Arc<SubscriberStarts>>,
}

/// How long a failed watermark lookup suppresses further broker calls. `fetch_watermarks`
/// is synchronous librdkafka and blocks the runtime worker for its full timeout, and
/// readiness is evaluated twice per idle wait for up to `drain_join_timeout`. Without this,
/// an unreachable broker meant a blocked worker thread for essentially that whole window.
const WATERMARK_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Resolves the offset a partition must reach, reading it from the broker at most once.
///
/// A failure is remembered: the next lookups return `None` (reported as "not drained yet",
/// the safe answer) without paying the blocking round-trip again until the backoff expires.
fn drain_target(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    key: &(String, i32),
) -> Option<i64> {
    if let Some(high) = state.targets.get(key) {
        return Some(*high);
    }
    if state
        .watermark_retry_at
        .is_some_and(|at| std::time::Instant::now() < at)
    {
        return None;
    }
    match consumer.fetch_watermarks(&key.0, key.1, Duration::from_secs(1)) {
        Ok((_low, high)) => {
            state.watermark_retry_at = None;
            state.targets.insert(key.clone(), high);
            Some(high)
        }
        Err(e) => {
            debug!(
                topic = %key.0,
                partition = key.1,
                error = %e,
                "Kafka watermark lookup failed; backing off before blocking on it again"
            );
            state.watermark_retry_at = Some(std::time::Instant::now() + WATERMARK_RETRY_BACKOFF);
            None
        }
    }
}

/// Decides what an idle drain-mode wait means.
///
/// The only trustworthy answer is where each partition stands against its target — an
/// idle channel says nothing on its own. Treating "idle after some records arrived" as a
/// drain is what silently truncated a topic: an ordinary gap between fetches is
/// indistinguishable from the end of the data, so the shorter the idle timeout the more
/// of the topic went missing, and the copy still reported success.
fn drain_readiness(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    topics: &[String],
    eof_positions: &std::sync::Mutex<PartitionOffsets>,
) -> DrainReadiness {
    if !has_assignment(consumer) {
        return DrainReadiness::NotStarted;
    }
    let Ok(positions) = consumer.position() else {
        return DrainReadiness::Starting;
    };
    let eof_positions = eof_positions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let elements = positions.elements();
    let mut untouched = TopicPartitionList::new();
    for elem in &elements {
        let key = (elem.topic().to_string(), elem.partition());
        let Some(high) = drain_target(consumer, state, &key) else {
            return DrainReadiness::Starting;
        };
        let delivered = state.delivered.get(&key).copied();
        let eof = eof_positions.get(&key).copied();
        if drain_offset_reached(high, delivered, eof) {
            continue;
        }
        // Such a partition gets no EOF marker, see `record_partition_eof`.
        let shares_partition = elements
            .iter()
            .filter(|other| other.partition() == key.1)
            .count()
            > 1;
        if delivered.is_some() || eof.is_some() || !shares_partition {
            return DrainReadiness::Starting;
        }
        untouched.add_partition(&key.0, key.1);
    }
    if untouched.count() > 0 && !committed_at_target(consumer, state, untouched) {
        return DrainReadiness::Starting;
    }
    if peers_behind(consumer, state, topics, &positions) {
        return DrainReadiness::PeersBehind;
    }
    DrainReadiness::Drained
}

/// Whether a partition this member does not hold has rows the group has not committed.
/// A failed lookup counts as yes; `await_first_within` bounds how long that is waited out.
fn peers_behind(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    topics: &[String],
    owned: &TopicPartitionList,
) -> bool {
    let Some(partitions) = topic_partitions(consumer, state, topics) else {
        return true;
    };
    let mut unowned = TopicPartitionList::new();
    for (topic, partition) in &partitions {
        if owned.find_partition(topic, *partition).is_none() {
            unowned.add_partition(topic, *partition);
        }
    }
    if unowned.count() == 0 {
        return false;
    }
    let Ok(committed) = consumer.committed_offsets(unowned, BROKER_PROBE_TIMEOUT) else {
        return true;
    };
    committed.elements().iter().any(|elem| {
        let key = (elem.topic().to_string(), elem.partition());
        drain_target(consumer, state, &key).is_none_or(|high| group_behind(high, elem.offset()))
    })
}

/// Whether the group resumes every partition in `pending` at or past its target, so a
/// partition nothing was read from is drained.
fn committed_at_target(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    pending: TopicPartitionList,
) -> bool {
    let Ok(committed) = consumer.committed_offsets(pending, BROKER_PROBE_TIMEOUT) else {
        return false;
    };
    committed.elements().iter().all(|elem| {
        let key = (elem.topic().to_string(), elem.partition());
        let Some(high) = drain_target(consumer, state, &key) else {
            return false;
        };
        match elem.offset() {
            Offset::Invalid => uncommitted_drained(high, subscriber_start(state, &key), || {
                low_watermark(consumer, state, &key)
            }),
            committed => !group_behind(high, committed),
        }
    })
}

/// A partition the group never committed is drained only if nothing below `high` can still
/// be read: a subscriber started at or past it, or retention already deleted it.
fn uncommitted_drained(high: i64, start: Option<i64>, low: impl FnOnce() -> Option<i64>) -> bool {
    high == 0 || start.is_some_and(|start| start >= high) || low().is_some_and(|low| low >= high)
}

fn subscriber_start(state: &DrainState, key: &(String, i32)) -> Option<i64> {
    let starts = state.subscriber_starts.as_ref()?;
    let offsets = starts.offsets.lock().unwrap_or_else(|e| e.into_inner());
    offsets.get(key).copied()
}

/// The partition's current low watermark; failures back off like [`drain_target`].
fn low_watermark(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    key: &(String, i32),
) -> Option<i64> {
    if state
        .watermark_retry_at
        .is_some_and(|at| std::time::Instant::now() < at)
    {
        return None;
    }
    match consumer.fetch_watermarks(&key.0, key.1, Duration::from_secs(1)) {
        Ok((low, _high)) => Some(low),
        Err(e) => {
            debug!(topic = %key.0, partition = key.1, error = %e, "Kafka watermark lookup failed; backing off");
            state.watermark_retry_at = Some(std::time::Instant::now() + WATERMARK_RETRY_BACKOFF);
            None
        }
    }
}

/// A committed offset is the next one to read, so the group is done at `high`.
fn group_behind(high: i64, committed: Offset) -> bool {
    match committed {
        Offset::Offset(offset) => offset < high,
        _ => high > 0,
    }
}

/// The topics' partitions, read from the broker once; failures back off like
/// [`drain_target`].
fn topic_partitions(
    consumer: &StreamConsumer,
    state: &mut DrainState,
    topics: &[String],
) -> Option<Vec<(String, i32)>> {
    if let Some(partitions) = &state.partitions {
        return Some(partitions.clone());
    }
    if state
        .watermark_retry_at
        .is_some_and(|at| std::time::Instant::now() < at)
    {
        return None;
    }
    let lookup = consumer
        .fetch_metadata(None, BROKER_PROBE_TIMEOUT)
        .map(|metadata| {
            metadata
                .topics()
                .iter()
                .filter(|t| topics.iter().any(|name| name == t.name()))
                .flat_map(|t| {
                    t.partitions()
                        .iter()
                        .map(|p| (t.name().to_string(), p.id()))
                })
                .collect::<Vec<_>>()
        });
    match lookup {
        Ok(partitions) => {
            state.partitions = Some(partitions.clone());
            Some(partitions)
        }
        Err(e) => {
            debug!(topics = ?topics, error = %e, "Kafka partition lookup failed; backing off");
            state.watermark_retry_at = Some(std::time::Instant::now() + WATERMARK_RETRY_BACKOFF);
            None
        }
    }
}

/// A librdkafka position is deliberately not accepted here: it advances when a record is
/// fetched, before mq-bridge's prefetch task has necessarily put that record on its channel.
fn drain_offset_reached(high: i64, delivered: Option<i64>, eof: Option<i64>) -> bool {
    high == 0
        || delivered.is_some_and(|offset| offset >= high)
        || eof.is_some_and(|o| o >= high)
        || (delivered.is_none() && eof == Some(EOF_BEFORE_ANY_RECORD))
}

/// Waits for the first record of a batch. `Ok(None)` means the source is drained.
///
/// Split from the Kafka specifics so the waiting rule can be tested without a broker:
/// `readiness` is the only thing that knows what a consumer is doing.
#[cfg(test)]
async fn await_first(
    rx: &async_channel::Receiver<Result<Prefetched, PrefetchError>>,
    exit_on_empty: bool,
    topic: &str,
    readiness: impl FnMut() -> DrainReadiness,
    topic_has_partitions: impl FnOnce() -> bool,
) -> Option<Result<Prefetched, PrefetchError>> {
    await_first_within(
        rx,
        exit_on_empty,
        topic,
        drain_join_timeout(Duration::ZERO),
        readiness,
        topic_has_partitions,
    )
    .await
}

/// Whether the broker knows one of `topics` with at least one partition. A failed lookup
/// counts as yes: "empty" has to be known, not assumed. Lists all topics, so nothing is
/// auto-created.
fn topic_has_partitions(consumer: &StreamConsumer, topics: &[String]) -> bool {
    match consumer.fetch_metadata(None, BROKER_PROBE_TIMEOUT) {
        Ok(metadata) => metadata
            .topics()
            .iter()
            .any(|t| topics.iter().any(|name| name == t.name()) && !t.partitions().is_empty()),
        Err(e) => {
            debug!(topics = ?topics, error = %e, "Kafka topic lookup failed");
            true
        }
    }
}

async fn await_first_within(
    rx: &async_channel::Receiver<Result<Prefetched, PrefetchError>>,
    exit_on_empty: bool,
    topic: &str,
    join_timeout: Duration,
    mut readiness: impl FnMut() -> DrainReadiness,
    topic_has_partitions: impl FnOnce() -> bool,
) -> Option<Result<Prefetched, PrefetchError>> {
    let start_deadline = std::time::Instant::now() + join_timeout;
    loop {
        // A drain that is already complete shouldn't spend a whole idle timeout finding
        // that out. Nothing can still be in flight: every partition is at its target.
        // The channel is checked again after `readiness()`: an EOF is published only once the
        // records ahead of it are in the channel, so they may have landed in between.
        if exit_on_empty && rx.is_empty() && readiness() == DrainReadiness::Drained && rx.is_empty()
        {
            return None;
        }
        if let Some(item) = crate::traits::drain_gated(exit_on_empty, rx.recv()).await {
            // A closed channel means the prefetch task is gone (it normally sends a
            // terminal first). Report that, rather than letting it read as a drain.
            return Some(item.unwrap_or(Err(PrefetchError::EndOfStream)));
        }
        let state = readiness();
        if state == DrainReadiness::Drained {
            if rx.is_empty() {
                return None;
            }
            continue;
        }
        if std::time::Instant::now() >= start_deadline {
            // No rebalance handed the lagging partitions over, so a live peer owns them.
            if state == DrainReadiness::PeersBehind {
                debug!(topic = %topic, timeout = ?join_timeout, "Kafka drain done; peers still hold lagging partitions");
                return None;
            }
            // Assigned and short of its target: there is data this drain has not copied.
            if state == DrainReadiness::Starting {
                return Some(Err(PrefetchError::Connection(format!(
                    "Kafka drain of '{topic}' stalled for {join_timeout:?} before reaching \
                     the end of its partitions"
                ))));
            }
            // The topic exists, so the join is late (a crashed member holds the group for
            // `session.timeout.ms`): not an empty source.
            if topic_has_partitions() {
                return Some(Err(PrefetchError::Connection(format!(
                    "Kafka drain of '{topic}' was not assigned a partition within \
                     {join_timeout:?} although the topic has partitions; a crashed member \
                     may still hold the group (raise MQ_BRIDGE_DRAIN_JOIN_TIMEOUT_MS)"
                ))));
            }
            tracing::warn!(
                topic = %topic,
                timeout = ?join_timeout,
                "Draining Kafka consumer was never assigned a partition and the broker has \
                 no such topic; treating the source as empty."
            );
            return None;
        }
    }
}

/// A batch taken off the prefetch channel, before it is turned into a `ReceivedBatch`.
struct BatchParts {
    messages: Vec<CanonicalMessage>,
    offsets: BatchOffsets,
    reply_infos: Vec<(Option<String>, Option<String>)>,
    /// The stream ended or failed. Surfaced only once the records already in hand have
    /// been delivered, so a failure never discards a batch that was read successfully.
    terminal: Option<PrefetchError>,
}

/// Takes `first` plus whatever else is already waiting, up to `max_messages`.
///
/// Never blocks past the first record: a batch is what has arrived, not what might.
fn assemble_batch(
    first: Result<Prefetched, PrefetchError>,
    rx: &async_channel::Receiver<Result<Prefetched, PrefetchError>>,
    max_messages: usize,
) -> BatchParts {
    let mut parts = BatchParts {
        messages: Vec::with_capacity(max_messages),
        offsets: BatchOffsets::new(),
        reply_infos: Vec::with_capacity(max_messages),
        terminal: None,
    };
    let mut next = Some(first);
    while let Some(item) = next.take() {
        match item {
            Ok(item) => {
                parts.reply_infos.push((
                    item.message.metadata.get("reply_to").cloned(),
                    item.message.metadata.get("correlation_id").cloned(),
                ));
                parts.messages.push(item.message);
                record_offset(&mut parts.offsets, &item.topic, item.partition, item.offset);
            }
            Err(e) => {
                parts.terminal = Some(e);
                break;
            }
        }
        if parts.messages.len() < max_messages {
            next = rx.try_recv().ok();
        }
    }
    parts
}

#[allow(clippy::too_many_arguments)]
async fn receive_batch_internal(
    prefetcher: &Prefetcher,
    consumer: &Arc<ClosingStreamConsumer>,
    producer: impl Into<Option<&FutureProducer>>,
    max_messages: usize,
    topic: &str,
    topics: &[String],
    exit_on_empty: bool,
    drain_state: &mut DrainState,
    nacked: NackLatch,
) -> Result<ReceivedBatch, ConsumerError> {
    // A terminal held back by the previous batch ends the stream now.
    if let Some(terminal) = drain_state.pending_terminal.take() {
        return Err(terminal_to_consumer_error(terminal));
    }

    // Block for the first record, then take whatever else the prefetcher already has.
    let first = await_first_within(
        &prefetcher.rx,
        exit_on_empty,
        topic,
        drain_join_timeout(drain_state.session_timeout),
        || drain_readiness(consumer, drain_state, topics, &prefetcher.eof_positions),
        || topic_has_partitions(consumer, topics),
    )
    .await;
    let Some(first) = first else {
        return Ok(ReceivedBatch::empty());
    };
    let parts = assemble_batch(first, &prefetcher.rx, max_messages);
    let BatchParts {
        messages,
        offsets: last_offsets,
        reply_infos,
        terminal,
    } = parts;

    // A failure is only surfaced once the records already read have been delivered.
    match terminal {
        Some(terminal) if messages.is_empty() => return Err(terminal_to_consumer_error(terminal)),
        // Held for the next call rather than dropped, so the stream still ends.
        Some(terminal) => drain_state.pending_terminal = Some(terminal),
        None if messages.is_empty() => return Ok(ReceivedBatch::empty()),
        None => {}
    }

    // Only a drain asks how far each partition has got, so only a drain pays for tracking it.
    if exit_on_empty {
        for (topic, partition, offset) in &last_offsets {
            let entry = drain_state
                .delivered
                .entry((topic.clone(), *partition))
                .or_insert(*offset);
            *entry = (*entry).max(*offset);
        }
    }

    let messages_len = messages.len();
    let last_offset_tpl = offsets_to_tpl(&last_offsets)?;
    trace!(count = messages_len, topic = %topic, message_ids = ?LazyMessageIds(&messages), "Received batch of Kafka messages");

    let consumer = consumer.clone();
    let producer = producer.into().cloned();

    let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
        Box::pin(async move {
            // Handle replies
            // Check for Nacks before moving dispositions for replies
            let any_nack = dispositions
                .iter()
                .any(|d| matches!(d, MessageDisposition::Nack));

            handle_kafka_replies(producer, &reply_infos, dispositions).await;

            if any_nack {
                nacked.trip();
            }
            if !nacked.is_tripped() && messages_len > 0 {
                // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
                if let Err(e) = consumer.commit(&last_offset_tpl, CommitMode::Async) {
                    tracing::error!("Failed to commit Kafka message batch: {:?}", e);
                    return Err(anyhow::anyhow!(
                        "Failed to commit Kafka message batch: {:?}",
                        e
                    ));
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
        for ((reply_topic, correlation_id), disposition) in reply_infos.iter().zip(dispositions) {
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
        let mut tpl = BatchOffsets::new();
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
        let mut tpl = BatchOffsets::new();
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
        let mut tpl = BatchOffsets::new();
        process_message(&msg, &mut messages, &mut tpl).unwrap();

        let expected_id = ((partition as u32 as u128) << 64) | (offset as u64 as u128);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, expected_id);
        // Check that the offset to commit was recorded correctly
        let committed_offset = offsets_to_tpl(&tpl)
            .unwrap()
            .find_partition("test_topic", 4)
            .unwrap()
            .offset();
        assert_eq!(committed_offset, Offset::Offset(124));
    }

    /// Many messages from one partition must collapse to a single entry carrying the last
    /// offset — recording one per message is what made a batch cost O(n²).
    #[test]
    fn test_batch_offsets_keep_one_entry_per_partition() {
        let mut messages = Vec::new();
        let mut offsets = BatchOffsets::new();
        for offset in 0..100 {
            let msg = create_mock_message(Some(b"payload"), None, None, offset, 4);
            process_message(&msg, &mut messages, &mut offsets).unwrap();
        }
        let msg = create_mock_message(Some(b"payload"), None, None, 7, 5);
        process_message(&msg, &mut messages, &mut offsets).unwrap();

        assert_eq!(offsets.len(), 2, "one entry per partition, not per message");
        let tpl = offsets_to_tpl(&offsets).unwrap();
        assert_eq!(
            tpl.find_partition("test_topic", 4).unwrap().offset(),
            Offset::Offset(100)
        );
        assert_eq!(
            tpl.find_partition("test_topic", 5).unwrap().offset(),
            Offset::Offset(8)
        );
    }

    fn header_value(headers: &OwnedHeaders, key: &str) -> Option<String> {
        headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| String::from_utf8_lossy(h.value.unwrap_or_default()).to_string())
    }

    /// The publisher writes the id header that the consumer reads back, so a message that
    /// goes through Kafka must come out with the id it went in with.
    #[test]
    fn test_message_id_survives_the_round_trip() {
        let message = CanonicalMessage::new(b"payload".to_vec(), None);
        let sent_id = message.message_id;

        let msg = create_mock_message(
            Some(b"payload"),
            None,
            Some(message_headers(&message)),
            0,
            0,
        );

        assert_eq!(to_canonical(&msg).unwrap().message_id, sent_id);
    }

    /// `mqb.src.*` records where a message was read from. Forwarding it would let a hop
    /// through Kafka overwrite the next reader's view of the source.
    #[test]
    fn test_publisher_drops_source_metadata_headers() {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message
            .metadata
            .insert("tenant".to_string(), "acme".to_string());
        message
            .metadata
            .insert("mqb.src.kafka_offset".to_string(), "42".to_string());

        let headers = message_headers(&message);

        assert_eq!(header_value(&headers, "tenant"), Some("acme".to_string()));
        assert_eq!(header_value(&headers, "mqb.src.kafka_offset"), None);
    }

    fn prefetched(topic: &str, partition: i32, offset: i64) -> Prefetched {
        let message = CanonicalMessage::new(b"payload".to_vec(), None);
        let budget = new_budget();
        let bytes = message.payload.len();
        budget
            .used
            .fetch_add(bytes, std::sync::atomic::Ordering::AcqRel);
        Prefetched {
            message,
            topic: topic.to_string(),
            partition,
            offset,
            _slot: BudgetSlot { budget, bytes },
        }
    }

    /// The slot count alone does not bound memory (8192 slots x 1 MB records is ~8 GB), so a
    /// paused sink must hit the byte budget and stall the reader well before the slots run out.
    #[tokio::test]
    async fn test_prefetch_budget_blocks_once_the_byte_limit_is_reached() {
        use std::sync::atomic::Ordering;

        let budget = Arc::new(PrefetchBudget {
            limit: 1024,
            used: std::sync::atomic::AtomicUsize::new(0),
            released: tokio::sync::Notify::new(),
        });

        budget.acquire(1024).await;
        assert_eq!(budget.used.load(Ordering::Acquire), 1024);

        // At the limit the next record must wait, however many slots are free.
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move { budget.acquire(16).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while !waiter.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "acquire must block while the budget is exhausted"
        );

        // Taking the record off the channel releases its bytes and lets the reader continue.
        drop(BudgetSlot {
            budget: budget.clone(),
            bytes: 1024,
        });
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("releasing the slot must wake the reader")
            .unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), 16);
    }

    type PrefetchChannel = (
        async_channel::Sender<Result<Prefetched, PrefetchError>>,
        async_channel::Receiver<Result<Prefetched, PrefetchError>>,
    );

    fn prefetch_channel(capacity: usize) -> PrefetchChannel {
        async_channel::bounded(capacity)
    }

    /// A record with no value is a delete on a compacted topic, not a broken message.
    /// Failing it left the offset uncommitted, so the route reconnected onto the same
    /// record forever and one tombstone wedged the consumer.
    #[test]
    fn test_tombstone_becomes_empty_payload_not_error() {
        let msg = create_mock_message(None, Some(b"orders-42"), None, 7, 1);

        let canonical = to_canonical(&msg).expect("a tombstone must not fail the batch");

        assert!(canonical.payload.is_empty());
        assert_eq!(
            canonical
                .metadata
                .get(KAFKA_TOMBSTONE_KEY)
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_ordinary_record_carries_no_tombstone_flag() {
        let msg = create_mock_message(Some(b"{\"a\":1}"), None, None, 7, 1);

        let canonical = to_canonical(&msg).unwrap();

        assert_eq!(canonical.payload.as_ref(), b"{\"a\":1}");
        assert!(!canonical.metadata.contains_key(KAFKA_TOMBSTONE_KEY));
    }

    /// A tombstone keeps its headers: the flag is added to the metadata the headers
    /// produced, not instead of it.
    #[test]
    fn test_tombstone_keeps_headers() {
        let headers = OwnedHeaders::new().insert(Header {
            key: "op",
            value: Some(b"delete"),
        });
        let msg = create_mock_message(None, None, Some(headers), 7, 1);

        let canonical = to_canonical(&msg).unwrap();

        assert_eq!(
            canonical.metadata.get("op").map(String::as_str),
            Some("delete")
        );
        assert!(canonical.metadata.contains_key(KAFKA_TOMBSTONE_KEY));
    }

    /// Round trip: what the consumer flagged, the publisher turns back into a null value.
    /// Publishing the empty payload instead wrote a zero-length record, which does not
    /// delete the key on a compacted topic.
    #[test]
    fn test_republished_tombstone_drops_its_payload_and_flag() {
        let msg = create_mock_message(None, Some(b"orders-42"), None, 7, 1);
        let canonical = to_canonical(&msg).unwrap();

        assert!(is_tombstone(&canonical));
        let headers = message_headers(&canonical);
        assert!(
            (0..headers.count()).all(|i| headers.get(i).key != KAFKA_TOMBSTONE_KEY),
            "the tombstone flag must not be shipped as a Kafka header"
        );
    }

    #[test]
    fn test_ordinary_message_is_not_republished_as_a_tombstone() {
        let msg = create_mock_message(Some(b"{\"a\":1}"), None, None, 7, 1);

        assert!(!is_tombstone(&to_canonical(&msg).unwrap()));
    }

    #[test]
    fn test_assemble_batch_takes_what_is_ready() {
        let (tx, rx) = prefetch_channel(8);
        for offset in 1..4 {
            tx.try_send(Ok(prefetched("t", 0, offset))).unwrap();
        }

        let parts = assemble_batch(Ok(prefetched("t", 0, 0)), &rx, 16);

        assert_eq!(parts.messages.len(), 4);
        assert_eq!(parts.offsets, vec![("t".to_string(), 0, 3)]);
        assert!(parts.terminal.is_none());
    }

    /// The cap is a cap, and what does not fit stays queued for the next call rather than
    /// being dropped.
    #[test]
    fn test_assemble_batch_stops_at_max_messages() {
        let (tx, rx) = prefetch_channel(8);
        for offset in 1..5 {
            tx.try_send(Ok(prefetched("t", 0, offset))).unwrap();
        }

        let parts = assemble_batch(Ok(prefetched("t", 0, 0)), &rx, 3);

        assert_eq!(parts.messages.len(), 3);
        assert_eq!(rx.len(), 2, "the remainder must survive for the next batch");
    }

    /// A batch is what has arrived, not what might: an empty channel ends the batch at the
    /// first record instead of waiting for the rest.
    #[test]
    fn test_assemble_batch_does_not_wait_for_more() {
        let (_tx, rx) = prefetch_channel(8);

        let parts = assemble_batch(Ok(prefetched("t", 0, 0)), &rx, 1024);

        assert_eq!(parts.messages.len(), 1);
    }

    #[test]
    fn test_assemble_batch_collapses_offsets_per_partition() {
        let (tx, rx) = prefetch_channel(8);
        tx.try_send(Ok(prefetched("t", 0, 6))).unwrap();
        tx.try_send(Ok(prefetched("t", 1, 9))).unwrap();

        let parts = assemble_batch(Ok(prefetched("t", 0, 5)), &rx, 16);

        assert_eq!(
            parts.offsets,
            vec![("t".to_string(), 0, 6), ("t".to_string(), 1, 9)]
        );
    }

    #[test]
    fn test_assemble_batch_extracts_reply_info() {
        let (tx, rx) = prefetch_channel(8);
        let mut with_reply = prefetched("t", 0, 1);
        with_reply
            .message
            .metadata
            .insert("reply_to".to_string(), "inbox.1".to_string());
        with_reply
            .message
            .metadata
            .insert("correlation_id".to_string(), "abc".to_string());
        tx.try_send(Ok(with_reply)).unwrap();

        let parts = assemble_batch(Ok(prefetched("t", 0, 0)), &rx, 16);

        assert_eq!(parts.reply_infos[0], (None, None));
        assert_eq!(
            parts.reply_infos[1],
            (Some("inbox.1".to_string()), Some("abc".to_string()))
        );
    }

    /// Records read before the failure are still delivered; the error rides along and is
    /// only surfaced once they are gone.
    #[test]
    fn test_assemble_batch_delivers_records_before_terminal_error() {
        let (tx, rx) = prefetch_channel(8);
        tx.try_send(Ok(prefetched("t", 0, 1))).unwrap();
        tx.try_send(Err(PrefetchError::EndOfStream)).unwrap();

        let parts = assemble_batch(Ok(prefetched("t", 0, 0)), &rx, 16);

        assert_eq!(parts.messages.len(), 2);
        assert!(matches!(parts.terminal, Some(PrefetchError::EndOfStream)));
    }

    /// Regression for a rare drain race: librdkafka's position can already equal `high`
    /// while the final fetched records have not reached the prefetch channel. Only delivery
    /// or an EOF observed later in that same ordered stream is a safe completion barrier.
    #[test]
    fn test_drain_requires_delivery_or_ordered_eof_at_the_target() {
        assert!(!drain_offset_reached(120, Some(112), None));
        assert!(!drain_offset_reached(120, None, Some(119)));
        assert!(drain_offset_reached(120, Some(120), None));
        assert!(drain_offset_reached(120, None, Some(120)));
        assert!(drain_offset_reached(0, None, None));
        // A group that had already consumed the partition gets an EOF and no position.
        assert!(drain_offset_reached(120, None, Some(EOF_BEFORE_ANY_RECORD)));
        assert!(!drain_offset_reached(
            120,
            Some(112),
            Some(EOF_BEFORE_ANY_RECORD)
        ));
    }

    /// `ready_chunks` polls every immediately-ready item before yielding the chunk. Capture
    /// the position as the EOF itself is polled, not after a later record advances it.
    #[tokio::test]
    async fn test_partition_eof_position_precedes_later_records_in_the_same_ready_chunk() {
        let position = std::cell::Cell::new(10);
        let events = futures::stream::iter([Err::<i32, _>(KafkaError::PartitionEOF(0)), Ok(10)]);
        let mut chunks = events
            .map(|event| {
                if event.is_ok() {
                    position.set(11);
                }
                capture_partition_eof_position(event, || {
                    let mut positions = TopicPartitionList::new();
                    positions.add_partition_offset("t", 0, Offset::Offset(position.get()))?;
                    Ok(positions)
                })
            })
            .ready_chunks(1024);

        let chunk = chunks.next().await.unwrap();
        let eof_position = chunk[0].1.as_ref().unwrap();
        let observed = std::sync::Mutex::new(PartitionOffsets::new());
        let (tx, rx) = prefetch_channel(1);
        tx.try_send(Ok(prefetched("t", 0, 11))).unwrap();
        record_partition_eof(eof_position, 0, &observed);

        assert_eq!(
            eof_position.find_partition("t", 0).unwrap().offset(),
            Offset::Offset(10)
        );
        assert!(matches!(chunk[1].0, Ok(10)));
        assert!(!drain_offset_reached(
            11,
            None,
            observed.lock().unwrap().get(&("t".to_string(), 0)).copied()
        ));
        assert_eq!(position.get(), 11, "the later record must have been polled");

        let first = await_first(&rx, true, "t", || DrainReadiness::Drained, || true).await;
        assert!(matches!(first, Some(Ok(record)) if record.offset == 11));
    }

    #[tokio::test]
    async fn test_a_record_landing_while_readiness_reports_drained_is_not_dropped() {
        for calls_before_landing in [0, 1] {
            let (tx, rx) = prefetch_channel(1);
            let mut calls = 0;
            let first = await_first(
                &rx,
                true,
                "t",
                || {
                    if calls == calls_before_landing {
                        tx.try_send(Ok(prefetched("t", 0, 7))).unwrap();
                    }
                    calls += 1;
                    if calls > calls_before_landing {
                        DrainReadiness::Drained
                    } else {
                        DrainReadiness::Starting
                    }
                },
                || true,
            )
            .await;
            assert!(
                matches!(first, Some(Ok(record)) if record.offset == 7),
                "record landing at readiness call {calls_before_landing} was dropped"
            );
        }
    }

    #[test]
    fn test_a_shared_partition_eof_marks_no_unconsumed_topic_drained() {
        let mut positions = TopicPartitionList::new();
        positions.add_partition("a", 0);
        positions
            .add_partition_offset("b", 0, Offset::Offset(5))
            .unwrap();
        positions.add_partition("c", 1);
        let observed = std::sync::Mutex::new(PartitionOffsets::new());
        record_partition_eof(&positions, 0, &observed);
        record_partition_eof(&positions, 1, &observed);
        let observed = observed.into_inner().unwrap();

        assert_eq!(observed.get(&("a".to_string(), 0)), None);
        assert_eq!(observed.get(&("b".to_string(), 0)), Some(&5));
        assert_eq!(
            observed.get(&("c".to_string(), 1)),
            Some(&EOF_BEFORE_ANY_RECORD)
        );
    }

    #[test]
    fn test_drain_join_wait_outlasts_a_crashed_members_session() {
        assert_eq!(
            session_timeout(&ClientConfig::new()),
            Duration::from_secs(45)
        );
        let mut config = ClientConfig::new();
        config.set("session.timeout.ms", "6000");
        assert_eq!(session_timeout(&config), Duration::from_secs(6));

        assert_eq!(
            drain_join_timeout(Duration::from_secs(45)),
            Duration::from_secs(60)
        );
        assert_eq!(
            drain_join_timeout(Duration::from_secs(6)),
            Duration::from_secs(30)
        );
    }

    #[tokio::test]
    async fn test_await_first_ignores_readiness_when_a_record_is_waiting() {
        let (tx, rx) = prefetch_channel(1);
        tx.try_send(Ok(prefetched("t", 0, 5))).unwrap();

        let first = await_first(
            &rx,
            true,
            "t",
            || panic!("readiness must not be consulted while a record is waiting"),
            || true,
        )
        .await;

        assert!(matches!(first, Some(Ok(p)) if p.offset == 5));
    }

    #[tokio::test]
    async fn test_await_first_ends_the_drain_only_when_drained() {
        let (_tx, rx) = prefetch_channel(1);

        let first = await_first(&rx, true, "t", || DrainReadiness::Drained, || true).await;

        assert!(first.is_none());
    }

    /// The regression that made `--drain` land zero rows: an idle wait before the first
    /// fetch arrives is not a drain, so it must keep waiting rather than report empty.
    #[tokio::test]
    async fn test_await_first_keeps_waiting_while_starting() {
        let (tx, rx) = prefetch_channel(1);
        let mut polls = 0;

        let first = await_first(
            &rx,
            true,
            "t",
            || {
                polls += 1;
                if polls == 1 {
                    tx.try_send(Ok(prefetched("t", 0, 5))).unwrap();
                }
                DrainReadiness::Starting
            },
            || true,
        )
        .await;

        assert!(matches!(first, Some(Ok(p)) if p.offset == 5));
        assert_eq!(polls, 1, "the record must be picked up on the next attempt");
    }

    /// Outside drain mode readiness carries no authority: a streaming route must block for
    /// the next record however long it takes, never end because the topic went quiet.
    #[tokio::test]
    async fn test_await_first_never_ends_a_streaming_route() {
        let (_tx, rx) = prefetch_channel(1);

        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            await_first(&rx, false, "t", || DrainReadiness::Drained, || true),
        )
        .await;

        assert!(
            outcome.is_err(),
            "a streaming route must not stop on drained"
        );
    }

    /// A producer that never reaches a broker: enqueue errors are local, deliveries time out.
    fn offline_publisher() -> KafkaPublisher {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", "127.0.0.1:1")
            .set("message.timeout.ms", "300")
            .create()
            .unwrap();
        KafkaPublisher {
            producer: Arc::new(SharedKafkaProducer { producer }),
            topic: "t".to_string(),
            delayed_ack: false,
            partition_key: None,
        }
    }

    /// A record over `message.max.bytes` can never be sent. Reported as retryable it was
    /// nacked, replayed and rejected again, and it took the rest of its batch with it.
    #[tokio::test]
    async fn test_an_oversized_record_fails_alone_and_for_good() {
        let publisher = offline_publisher();
        let small = || CanonicalMessage::new(b"ok".to_vec(), None);
        let oversized = CanonicalMessage::new(vec![0u8; 2_000_000], None);
        let oversized_id = oversized.message_id;

        let sent = publisher
            .send_batch(vec![small(), oversized, small()])
            .await
            .unwrap();

        let SentBatch::Partial { failed, .. } = sent else {
            panic!("expected failures");
        };
        let (_, error) = failed
            .iter()
            .find(|(m, _)| m.message_id == oversized_id)
            .expect("the oversized record must be reported");
        assert!(
            matches!(error, PublisherError::NonRetryable(_)),
            "an oversized record must not be retried: {error:?}"
        );
        assert!(
            failed
                .iter()
                .all(|(_, e)| !e.to_string().contains("aborted")),
            "the record after the oversized one must still be attempted"
        );
    }

    /// With `create_topic: false` no admin request is sent, so a publisher starts without
    /// a reachable broker; with the default the request fails.
    #[tokio::test]
    async fn test_create_topic_false_sends_no_admin_request() {
        let config = |create_topic| KafkaConfig {
            url: "127.0.0.1:1".to_string(),
            topic: Some("t".to_string()),
            create_topic,
            shared: Some(false),
            producer_options: Some(vec![(
                "socket.connection.setup.timeout.ms".to_string(),
                "1000".to_string(),
            )]),
            ..Default::default()
        };
        assert!(KafkaPublisher::new(&config(Some(false))).await.is_ok());
        let created =
            tokio::time::timeout(Duration::from_secs(2), KafkaPublisher::new(&config(None))).await;
        assert!(!matches!(created, Ok(Ok(_))), "the default asks the broker");
    }

    #[test]
    fn test_a_missing_create_permission_does_not_stop_the_publisher() {
        assert!(topic_may_exist(RDKafkaErrorCode::TopicAlreadyExists));
        assert!(topic_may_exist(RDKafkaErrorCode::TopicAuthorizationFailed));
        assert!(topic_may_exist(
            RDKafkaErrorCode::ClusterAuthorizationFailed
        ));
        assert!(!topic_may_exist(RDKafkaErrorCode::InvalidPartitions));
    }

    fn sasl_config() -> KafkaConfig {
        KafkaConfig {
            url: "localhost:9092".to_string(),
            username: Some("user".to_string()),
            password: Some("secret".to_string()),
            ..Default::default()
        }
    }

    /// SASL credentials force `sasl_ssl`, so the `tls` block has to apply with them even
    /// when `required` was left off.
    #[test]
    fn test_the_tls_block_applies_to_a_sasl_connection() {
        let mut config = sasl_config();
        config.tls.ca_file = Some("/ca.pem".to_string());

        let client = create_common_config(&config);

        assert_eq!(client.get("security.protocol"), Some("sasl_ssl"));
        assert_eq!(client.get("ssl.ca.location"), Some("/ca.pem"));
    }

    #[test]
    fn test_tls_without_a_ca_file_searches_the_system_ca_store() {
        let client = create_common_config(&sasl_config());

        assert_eq!(client.get("ssl.ca.location"), Some("probe"));
    }

    #[test]
    fn test_the_key_password_reaches_librdkafka() {
        let mut config = sasl_config();
        config.tls.required = true;
        config.tls.key_file = Some("/client.key".to_string());
        config.tls.cert_password = Some("pw".to_string());

        let client = create_common_config(&config);

        assert_eq!(client.get("ssl.key.password"), Some("pw"));
    }

    /// A partition that is assigned but never reaches its target has data the drain did not
    /// copy. Ending the drain there reported a truncated copy as a success.
    #[tokio::test]
    async fn test_a_drain_that_stalls_before_its_target_is_an_error() {
        let (_tx, rx) = prefetch_channel(1);

        let first = await_first_within(
            &rx,
            true,
            "t",
            Duration::ZERO,
            || DrainReadiness::Starting,
            || true,
        )
        .await;

        assert!(matches!(first, Some(Err(PrefetchError::Connection(_)))));
    }

    #[tokio::test]
    async fn test_a_drain_without_an_assignment_ends_empty_only_without_a_topic() {
        let (_tx, rx) = prefetch_channel(1);
        let wait = |topic_exists: bool| {
            await_first_within(
                &rx,
                true,
                "t",
                Duration::ZERO,
                || DrainReadiness::NotStarted,
                move || topic_exists,
            )
        };

        assert!(wait(false).await.is_none());
        // A late group join on a topic that exists must not read as an empty source.
        assert!(matches!(
            wait(true).await,
            Some(Err(PrefetchError::Connection(_)))
        ));
    }

    /// A member done with its own partitions must not end while a peer's still lag: the
    /// peer may have left, and the rebalance handing them over not have reached it yet.
    #[tokio::test]
    async fn test_a_drain_waits_for_peers_then_ends_cleanly() {
        let (tx, rx) = prefetch_channel(1);
        let mut polls = 0;

        let first = await_first(
            &rx,
            true,
            "t",
            || {
                polls += 1;
                if polls == 2 {
                    tx.try_send(Ok(prefetched("t", 1, 7))).unwrap();
                }
                DrainReadiness::PeersBehind
            },
            || true,
        )
        .await;
        assert!(matches!(first, Some(Ok(p)) if p.offset == 7));

        // No rebalance came within the join wait: a live peer owns them, not an error.
        let (_tx, rx) = prefetch_channel(1);
        let first = await_first_within(
            &rx,
            true,
            "t",
            Duration::ZERO,
            || DrainReadiness::PeersBehind,
            || true,
        )
        .await;
        assert!(first.is_none());
    }

    /// A consumer must keep reading after the broker refuses a commit, and the next commit
    /// must cover the refused one.
    #[tokio::test]
    async fn test_a_refused_commit_is_covered_by_the_next_one() {
        use rdkafka::bindings as rdsys;
        use rdkafka::types::{RDKafkaApiKey, RDKafkaRespErr};

        let producer: FutureProducer = ClientConfig::new()
            .set("test.mock.num.brokers", "1")
            .create()
            .unwrap();
        let cluster = producer.client().mock_cluster().unwrap();
        // The mock holds a new group for 3 s before its first assignment.
        unsafe {
            let handle = rdsys::rd_kafka_handle_mock_cluster(producer.client().native_ptr());
            rdsys::rd_kafka_mock_group_initial_rebalance_delay_ms(handle, 0);
        }
        cluster.create_topic("commits", 1, 1).unwrap();
        let config = KafkaConfig {
            url: cluster.bootstrap_servers(),
            topic: Some("commits".to_string()),
            group_id: Some("commits".to_string()),
            ..Default::default()
        };
        for payload in ["m0", "m1"] {
            producer
                .send(
                    FutureRecord::<(), _>::to("commits").payload(payload),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
        }

        let mut consumer = KafkaConsumer::new(&config).await.unwrap();
        cluster.request_errors(
            RDKafkaApiKey::OffsetCommit,
            &[RDKafkaRespErr::RD_KAFKA_RESP_ERR_GROUP_AUTHORIZATION_FAILED],
        );
        for expected in ["m0", "m1"] {
            let batch = tokio::time::timeout(Duration::from_secs(30), consumer.receive_batch(1))
                .await
                .expect("the consumer stopped reading")
                .unwrap();
            assert_eq!(batch.messages[0].get_payload_str(), expected);
            (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        }

        let handle = consumer.consumer.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let handle = handle.clone();
            let committed = tokio::task::spawn_blocking(move || {
                handle.committed(Duration::from_secs(5)).unwrap()
            })
            .await
            .unwrap();
            let offset = committed.find_partition("commits", 0).unwrap().offset();
            if offset == Offset::Offset(2) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the commit after the refused one never landed: {offset:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// One consumer drains every listed topic, and a drain refuses a pattern it cannot bound.
    #[tokio::test]
    async fn test_a_drain_reads_every_listed_topic() {
        use rdkafka::bindings as rdsys;

        let producer: FutureProducer = ClientConfig::new()
            .set("test.mock.num.brokers", "1")
            .create()
            .unwrap();
        let cluster = producer.client().mock_cluster().unwrap();
        unsafe {
            let handle = rdsys::rd_kafka_handle_mock_cluster(producer.client().native_ptr());
            rdsys::rd_kafka_mock_group_initial_rebalance_delay_ms(handle, 0);
        }
        for (topic, payload) in [("orders.eu", "eu"), ("orders.us", "us")] {
            cluster.create_topic(topic, 2, 1).unwrap();
            producer
                .send(
                    FutureRecord::<(), _>::to(topic).payload(payload),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
        }
        let config = |topic: &str| KafkaConfig {
            url: cluster.bootstrap_servers(),
            topic: Some(topic.to_string()),
            group_id: Some("multi".to_string()),
            ..Default::default()
        };

        let mut consumer = KafkaConsumer::new(&config("orders.eu, orders.us"))
            .await
            .unwrap();
        consumer.set_exit_on_empty(true);
        let mut payloads = Vec::new();
        loop {
            let batch = tokio::time::timeout(Duration::from_secs(60), consumer.receive_batch(10))
                .await
                .expect("the drain never ended")
                .unwrap();
            if batch.messages.is_empty() {
                break;
            }
            payloads.extend(
                batch
                    .messages
                    .iter()
                    .map(|m| m.get_payload_str().to_string()),
            );
            (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
                .await
                .unwrap();
        }
        payloads.sort();
        assert_eq!(payloads, ["eu", "us"]);

        let mut pattern = KafkaConsumer::new(&config("^orders\\..*")).await.unwrap();
        pattern.set_exit_on_empty(true);
        assert!(matches!(
            pattern.receive_batch(10).await,
            Err(ConsumerError::Permanent(_))
        ));
    }

    #[test]
    fn test_a_subscription_splits_on_commas_and_spots_patterns() {
        assert_eq!(topic_list(" a, b,,c "), ["a", "b", "c"]);
        assert!(topic_list("").is_empty());
        assert_eq!(
            topic_list("^orders_[0-9]{1,2}$, audit"),
            ["^orders_[0-9]{1,2}$", "audit"]
        );
        assert!(is_topic_pattern("^orders\\..*"));
        assert!(!is_topic_pattern("orders"));
    }

    #[test]
    fn test_a_multi_topic_fallback_id_tells_topics_apart() {
        let other = |topic: &str| {
            OwnedMessage::new(
                Some(b"x".to_vec()),
                None,
                topic.to_string(),
                rdkafka::Timestamp::now(),
                3,
                7,
                None,
            )
        };
        let id = |m: &OwnedMessage, many| to_canonical_with_topic_id(m, many).unwrap().message_id;
        let single = (3u128 << 64) | 7;
        assert_eq!(id(&other("a"), false), single);
        assert_eq!(id(&other("b"), false), single);
        assert_ne!(id(&other("a"), true), id(&other("b"), true));
        assert_eq!(id(&other("a"), true), id(&other("a"), true));
        assert!(subscribes_many_topics(&topic_list("a,b")));
        assert!(subscribes_many_topics(&topic_list("^a.*")));
        assert!(!subscribes_many_topics(&topic_list("a")));
    }

    #[tokio::test]
    async fn test_a_publisher_rejects_a_topic_list_or_pattern() {
        for topic in ["a,b", "^a.*"] {
            let config = KafkaConfig {
                url: "127.0.0.1:1".to_string(),
                topic: Some(topic.to_string()),
                create_topic: Some(false),
                ..Default::default()
            };
            assert!(KafkaPublisher::new(&config).await.is_err(), "{topic}");
        }
    }

    #[test]
    fn test_group_behind_compares_the_commit_with_the_target() {
        assert!(group_behind(10, Offset::Offset(9)));
        assert!(!group_behind(10, Offset::Offset(10)));
        assert!(group_behind(10, Offset::Invalid));
        assert!(!group_behind(0, Offset::Invalid));
    }

    #[test]
    fn test_uncommitted_partition_drained_only_when_nothing_below_target_is_readable() {
        assert!(uncommitted_drained(0, None, || None));
        assert!(uncommitted_drained(10, Some(10), || None));
        assert!(uncommitted_drained(10, None, || Some(10)));
        assert!(!uncommitted_drained(10, Some(9), || Some(9)));
        assert!(!uncommitted_drained(10, None, || None));
    }

    #[test]
    fn test_record_key_selector() {
        let mut msg = CanonicalMessage::new(b"payload".to_vec(), None);
        msg.metadata
            .insert("pk".to_string(), "public.orders".to_string());

        // Configured field present: its value is the key.
        assert_eq!(record_key(Some("pk"), &msg), b"public.orders".to_vec());
        // Configured field absent on this message: fall back to message_id.
        assert_eq!(
            record_key(Some("missing"), &msg),
            msg.message_id.to_be_bytes().to_vec()
        );
        // No selector configured: fall back to message_id.
        assert_eq!(
            record_key(None, &msg),
            msg.message_id.to_be_bytes().to_vec()
        );
    }
}
