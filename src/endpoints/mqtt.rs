use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{MqttConfig, MqttProtocol};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use rumqttc::v5::mqttbytes::v5::{Publish as PublishV5, PublishProperties};
use rumqttc::v5::mqttbytes::QoS as QoSV5;
use rumqttc::v5::{
    AsyncClient as AsyncClientV5, EventLoop as EventLoopV5, MqttOptions as MqttOptionsV5,
};
use rumqttc::Publish as PublishV3;
use rumqttc::{tokio_rustls::rustls, AsyncClient, MqttOptions, QoS, Transport};
use std::any::Any;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};

#[derive(Clone)]
pub enum Client {
    V3(AsyncClient),
    V5(AsyncClientV5),
}

fn to_qos_v5(qos: QoS) -> QoSV5 {
    match qos {
        QoS::AtMostOnce => QoSV5::AtMostOnce,
        QoS::AtLeastOnce => QoSV5::AtLeastOnce,
        QoS::ExactlyOnce => QoSV5::ExactlyOnce,
    }
}

use crate::traits::EndpointStatus;
impl Client {
    async fn ack(&self, ack: &MqttAck) -> anyhow::Result<()> {
        match (self, ack) {
            (Client::V3(c), MqttAck::V3(p)) => c.ack(p).await.map_err(|e| e.into()),
            (Client::V5(c), MqttAck::V5(p)) => c.ack(p).await.map_err(|e| e.into()),
            _ => Ok(()), // Mismatch or None (QoS 0), ignore
        }
    }

    async fn publish(
        &self,
        topic: &str,
        qos: QoS,
        mut message: CanonicalMessage,
    ) -> anyhow::Result<()> {
        // Whether the message carried per-hop source/provenance keys. In v3 the
        // message_id rides only in the JSON envelope, which is emitted when
        // metadata is non-empty; if stripping the source keys empties metadata we
        // must still keep the envelope, or the message_id would be lost.
        let had_source_metadata = message
            .metadata
            .keys()
            .any(|key| crate::canonical_message::is_source_metadata_key(key));
        // Drop source/provenance keys so they are not forwarded (v5 user
        // properties or the v3 JSON envelope below).
        message.strip_source_metadata();
        match self {
            Client::V5(client) => {
                let mut props = PublishProperties::default();
                if let Some(rt) = message.metadata.get("reply_to") {
                    props.response_topic = Some(rt.clone());
                }
                if let Some(cd) = message.metadata.get("correlation_id") {
                    props.correlation_data = Some(cd.as_bytes().to_vec().into());
                }
                let mut user_properties: Vec<(String, String)> =
                    message.metadata.into_iter().collect();
                user_properties.push((
                    "mq_bridge.message_id".to_string(),
                    format!("{:032x}", message.message_id),
                ));
                props.user_properties = user_properties;
                client
                    .publish_with_properties(topic, to_qos_v5(qos), false, message.payload, props)
                    .await
                    .map_err(|e| e.into())
            }
            Client::V3(client) => {
                let payload = if !message.metadata.is_empty() || had_source_metadata {
                    serde_json::to_vec(&message)?
                } else {
                    message.payload.into()
                };
                client
                    .publish(topic, qos, false, payload)
                    .await
                    .map_err(|e| e.into())
            }
        }
    }

    async fn subscribe(&self, topic: &str, qos: QoS) -> anyhow::Result<()> {
        match self {
            Client::V3(client) => client.subscribe(topic, qos).await.map_err(|e| e.into()),
            Client::V5(client) => client
                .subscribe(topic, to_qos_v5(qos))
                .await
                .map_err(|e| e.into()),
        }
    }
}

struct MqttState {
    client: Client,
    _stop_tx: mpsc::Sender<()>,
    is_connected: Arc<AtomicBool>,
}

/// Tracks broker confirmation (PUBACK for QoS 1, PUBCOMP for QoS 2) of publishes
/// so the publisher can ack the route only once the broker has confirmed delivery,
/// rather than when rumqttc merely enqueues the publish in its eventloop channel.
///
/// `submitted`/`confirmed` are monotonic global counters. A publish is "confirmed"
/// once `confirmed >= the value of submitted captured right after enqueueing it`.
/// This is event-driven (no polling) and counts in aggregate, so the in-flight
/// window stays full and concurrent throughput stays broker-bound.
///
/// `epoch` is the MQTT session generation. rumqttc only resends its in-flight QoS
/// 1/2 publishes after a reconnect if the broker returns `session_present == true`;
/// on a `session_present == false` reconnect it silently drops them (no PUBACK, no
/// error). Were we to rely on the aggregate counter alone, a dropped message's
/// watermark could still be reached by *later* messages' genuine PUBACKs, so its
/// batch would be acked and the route would drop the source — silent loss. The
/// epoch is bumped on every session reset; a wait that spans a bump fails so the
/// affected publishes are retried instead of falsely confirmed.
struct PublishConfirm {
    submitted: AtomicU64,
    confirmed: AtomicU64,
    epoch: AtomicU64,
    notify: Notify,
}

impl PublishConfirm {
    fn new() -> Self {
        Self {
            submitted: AtomicU64::new(0),
            confirmed: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    /// Records one broker confirmation and wakes any waiters.
    fn record_confirmation(&self) {
        self.confirmed.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    /// Marks an MQTT session reset (CONNACK with `session_present == false`), under
    /// which rumqttc discards any in-flight publishes. Wakes waiters so in-progress
    /// confirmations fail fast and the publishes are retried.
    fn reset_session(&self) {
        let submitted = self.submitted.load(Ordering::Acquire);
        self.confirmed.fetch_max(submitted, Ordering::AcqRel);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Reserves a slot for a publish about to be enqueued, returning the watermark
    /// (`confirmed` must reach this value for the publish to count as confirmed).
    fn reserve(&self) -> u64 {
        self.submitted.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Waits until `confirmed >= target` or the deadline elapses. Returns `true`
    /// if the target was reached. Returns `false` if the session was reset since
    /// `start_epoch` (the in-flight publishes may have been dropped, so the caller
    /// must retry) or the deadline elapsed. Event-driven via `Notify`; the deadline
    /// sleep is only a backstop against a missed wakeup.
    async fn wait_for(&self, target: u64, start_epoch: u64, deadline: Instant) -> bool {
        loop {
            if self.current_epoch() != start_epoch {
                return false;
            }
            // Register for notification BEFORE loading `confirmed`, so a confirmation
            // that lands between the check and the await is not missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.current_epoch() != start_epoch {
                return false;
            }
            if self.confirmed.load(Ordering::Acquire) >= target {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(deadline - now) => {
                    return self.current_epoch() == start_epoch
                        && self.confirmed.load(Ordering::Acquire) >= target;
                }
            }
        }
    }
}

/// How long a publish waits for broker confirmation before being reported as
/// retryable. Must comfortably outlast a transient broker restart so rumqttc can
/// reconnect and redeliver in-flight QoS 1/2 publishes.
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Below the route's 5 s start window, so the cause is reported with the start failure.
const PUBLISHER_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

pub struct MqttPublisher {
    state: Arc<RwLock<MqttState>>,
    topic: String,
    qos: QoS,
    /// `Some` for QoS 1/2 (confirm via PUBACK/PUBCOMP); `None` for QoS 0 (fire-and-forget).
    confirm: Option<Arc<PublishConfirm>>,
}

impl MqttPublisher {
    pub async fn new(config: &MqttConfig) -> anyhow::Result<Self> {
        let topic = config
            .topic
            .as_deref()
            .ok_or_else(|| anyhow!("Topic is required for MQTT publisher"))?;
        let client_id = config.client_id.clone().unwrap_or_else(|| {
            sanitize_for_client_id(&format!("{}-{}", APP_NAME, fast_uuid_v7::gen_id()))
        });

        let qos = parse_qos(config.qos.unwrap_or(1));
        // QoS 1/2 publishes are confirmed end-to-end via PUBACK/PUBCOMP; QoS 0 is
        // fire-and-forget with nothing to confirm.
        let confirm = (qos != QoS::AtMostOnce).then(|| Arc::new(PublishConfirm::new()));

        let state = Self::connect(config, &client_id, confirm.clone()).await?;

        Ok(Self {
            state: Arc::new(RwLock::new(state)),
            topic: topic.to_string(),
            qos,
            confirm,
        })
    }

    async fn connect(
        config: &MqttConfig,
        client_id: &str,
        confirm: Option<Arc<PublishConfirm>>,
    ) -> anyhow::Result<MqttState> {
        let (client, eventloop) = create_client_and_eventloop(config, client_id).await?;
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let is_connected = Arc::new(AtomicBool::new(false));

        tokio::spawn(run_eventloop(
            eventloop,
            None::<Sender<MqttInternalMessage>>,
            stop_rx,
            None,
            !config.delayed_ack,
            is_connected.clone(),
            confirm,
            None, // publishers don't subscribe
        ));

        // The event loop connects in the background and retries forever, so without this
        // a publisher for an unreachable broker would look started.
        let deadline = tokio::time::Instant::now() + PUBLISHER_CONNECT_TIMEOUT;
        while !is_connected.load(Ordering::Relaxed) {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "the MQTT broker did not accept the connection within {:?}",
                    PUBLISHER_CONNECT_TIMEOUT
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        Ok(MqttState {
            client,
            _stop_tx: stop_tx,
            is_connected,
        })
    }
}

#[async_trait]
impl MessagePublisher for MqttPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            message_id = %format!("{:032x}", message.message_id),
            topic = %self.topic,
            payload_size = message.payload.len(),
            "Publishing MQTT message"
        );

        let client = self.state.read().await.client.clone();
        let publish_future = client.publish(&self.topic, self.qos, message);

        // We use a longer timeout here (10s) to allow for transient connection drops/reconnects
        // without immediately failing the batch, while still preventing indefinite hangs.
        match tokio::time::timeout(Duration::from_secs(10), publish_future).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return Err(PublisherError::Connection(anyhow!(
                    "Failed to publish MQTT message: {}",
                    e
                )))
            }
            Err(_) => {
                return Err(PublisherError::Connection(anyhow!(
                    "MQTT publish timed out"
                )))
            }
        }

        // For QoS 1/2, wait for the broker to confirm (PUBACK/PUBCOMP) before
        // reporting success. On enqueue-only success the broker can still drop the
        // publish on a session reset, which is silent message loss.
        if let Some(confirm) = &self.confirm {
            let start_epoch = confirm.current_epoch();
            let target = confirm.reserve();
            let deadline = Instant::now() + CONFIRMATION_TIMEOUT;
            if !confirm.wait_for(target, start_epoch, deadline).await {
                return Err(PublisherError::Connection(anyhow!(
                    "MQTT publish not confirmed by broker (timeout or session reset)"
                )));
            }
        }

        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(count = messages.len(), topic = %self.topic, message_ids = ?LazyMessageIds(&messages), "Publishing batch of MQTT messages");
        let client = self.state.read().await.client.clone();

        let mut first_error: Option<anyhow::Error> = None;
        let mut failed_indices = Vec::new();
        // Highest confirmation watermark across the messages we enqueued; once
        // `confirmed` reaches it, every enqueued message in this batch is confirmed.
        let mut confirm_target: u64 = 0;
        // Session generation captured before enqueueing; if it changes before the
        // batch is confirmed, a reset may have dropped our publishes -> retry.
        let start_epoch = self.confirm.as_ref().map_or(0, |c| c.current_epoch());

        for (i, message) in messages.iter().enumerate() {
            if first_error.is_some() {
                failed_indices.push(i);
                continue;
            }
            let publish_future = client.publish(&self.topic, self.qos, message.clone());
            match tokio::time::timeout(Duration::from_secs(10), publish_future).await {
                Ok(Ok(_)) => {
                    // Enqueued; reserve a confirmation slot for QoS 1/2.
                    if let Some(confirm) = &self.confirm {
                        confirm_target = confirm.reserve();
                    }
                }
                Ok(Err(e)) => {
                    first_error = Some(anyhow!("Failed to publish MQTT message in batch: {}", e));
                    failed_indices.push(i);
                }
                Err(_) => {
                    first_error = Some(anyhow!("MQTT publish timed out in batch"));
                    failed_indices.push(i);
                }
            }
        }

        // For QoS 1/2, wait for the broker to confirm the enqueued messages. Any
        // that are unconfirmed before the timeout (e.g. dropped on a broker
        // restart) are returned as retryable so the route never drops them.
        let confirmation_failed = if let Some(confirm) = &self.confirm {
            if confirm_target > 0 {
                let deadline = Instant::now() + CONFIRMATION_TIMEOUT;
                !confirm
                    .wait_for(confirm_target, start_epoch, deadline)
                    .await
            } else {
                false
            }
        } else {
            false
        };

        if let Some(e) = &first_error {
            warn!(
                "MQTT batch send failed, marking {} message(s) for retry. First error: {}",
                failed_indices.len(),
                e
            );
        }
        if confirmation_failed {
            warn!("MQTT batch publish not confirmed by broker before timeout, marking enqueued messages for retry");
        }

        if first_error.is_some() || confirmation_failed {
            let failed_messages = messages
                .into_iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    // Retry both enqueue failures and (on confirmation timeout)
                    // every successfully-enqueued-but-unconfirmed message.
                    let enqueue_failed = failed_indices.contains(&i);
                    (enqueue_failed || confirmation_failed).then_some((
                        m,
                        PublisherError::Retryable(anyhow!("Batch failed due to connection issue")),
                    ))
                })
                .collect();

            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
            })
        } else {
            Ok(SentBatch::Ack)
        }
    }

    async fn status(&self) -> EndpointStatus {
        let state = self.state.read().await;
        let healthy = state.is_connected.load(Ordering::Relaxed);
        EndpointStatus {
            healthy,
            target: self.topic.clone(),
            error: if healthy {
                None
            } else {
                Some("Disconnected".to_string())
            },
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MqttConsumer(MqttListener);

impl MqttConsumer {
    pub async fn new(config: &MqttConfig) -> anyhow::Result<Self> {
        let topic = config
            .topic
            .as_deref()
            .ok_or_else(|| anyhow!("Topic is required for MQTT consumer"))?;
        let client_id = config.client_id.clone().unwrap_or_else(|| {
            sanitize_for_client_id(&format!("{}-{}", APP_NAME, fast_uuid_v7::gen_id()))
        });

        let listener = MqttListener::new(config, topic, &client_id, "consumer").await?;
        info!("Known issue: Messages might be lost in rare cases if the MQTT broker is restarted while the consumer is running.");
        Ok(Self(listener))
    }
}

#[async_trait]
impl MessageConsumer for MqttConsumer {
    // MQTT acks per packet id (PUBACK), so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.0.set_exit_on_empty(exit_on_empty);
    }
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.0.receive().await
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.0.receive_batch(max_messages).await
    }
    async fn status(&self) -> crate::traits::EndpointStatus {
        self.0.status().await
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
enum EventWrapper {
    V3(rumqttc::Event),
    V5(Box<rumqttc::v5::Event>),
}

enum EventLoop {
    V3(Box<rumqttc::EventLoop>),
    V5(Box<EventLoopV5>),
}

struct MqttInternalMessage {
    msg: CanonicalMessage,
    ack: MqttAck,
}

enum MqttAck {
    V3(PublishV3),
    V5(PublishV5),
    None,
}

struct MqttListener {
    client: Client,
    _stop_tx: mpsc::Sender<()>,
    message_rx: Receiver<MqttInternalMessage>,
    capacity: usize,
    is_connected: Arc<AtomicBool>,
    topic: String,
    /// Drain mode: only then does an idle recv time out into an empty batch.
    exit_on_empty: bool,
}

impl MqttListener {
    async fn new(
        config: &MqttConfig,
        topic: &str,
        client_id: &str,
        _client_type: &'static str,
    ) -> anyhow::Result<Self> {
        let (client, eventloop) = create_client_and_eventloop(config, client_id).await?;
        let qos = parse_qos(config.qos.unwrap_or(1));
        let queue_capacity = config.queue_capacity.unwrap_or(100);
        let (tx, rx) = bounded(queue_capacity);
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let is_connected = Arc::new(AtomicBool::new(false));

        let sub_info = Some((client.clone(), topic.to_string(), qos));
        // `None` = no SUBACK yet, `Some(true)` = accepted, `Some(false)` = rejected.
        let (subscribed_tx, mut subscribed_rx) = watch::channel(None);
        tokio::spawn(run_eventloop(
            eventloop,
            Some(tx),
            stop_rx,
            sub_info,
            !config.delayed_ack,
            is_connected.clone(),
            None, // consumers don't publish, so there is nothing to confirm
            Some(subscribed_tx),
        ));

        client.subscribe(topic, qos).await?;

        // `subscribe()` only enqueues the request; wait for the broker's SUBACK so the
        // subscription is actually active before we return. Otherwise a message
        // published immediately after `new()` can be missed (QoS 1 delivers nothing to
        // a not-yet-registered subscription).
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(accepted) = *subscribed_rx.borrow_and_update() {
                    return Ok(accepted);
                }
                if subscribed_rx.changed().await.is_err() {
                    // Event loop dropped the sender before a SUBACK arrived.
                    return Err(());
                }
            }
        })
        .await;
        match outcome {
            Err(_elapsed) => {
                return Err(anyhow!(
                    "Timed out waiting for MQTT SUBACK for topic '{topic}' (client_id '{client_id}')"
                ));
            }
            Ok(Err(())) => {
                return Err(anyhow!(
                    "MQTT subscription channel closed before SUBACK for topic '{topic}' (client_id '{client_id}')"
                ));
            }
            Ok(Ok(false)) => {
                return Err(anyhow!(
                    "Broker rejected MQTT subscription for topic '{topic}' (client_id '{client_id}')"
                ));
            }
            Ok(Ok(true)) => {}
        }
        info!(topic = %topic, client_id = %client_id, "MQTT subscribed");

        Ok(Self {
            client,
            _stop_tx: stop_tx,
            message_rx: rx,
            capacity: queue_capacity,
            is_connected,
            topic: topic.to_string(),
            exit_on_empty: false,
        })
    }
}

#[async_trait]
impl MessageConsumer for MqttListener {
    // MQTT acks per packet id (PUBACK), so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let internal = self
            .message_rx
            .recv()
            .await
            .map_err(|_| ConsumerError::EndOfStream)?;

        let message = internal.msg;
        let client = self.client.clone();
        let reply_topic = message.metadata.get("reply_to").cloned();
        let correlation_data = message.metadata.get("correlation_id").cloned();
        let ack_info = internal.ack;

        let commit = Box::new(move |disposition: MessageDisposition| {
            Box::pin(async move {
                match disposition {
                    MessageDisposition::Nack => return Ok(()),
                    MessageDisposition::Reply(resp) => {
                        handle_mqtt_reply(&client, reply_topic, correlation_data, resp).await?;
                        // Fallthrough to Ack
                    }
                    MessageDisposition::Ack => {
                        // Fallthrough to Ack
                    }
                }

                // Acknowledge the original message
                if let Err(e) = client.ack(&ack_info).await {
                    error!("Failed to ack MQTT message: {}", e);
                    return Err(anyhow!("Failed to ack MQTT message: {}", e));
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        Ok(Received { message, commit })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = Vec::with_capacity(max_messages);
        let mut reply_infos = Vec::with_capacity(max_messages);
        let mut acks = Vec::with_capacity(max_messages);

        // Block for the first message; drain mode times out into an empty batch.
        let Some(first) =
            crate::traits::drain_gated(self.exit_on_empty, self.message_rx.recv()).await
        else {
            return Ok(ReceivedBatch::empty());
        };
        match first {
            Ok(internal) => {
                reply_infos.push((
                    internal.msg.metadata.get("reply_to").cloned(),
                    internal.msg.metadata.get("correlation_id").cloned(),
                ));
                messages.push(internal.msg);
                acks.push(internal.ack);
            }
            Err(_) => return Err(ConsumerError::EndOfStream),
        }

        // Greedily consume more messages if they are already buffered, up to max_messages.
        while messages.len() < max_messages {
            match self.message_rx.try_recv() {
                Ok(internal) => {
                    reply_infos.push((
                        internal.msg.metadata.get("reply_to").cloned(),
                        internal.msg.metadata.get("correlation_id").cloned(),
                    ));
                    messages.push(internal.msg);
                    acks.push(internal.ack);
                }
                Err(_) => break, // Empty or Disconnected
            }
        }

        let client = self.client.clone();
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Length check to avoid silent truncation
                if dispositions.len() != reply_infos.len() || dispositions.len() != acks.len() {
                    return Err(anyhow!(
                        "MQTT batch commit: mismatched lengths: dispositions={}, reply_infos={}, acks={}",
                        dispositions.len(), reply_infos.len(), acks.len()
                    ));
                }
                let mut ack_futures = Vec::with_capacity(dispositions.len());

                for (((reply_topic, correlation_data), ack), disposition) in
                    reply_infos.into_iter().zip(acks).zip(dispositions)
                {
                    let client = client.clone();
                    ack_futures.push(async move {
                        match disposition {
                            MessageDisposition::Reply(resp) => {
                                handle_mqtt_reply(&client, reply_topic, correlation_data, resp)
                                    .await?;
                                client.ack(&ack).await.map_err(|e| {
                                    error!("Failed to ack MQTT message in batch: {}", e);
                                    anyhow!("Failed to ack MQTT message batch: {}", e)
                                })
                            }
                            MessageDisposition::Ack => client.ack(&ack).await.map_err(|e| {
                                error!("Failed to ack MQTT message in batch: {}", e);
                                e
                            }),
                            MessageDisposition::Nack => Ok(()),
                        }
                    });
                }

                futures::future::try_join_all(ack_futures).await?;
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        let healthy = self.is_connected.load(Ordering::Relaxed);
        crate::traits::EndpointStatus {
            healthy,
            target: self.topic.clone(),
            // MQTT is push-based: the broker exposes no per-subscriber backlog, so
            // `pending` (broker lag) is not meaningful and is left unset. The local
            // receive-buffer depth/capacity are reported in `details` instead, so
            // they are not mistaken for a "caught up" signal.
            error: if healthy {
                None
            } else {
                Some("Disconnected".to_string())
            },
            details: serde_json::json!({
                "buffered": self.message_rx.len(),
                "buffer_capacity": self.capacity,
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn handle_mqtt_reply(
    client: &Client,
    reply_topic: Option<String>,
    correlation_data: Option<String>,
    resp: CanonicalMessage,
) -> anyhow::Result<()> {
    if let Some(rt) = reply_topic {
        trace!(topic = %rt, "Committing MQTT message, sending reply");
        let mut msg = resp;
        if let Some(cd) = correlation_data {
            msg.metadata.insert("correlation_id".to_string(), cd);
        }
        // Use a timeout to prevent hanging if the client buffer is full or eventloop is stuck
        match tokio::time::timeout(
            Duration::from_secs(60),
            client.publish(&rt, QoS::AtLeastOnce, msg),
        )
        .await
        {
            Ok(Err(e)) => {
                error!(topic = %rt, error = %e, "Failed to publish MQTT reply");
                return Err(anyhow::anyhow!("Failed to publish MQTT reply: {}", e));
            }
            Ok(Ok(_)) => trace!(topic = %rt, "MQTT reply published to channel"),
            Err(_) => {
                error!(topic = %rt, "Timed out publishing MQTT reply");
                return Err(anyhow::anyhow!("Timed out publishing MQTT reply to {}", rt));
            }
        }
    }
    Ok(())
}

async fn create_client_and_eventloop(
    config: &MqttConfig,
    client_id: &str,
) -> anyhow::Result<(Client, EventLoop)> {
    let (host, port) = parse_url(&config.url)?;
    let queue_capacity = config.queue_capacity.unwrap_or(100);
    let use_tls = uses_tls(config);
    crate::support::tls_check::warn_plaintext_credentials(
        "mqtt",
        &config.url,
        config.username.is_some() && config.password.is_some(),
        use_tls,
    );

    let (client, eventloop) = match config.protocol {
        MqttProtocol::V5 => {
            let mut mqttoptions = MqttOptionsV5::new(client_id, host, port);
            mqttoptions
                .set_keep_alive(Duration::from_secs(config.keep_alive_seconds.unwrap_or(20)));
            mqttoptions.set_manual_acks(!config.delayed_ack);
            let default_window: u16 = match config.max_inflight {
                Some(v) => v,
                None => {
                    let capped = std::cmp::min(queue_capacity, u16::MAX as usize);
                    capped as u16
                }
            };
            mqttoptions.set_outgoing_inflight_upper_limit(default_window);
            mqttoptions.set_receive_maximum(Some(default_window));
            mqttoptions.set_max_packet_size(Some(10 * 1024 * 1024)); // Set max packet size to 10MB
            mqttoptions.set_clean_start(config.clean_session);

            if let Some(expiry) = config.session_expiry_interval {
                mqttoptions.set_session_expiry_interval(Some(expiry));
            } else if !config.clean_session {
                // If persistence is requested but no expiry set, default to 1 hour to ensure session survives disconnects.
                mqttoptions.set_session_expiry_interval(Some(3600));
            }

            if let (Some(username), Some(password)) = (&config.username, &config.password) {
                mqttoptions.set_credentials(username, password);
            }

            if use_tls {
                let tls_config = build_tls_config(config).await?;
                mqttoptions.set_transport(Transport::tls_with_config(tls_config.into()));
            }

            let (client, eventloop) = AsyncClientV5::new(mqttoptions, queue_capacity);
            (Client::V5(client), EventLoop::V5(Box::new(eventloop)))
        }
        MqttProtocol::V3 => {
            let mut mqttoptions = MqttOptions::new(client_id, host, port);
            mqttoptions
                .set_keep_alive(Duration::from_secs(config.keep_alive_seconds.unwrap_or(20)));
            mqttoptions.set_manual_acks(!config.delayed_ack);
            let default_window: u16 = match config.max_inflight {
                Some(v) => v,
                None => {
                    let capped = std::cmp::min(queue_capacity, u16::MAX as usize);
                    capped as u16
                }
            };
            mqttoptions.set_inflight(default_window);
            mqttoptions.set_clean_session(config.clean_session);

            if let (Some(username), Some(password)) = (&config.username, &config.password) {
                mqttoptions.set_credentials(username, password);
            }

            if use_tls {
                let tls_config = build_tls_config(config).await?;
                mqttoptions.set_transport(Transport::tls_with_config(tls_config.into()));
            }

            let (client, eventloop) = AsyncClient::new(mqttoptions, queue_capacity);
            (Client::V3(client), EventLoop::V3(Box::new(eventloop)))
        }
    };

    info!(url = %crate::support::redact::url_password(&config.url), "MQTT client created. Eventloop will connect.");
    Ok((client, eventloop))
}

#[allow(clippy::too_many_arguments)]
async fn run_eventloop(
    mut eventloop: EventLoop,
    message_tx: Option<Sender<MqttInternalMessage>>,
    mut stop_rx: mpsc::Receiver<()>,
    subscription_info: Option<(Client, String, QoS)>,
    manual_acks: bool,
    is_connected: Arc<AtomicBool>,
    confirm: Option<Arc<PublishConfirm>>,
    // Reports the SUBACK outcome so `new()` can wait until the subscription is
    // actually active before returning: `Some(true)` = accepted, `Some(false)` =
    // rejected. Only set for consumers.
    subscribed_tx: Option<watch::Sender<Option<bool>>>,
) {
    let mut stopping = false;
    // A future that is always pending until we decide to start the timeout
    let mut flush_timeout: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(futures::future::pending());

    // Packet-ids of our QoS 1/2 publishes that the broker has not yet confirmed.
    // A publish is counted as confirmed exactly once, when a PUBACK/PUBCOMP matches
    // a still-outstanding pkid. This makes confirmation immune to the duplicate
    // PUBACKs that rumqttc produces when it resends in-flight publishes after a
    // reconnect, which would otherwise let a batch be acked before its own message
    // is truly confirmed. Only used by publishers (`confirm` is `Some`).
    let mut outstanding_pkids: HashSet<u16> = HashSet::new();
    // Nothing is in flight before the first CONNACK, so it resets no session.
    let mut connected_before = false;

    loop {
        tokio::select! {
            _ = stop_rx.recv(), if !stopping => {
                stopping = true;
                // Run for a bit longer to flush outgoing messages (like ACKs or replies)
                flush_timeout = Box::pin(tokio::time::sleep(Duration::from_millis(100)));
                debug!("MQTT client dropped, flushing event loop for 100ms");
            }
            _ = &mut flush_timeout, if stopping => {
                debug!("MQTT event loop flush complete, exiting");
                break;
            }
            event_result = poll_event(&mut eventloop) => {
                match event_result {
                    Ok(event) => {
                        match event {
                            EventWrapper::V3(rumqttc::Event::Incoming(incoming)) => match incoming {
                                rumqttc::Incoming::Publish(p) => {
                                    if let Some(tx) = &message_tx {
                                        let topic = p.topic.clone();
                                        let msg = publish_to_canonical_message_v3(&p);
                                        let ack = if manual_acks && p.qos != QoS::AtMostOnce { MqttAck::V3(p) } else { MqttAck::None };
                                        let internal = MqttInternalMessage {
                                            msg, ack
                                        };
                                        trace!(message_id = %format!("{:032x}", internal.msg.message_id), %topic, "Received MQTT v3 message");
                                        if tx.send(internal).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                rumqttc::Incoming::ConnAck(ack) => {
                                    is_connected.store(true, Ordering::Relaxed);
                                    if !ack.session_present {
                                        // rumqttc drops its in-flight publishes on a fresh
                                        // session; fail any pending confirmations so they retry.
                                        if let Some(confirm) = confirm.as_ref().filter(|_| connected_before) {
                                            confirm.reset_session();
                                        }
                                        if let Some((client, topic, qos)) = &subscription_info {
                                            let client = client.clone();
                                            let topic = topic.clone();
                                            let qos = *qos;
                                            info!("Session not present on V3 connection, resubscribing to {}", topic);
                                            tokio::spawn(async move {
                                                if let Err(e) = client.subscribe(&topic, qos).await {
                                                    error!("Failed to resubscribe: {}", e);
                                                }
                                            });
                                        }
                                    } else {
                                        info!("Session present on V3 connection, resuming...");
                                    }
                                    connected_before = true;
                                }
                                rumqttc::Incoming::SubAck(ack) => {
                                    if let Some(tx) = &subscribed_tx {
                                        let accepted = ack.return_codes.iter().all(|c| {
                                            matches!(c, rumqttc::SubscribeReasonCode::Success(_))
                                        });
                                        let _ = tx.send(Some(accepted));
                                    }
                                }
                                rumqttc::Incoming::Disconnect => {
                                    is_connected.store(false, Ordering::Relaxed);
                                }
                                // Broker confirmed one of our QoS 1 (PubAck) / QoS 2
                                // (PubComp) publishes. Count it only if the pkid is
                                // still outstanding, so resend duplicates don't inflate
                                // the confirmation counter.
                                rumqttc::Incoming::PubAck(pa) => {
                                    if let Some(confirm) = &confirm {
                                        if outstanding_pkids.remove(&pa.pkid) {
                                            confirm.record_confirmation();
                                        }
                                    }
                                }
                                rumqttc::Incoming::PubComp(pc) => {
                                    if let Some(confirm) = &confirm {
                                        if outstanding_pkids.remove(&pc.pkid) {
                                            confirm.record_confirmation();
                                        }
                                    }
                                }
                                _ => {}
                            },
                            // Track each QoS 1/2 publish as outstanding when rumqttc
                            // writes it to the wire (also re-emitted on resend, which is
                            // a harmless no-op insert).
                            EventWrapper::V3(rumqttc::Event::Outgoing(rumqttc::Outgoing::Publish(pkid)))
                                if confirm.is_some() =>
                            {
                                outstanding_pkids.insert(pkid);
                            }
                            EventWrapper::V5(event_box) => {
                                match *event_box {
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::Publish(p)) => {
                                        if let Some(tx) = &message_tx {
                                            let topic_bytes = p.topic.clone();
                                            let msg = publish_to_canonical_message_v5(&p);
                                            let ack = if manual_acks && p.qos != QoSV5::AtMostOnce { MqttAck::V5(p) } else { MqttAck::None };
                                            let internal = MqttInternalMessage {
                                                msg, ack
                                            };
                                            trace!(message_id = %format!("{:032x}", internal.msg.message_id), topic = %String::from_utf8_lossy(&topic_bytes), "Received MQTT v5 message");
                                            if tx.send(internal).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::ConnAck(ack)) => {
                                        is_connected.store(true, Ordering::Relaxed);
                                        if !ack.session_present {
                                            // rumqttc drops its in-flight publishes on a fresh
                                            // session; fail pending confirmations so they retry.
                                            if let Some(confirm) = confirm.as_ref().filter(|_| connected_before) {
                                                confirm.reset_session();
                                            }
                                            if let Some((client, topic, qos)) = &subscription_info {
                                                let client = client.clone();
                                                let topic = topic.clone();
                                                let qos = *qos;
                                                info!("Session not present on V5 connection, resubscribing to {}", topic);
                                                tokio::spawn(async move {
                                                    if let Err(e) = client.subscribe(&topic, qos).await {
                                                        error!("Failed to resubscribe: {}", e);
                                                    }
                                                });
                                            }
                                        } else {
                                            info!("Session present on V5 connection, resuming...");
                                        }
                                        connected_before = true;
                                    }
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::SubAck(ack)) => {
                                        if let Some(tx) = &subscribed_tx {
                                            let accepted = ack.return_codes.iter().all(|c| {
                                                matches!(c, rumqttc::v5::mqttbytes::v5::SubscribeReasonCode::Success(_))
                                            });
                                            let _ = tx.send(Some(accepted));
                                        }
                                    }
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::Disconnect(_)) => {
                                        is_connected.store(false, Ordering::Relaxed);
                                    }
                                    // Broker confirmed one of our QoS 1 (PubAck) / QoS 2
                                    // (PubComp) publishes. Count it only if the pkid is
                                    // still outstanding, so resend duplicates don't
                                    // inflate the confirmation counter.
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::PubAck(pa)) => {
                                        if let Some(confirm) = &confirm {
                                            if outstanding_pkids.remove(&pa.pkid) {
                                                confirm.record_confirmation();
                                            }
                                        }
                                    }
                                    rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::PubComp(pc)) => {
                                        if let Some(confirm) = &confirm {
                                            if outstanding_pkids.remove(&pc.pkid) {
                                                confirm.record_confirmation();
                                            }
                                        }
                                    }
                                    // Track each QoS 1/2 publish as outstanding when
                                    // rumqttc writes it to the wire (re-emitted on
                                    // resend, a harmless no-op insert).
                                    rumqttc::v5::Event::Outgoing(rumqttc::Outgoing::Publish(pkid))
                                        if confirm.is_some() =>
                                    {
                                        outstanding_pkids.insert(pkid);
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        is_connected.store(false, Ordering::Relaxed);
                        error!("MQTT EventLoop error: {}. Reconnecting...", e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
    }
}

async fn poll_event(eventloop: &mut EventLoop) -> anyhow::Result<EventWrapper> {
    match eventloop {
        EventLoop::V3(el) => el.poll().await.map(EventWrapper::V3).map_err(|e| e.into()),
        EventLoop::V5(el) => el
            .poll()
            .await
            .map(|e| EventWrapper::V5(Box::new(e)))
            .map_err(|e| e.into()),
    }
}

fn publish_to_canonical_message_v5(p: &PublishV5) -> CanonicalMessage {
    let mut canonical_message = CanonicalMessage::new(p.payload.to_vec(), None);

    if let Some(props) = &p.properties {
        let mut metadata = std::collections::HashMap::new();
        for (key, value) in &props.user_properties {
            if key == "mq_bridge.message_id" {
                if let Ok(id) = u128::from_str_radix(value, 16) {
                    canonical_message.message_id = id;
                }
            }
            // Never let an inbound property spoof a reserved `mqb.src.*` value; the
            // authoritative topic cursor is injected below.
            if crate::canonical_message::is_source_metadata_key(key) {
                continue;
            }
            metadata.insert(key.clone(), value.clone());
        }
        if let Some(rt) = &props.response_topic {
            metadata.insert("reply_to".to_string(), rt.clone());
        }
        if let Some(cd) = &props.correlation_data {
            metadata.insert(
                "correlation_id".to_string(),
                String::from_utf8_lossy(cd).into_owned(),
            );
        }

        if !metadata.is_empty() {
            canonical_message.metadata = metadata;
        }
    }
    // Per-message topic — the only source cursor MQTT offers.
    // Opt-in via the MQB_SOURCE_METADATA env var; off by default.
    if crate::canonical_message::source_metadata_enabled() {
        canonical_message.metadata.insert(
            "mqb.src.mqtt_topic".to_string(),
            String::from_utf8_lossy(&p.topic).into_owned(),
        );
    }
    canonical_message
}

fn publish_to_canonical_message_v3(p: &rumqttc::Publish) -> CanonicalMessage {
    let mut msg = match serde_json::from_slice::<CanonicalMessage>(&p.payload) {
        Ok(msg) => msg,
        Err(_) => CanonicalMessage::new(p.payload.to_vec(), None),
    };
    // Never let a spoofed `mqb.src.*` key in the inbound envelope survive; the
    // authoritative topic cursor is injected below. (No durable offset/sequence —
    // the per-message topic is the only source cursor MQTT offers, and the only way
    // to recover it under a wildcard subscription.)
    msg.strip_source_metadata();
    // Opt-in via the MQB_SOURCE_METADATA env var; off by default.
    if crate::canonical_message::source_metadata_enabled() {
        msg.metadata
            .insert("mqb.src.mqtt_topic".to_string(), p.topic.clone());
    }
    msg
}

/// Sanitizes a string to be used as part of an MQTT client ID.
/// Replaces non-alphanumeric characters with a hyphen.
fn sanitize_for_client_id(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

async fn build_tls_config(config: &MqttConfig) -> anyhow::Result<rustls::ClientConfig> {
    let mut root_cert_store = rustls::RootCertStore::empty();
    if let Some(ca_file) = &config.tls.ca_file {
        let mut ca_buf = std::io::BufReader::new(std::fs::File::open(ca_file)?);
        let certs = rustls_pemfile::certs(&mut ca_buf).collect::<Result<Vec<_>, _>>()?;
        for cert in certs {
            root_cert_store.add(cert)?;
        }
    } else {
        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let client_config_builder =
        rustls::ClientConfig::builder_with_provider(crate::endpoints::get_crypto_provider()?)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_cert_store);

    let mut client_config = if config.tls.is_mtls_client_configured() {
        let cert_file = config.tls.cert_file.as_ref().unwrap();
        let key_file = config.tls.key_file.as_ref().unwrap();
        let cert_chain = load_certs(cert_file)?;
        let key_der = load_private_key(key_file)?;
        client_config_builder.with_client_auth_cert(cert_chain, key_der)?
    } else {
        client_config_builder.with_no_client_auth()
    };

    if config.tls.accept_invalid_certs {
        warn!("MQTT TLS is configured to accept invalid certificates. This is insecure and should not be used in production.");
        let mut dangerous_config = client_config.dangerous();
        let schemes = crate::endpoints::get_crypto_provider()?
            .signature_verification_algorithms
            .supported_schemes();
        let verifier = NoopServerCertVerifier {
            supported_schemes: schemes,
        };
        dangerous_config.set_certificate_verifier(Arc::new(verifier));
    }
    Ok(client_config)
}

fn load_certs(path: &str) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let mut cert_buf = std::io::BufReader::new(std::fs::File::open(path)?);
    Ok(rustls_pemfile::certs(&mut cert_buf).collect::<Result<Vec<_>, _>>()?)
}

fn load_private_key(path: &str) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let mut key_buf = std::io::BufReader::new(std::fs::File::open(path)?);
    let key = rustls_pemfile::private_key(&mut key_buf)?
        .ok_or_else(|| anyhow!("No private key found in {}", path))?;
    Ok(key)
}

/// A rustls certificate verifier that does not perform any validation.
#[derive(Debug)]
struct NoopServerCertVerifier {
    supported_schemes: Vec<rustls::SignatureScheme>,
}

impl rustls::client::danger::ServerCertVerifier for NoopServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported_schemes.clone()
    }
}

fn parse_url(url: &str) -> anyhow::Result<(String, u16)> {
    let url = url::Url::parse(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("No host in URL"))?
        .to_string();
    // Prefer IPv4 localhost to avoid macOS resolving `localhost` to ::1
    // which can bypass Docker Desktop port forwarding in some setups.
    let host = if host == "localhost" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    let port = url
        .port()
        .unwrap_or(if url.scheme() == "mqtts" || url.scheme() == "ssl" {
            8883
        } else {
            1883
        });
    Ok((host, port))
}

/// TLS is on with `tls.required` and with an `mqtts://` or `ssl://` URL.
fn uses_tls(config: &MqttConfig) -> bool {
    let scheme = config.url.split_once("://").map(|(scheme, _)| scheme);
    config.tls.required || matches!(scheme, Some("mqtts" | "ssl"))
}

fn parse_qos(qos: u8) -> QoS {
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtLeastOnce,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;

    #[test]
    fn an_mqtts_url_turns_tls_on_without_tls_required() {
        assert!(uses_tls(&MqttConfig::new("mqtts://broker:8883")));
        assert!(uses_tls(&MqttConfig::new("ssl://broker:8883")));
        assert!(!uses_tls(&MqttConfig::new("mqtt://broker:1883")));
        assert!(!uses_tls(&MqttConfig::new("broker:1883")));
        let required = MqttConfig::new("mqtt://broker:1883")
            .with_tls(crate::models::TlsConfig::new().with_required(true));
        assert!(uses_tls(&required));
    }

    #[test]
    fn v3_strips_spoofed_source_metadata_and_injects_topic() {
        // A v3 JSON envelope carries a reserved `mqb.src.*` key. It must be dropped,
        // and the authoritative per-message topic cursor injected.
        let _source_metadata = crate::canonical_message::force_source_metadata_for_test(Some(true));
        let mut msg = CanonicalMessage::from_vec("body");
        msg.metadata
            .insert("mqb.src.kafka_offset".to_string(), "999".to_string());
        msg.metadata
            .insert("user_key".to_string(), "kept".to_string());
        let envelope = serde_json::to_vec(&msg).unwrap();
        let publish = PublishV3::new("orders/new", QoS::AtLeastOnce, envelope);

        let canonical = publish_to_canonical_message_v3(&publish);

        assert!(!canonical.metadata.contains_key("mqb.src.kafka_offset"));
        assert_eq!(
            canonical.metadata.get("user_key").map(String::as_str),
            Some("kept")
        );
        assert_eq!(
            canonical
                .metadata
                .get("mqb.src.mqtt_topic")
                .map(String::as_str),
            Some("orders/new")
        );
    }
}
