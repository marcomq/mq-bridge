use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{MqttConfig, MqttProtocol};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use rumqttc::v5::mqttbytes::v5::{Publish as PublishV5, PublishProperties};
use rumqttc::v5::mqttbytes::QoS as QoSV5;
use rumqttc::v5::{
    AsyncClient as AsyncClientV5, EventLoop as EventLoopV5, MqttOptions as MqttOptionsV5,
};
use rumqttc::{tokio_rustls::rustls, AsyncClient, Event, MqttOptions, QoS, Transport};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use std::{any::Any, future::Future};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

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

impl Client {
    async fn disconnect(&self) -> anyhow::Result<()> {
        match self {
            Client::V3(client) => client.disconnect().await.map_err(|e| e.into()),
            Client::V5(client) => client.disconnect().await.map_err(|e| e.into()),
        }
    }

    async fn publish(
        &self,
        topic: &str,
        qos: QoS,
        message: CanonicalMessage,
    ) -> anyhow::Result<()> {
        match self {
            Client::V5(client) => {
                let mut props = PublishProperties::default();
                if !message.metadata.is_empty() {
                    props.user_properties = message.metadata.into_iter().collect();
                }
                client
                    .publish_with_properties(topic, to_qos_v5(qos), false, message.payload, props)
                    .await
                    .map_err(|e| e.into())
            }
            Client::V3(client) => {
                if !message.metadata.is_empty() {
                    warn!(
                        "MQTT protocol is V3, but message has metadata. Metadata will be dropped."
                    );
                }
                client
                    .publish(topic, qos, false, message.payload)
                    .await
                    .map_err(|e| e.into())
            }
        }
    }
}

pub struct MqttPublisher {
    client: Client,
    topic: String,
    eventloop_handle: Arc<JoinHandle<()>>,
    qos: QoS,
}

impl MqttPublisher {
    pub async fn new(config: &MqttConfig, topic: &str, bridge_id: &str) -> anyhow::Result<Self> {
        let client_id = sanitize_for_client_id(&format!("{}-{}", APP_NAME, bridge_id));
        let (client, eventloop) = create_client_and_eventloop(config, &client_id).await?;
        let (eventloop_handle, ready_rx) = run_eventloop(eventloop, "Publisher", None).await;
        let qos = parse_qos(config.qos.unwrap_or(1));

        // Wait for the eventloop to confirm connection.
        tokio::time::timeout(Duration::from_secs(10), ready_rx).await??;

        Ok(Self {
            client,
            topic: topic.to_string(),
            eventloop_handle: Arc::new(eventloop_handle),
            qos,
        })
    }
    pub fn with_topic(&self, topic: &str) -> Self {
        Self {
            client: self.client.clone(),
            topic: topic.to_string(),
            eventloop_handle: self.eventloop_handle.clone(),
            qos: self.qos,
        }
    }

    pub async fn disconnect(&self) -> anyhow::Result<()> {
        self.client.disconnect().await
    }
}

impl Drop for MqttPublisher {
    fn drop(&mut self) {
        // When the publisher is dropped, abort its background eventloop task.
        // Only abort when this is the last reference to the eventloop handle.
        if Arc::strong_count(&self.eventloop_handle) == 1 {
            self.eventloop_handle.abort();
        }
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

        self.client
            .publish(&self.topic, self.qos, message)
            .await
            .context("Failed to publish MQTT message")?;
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(count = messages.len(), topic = %self.topic, message_ids = ?LazyMessageIds(&messages), "Publishing batch of MQTT messages");
        for message in messages {
            self.client
                .publish(&self.topic, self.qos, message)
                .await
                .context("Failed to publish MQTT message")?;
        }
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

enum ReceivedMessage {
    V3(rumqttc::Publish),
    V5(PublishV5),
}

impl ReceivedMessage {
    fn to_canonical(&self) -> CanonicalMessage {
        match self {
            ReceivedMessage::V3(p) => publish_to_canonical_message_v3(p),
            ReceivedMessage::V5(p) => publish_to_canonical_message_v5(p),
        }
    }
    fn topic(&self) -> std::borrow::Cow<'_, str> {
        match self {
            ReceivedMessage::V3(p) => std::borrow::Cow::Borrowed(&p.topic),
            ReceivedMessage::V5(p) => String::from_utf8_lossy(&p.topic),
        }
    }
}

struct MqttListener {
    _client: Client,
    eventloop_handle: Arc<JoinHandle<()>>,
    message_rx: mpsc::Receiver<ReceivedMessage>,
    log_noun: &'static str,
    topic: String,
}

impl MqttListener {
    async fn new(
        config: &MqttConfig,
        topic: &str,
        client_id: &str,
        log_noun: &'static str,
        client_type_log: &'static str,
    ) -> anyhow::Result<Self> {
        let (client, eventloop) = create_client_and_eventloop(config, client_id).await?;
        let queue_capacity = config.queue_capacity.unwrap_or(100);
        let (message_tx, message_rx) = mpsc::channel(queue_capacity);

        let qos = parse_qos(config.qos.unwrap_or(1));
        let (eventloop_handle, ready_rx) = run_eventloop(
            eventloop,
            client_type_log,
            Some((client.clone(), topic.to_string(), qos, message_tx)),
        )
        .await;

        tokio::time::timeout(Duration::from_secs(10), ready_rx).await??;

        Ok(Self {
            _client: client,
            eventloop_handle: Arc::new(eventloop_handle),
            message_rx,
            log_noun,
            topic: topic.to_string(),
        })
    }
}

impl Drop for MqttListener {
    fn drop(&mut self) {
        self.eventloop_handle.abort();
    }
}

#[async_trait]
impl MessageConsumer for MqttListener {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let p = self.message_rx.recv().await.ok_or_else(|| {
            anyhow!(
                "MQTT {} channel closed",
                if self.log_noun == "message" {
                    "source"
                } else {
                    "subscriber"
                }
            )
        })?;

        let topic_for_trace = p.topic().to_string();
        let canonical_message = p.to_canonical();
        let log_noun = self.log_noun;

        let commit = Box::new(move |_response| {
            Box::pin(async move {
                trace!(topic = %topic_for_trace, "MQTT {} processed", log_noun);
            }) as BoxFuture<'static, ()>
        });

        Ok(Received {
            message: canonical_message,
            commit,
        })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let first = self.message_rx.recv().await.ok_or_else(|| {
            anyhow!(
                "MQTT {} channel closed",
                if self.log_noun == "message" {
                    "source"
                } else {
                    "subscriber"
                }
            )
        })?;

        let mut messages = Vec::with_capacity(max_messages);
        messages.push(first.to_canonical());

        while messages.len() < max_messages {
            match self.message_rx.try_recv() {
                Ok(p) => messages.push(p.to_canonical()),
                Err(_) => break,
            }
        }

        let count = messages.len();
        trace!(count = count, topic = %self.topic, message_ids = ?LazyMessageIds(&messages), "Received batch of MQTT messages");
        let log_noun = self.log_noun;
        let message_ids = format!("{:?}", LazyMessageIds(&messages)); // could be optimized
        let commit = Box::new(move |_responses: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                trace!(count = count, message_ids = %message_ids, "MQTT batch of {}s processed", log_noun);
            }) as BoxFuture<'static, ()>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MqttConsumer(MqttListener);

impl MqttConsumer {
    pub async fn new(config: &MqttConfig, topic: &str, bridge_id: &str) -> anyhow::Result<Self> {
        let client_id = sanitize_for_client_id(&format!("{}-{}", APP_NAME, bridge_id));
        Ok(Self(
            MqttListener::new(config, topic, &client_id, "message", "Consumer").await?,
        ))
    }
}

#[async_trait]
impl MessageConsumer for MqttConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.0.receive().await
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.0.receive_batch(max_messages).await
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

enum EventLoop {
    V3(rumqttc::EventLoop),
    V5(EventLoopV5),
}

/// Runs the MQTT event loop, handling connections, subscriptions, and message forwarding.
async fn run_eventloop(
    eventloop: EventLoop,
    client_type: &'static str,
    consumer_info: Option<(Client, String, QoS, mpsc::Sender<ReceivedMessage>)>,
) -> (
    JoinHandle<()>,
    impl Future<Output = Result<(), anyhow::Error>>,
) {
    let (ready_tx, ready_rx) = oneshot::channel();

    let handle = tokio::spawn(async move {
        // Dispatch to the appropriate event loop implementation
        match eventloop {
            EventLoop::V3(el) => {
                let consumer_v3 = if let Some((client, topic, qos, tx)) = consumer_info {
                    if let Client::V3(c) = client {
                        Some((c, topic, qos, tx))
                    } else {
                        error!("Client type mismatch in event loop V3");
                        return;
                    }
                } else {
                    None
                };
                run_eventloop_v3(el, client_type, consumer_v3, Some(ready_tx)).await;
            }
            EventLoop::V5(el) => {
                let consumer_v5 = if let Some((client, topic, qos, tx)) = consumer_info {
                    if let Client::V5(c) = client {
                        Some((c, topic, qos, tx))
                    } else {
                        error!("Client type mismatch in event loop V5");
                        return;
                    }
                } else {
                    None
                };
                run_eventloop_v5(el, client_type, consumer_v5, Some(ready_tx)).await;
            }
        }
        debug!("MQTT {} eventloop finished.", client_type);
    });

    (handle, async {
        ready_rx
            .await
            .unwrap_or_else(|_| Err(anyhow!("Sender dropped")))
    })
}

macro_rules! impl_eventloop_runner {
    ($name:ident, $loop_type:ty, $client_type:ty, $event_path:path, $incoming_path:path, $connack_code:path, $qos_map:expr, $msg_map:expr) => {
        async fn $name(
            mut eventloop: $loop_type,
            client_type: &'static str,
            consumer_info: Option<($client_type, String, QoS, mpsc::Sender<ReceivedMessage>)>,
            mut ready_tx: Option<oneshot::Sender<Result<(), anyhow::Error>>>,
        ) {
            use $event_path as MqttEvent;
            use $incoming_path as Incoming;
            loop {
                match eventloop.poll().await {
                    Ok(MqttEvent::Incoming(Incoming::ConnAck(ack))) => {
                        if ack.code == $connack_code {
                            info!("MQTT {} connected.", client_type);
                            if let Some((client, topic, qos, _)) = &consumer_info {
                                if let Err(e) = client.subscribe(topic, $qos_map(*qos)).await {
                                    error!(
                                        "MQTT {} failed to subscribe: {}. Halting.",
                                        client_type, e
                                    );
                                    if let Some(tx) = ready_tx.take() {
                                        let _ = tx.send(Err(e.into()));
                                    }
                                    break;
                                }
                                info!("MQTT {} subscribed to topic '{}'", client_type, topic);
                            }
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                        } else {
                            error!(
                                "MQTT {} connection refused: {:?}. Halting.",
                                client_type, ack.code
                            );
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Err(anyhow!("Connection refused: {:?}", ack.code)));
                            }
                            break;
                        }
                    }
                    Ok(MqttEvent::Incoming(Incoming::Publish(publish))) => {
                        if let Some((_, _, _, message_tx)) = &consumer_info {
                            if message_tx.send($msg_map(publish)).await.is_err() {
                                info!("MQTT {} message channel closed. Halting.", client_type);
                                break;
                            }
                        }
                    }
                    Ok(event) => {
                        trace!("MQTT {} received event: {:?}", client_type, event);
                    }
                    Err(e) => {
                        error!(
                            "MQTT {} eventloop error: {}. Reconnecting...",
                            client_type, e
                        );
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(e.into()));
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
    };
}

impl_eventloop_runner!(
    run_eventloop_v3,
    rumqttc::EventLoop,
    AsyncClient,
    Event,
    rumqttc::Incoming,
    rumqttc::ConnectReturnCode::Success,
    |q| q,
    ReceivedMessage::V3
);

impl_eventloop_runner!(
    run_eventloop_v5,
    EventLoopV5,
    AsyncClientV5,
    rumqttc::v5::Event,
    rumqttc::v5::Incoming,
    rumqttc::v5::mqttbytes::v5::ConnectReturnCode::Success,
    to_qos_v5,
    ReceivedMessage::V5
);

async fn create_client_and_eventloop(
    config: &MqttConfig,
    client_id: &str,
) -> anyhow::Result<(Client, EventLoop)> {
    let (host, port) = parse_url(&config.url)?;
    let queue_capacity = config.queue_capacity.unwrap_or(100);

    let (client, eventloop) = match config.protocol {
        MqttProtocol::V5 => {
            let mut mqttoptions = MqttOptionsV5::new(client_id, host, port);
            mqttoptions
                .set_keep_alive(Duration::from_secs(config.keep_alive_seconds.unwrap_or(20)));
            mqttoptions.set_clean_start(config.clean_session);

            if let (Some(username), Some(password)) = (&config.username, &config.password) {
                mqttoptions.set_credentials(username, password);
            }

            if config.tls.required {
                let tls_config = build_tls_config(config).await?;
                mqttoptions.set_transport(Transport::tls_with_config(tls_config.into()));
            }

            let (client, eventloop) = AsyncClientV5::new(mqttoptions, queue_capacity);
            (Client::V5(client), EventLoop::V5(eventloop))
        }
        MqttProtocol::V3 => {
            let mut mqttoptions = MqttOptions::new(client_id, host, port);
            mqttoptions
                .set_keep_alive(Duration::from_secs(config.keep_alive_seconds.unwrap_or(20)));
            mqttoptions.set_clean_session(config.clean_session);

            if let (Some(username), Some(password)) = (&config.username, &config.password) {
                mqttoptions.set_credentials(username, password);
            }

            if config.tls.required {
                let tls_config = build_tls_config(config).await?;
                mqttoptions.set_transport(Transport::tls_with_config(tls_config.into()));
            }

            let (client, eventloop) = AsyncClient::new(mqttoptions, queue_capacity);
            (Client::V3(client), EventLoop::V3(eventloop))
        }
    };

    info!(url = %config.url, "MQTT client created. Eventloop will connect.");
    Ok((client, eventloop))
}

fn publish_to_canonical_message_v5(p: &PublishV5) -> CanonicalMessage {
    let mut canonical_message = CanonicalMessage::new(p.payload.to_vec(), None);

    if let Some(props) = &p.properties {
        if !props.user_properties.is_empty() {
            let mut metadata = std::collections::HashMap::new();
            for (key, value) in &props.user_properties {
                metadata.insert(key.clone(), value.clone());
            }
            canonical_message.metadata = metadata;
        }
    }
    canonical_message
}

fn publish_to_canonical_message_v3(p: &rumqttc::Publish) -> CanonicalMessage {
    // The MQTT `pkid` is a u16 packet identifier for QoS > 0 messages.
    // It is reused by the client and broker for different messages over time,
    // so it is NOT a unique message identifier. Using it for deduplication
    // would lead to incorrect behavior. We pass `None` here.
    CanonicalMessage::new(p.payload.to_vec(), None)
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
    }

    let client_config_builder =
        rustls::ClientConfig::builder().with_root_certificates(root_cert_store);

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
        dangerous_config.set_certificate_verifier(Arc::new(NoopServerCertVerifier {}));
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
struct NoopServerCertVerifier;

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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
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

fn parse_qos(qos: u8) -> QoS {
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtLeastOnce,
    }
}

pub struct MqttSubscriber(MqttListener);

impl MqttSubscriber {
    pub async fn new(
        config: &MqttConfig,
        topic: &str,
        subscribe_id: Option<String>,
    ) -> anyhow::Result<Self> {
        let unique_id = subscribe_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let client_id = sanitize_for_client_id(&format!("{}-{}", APP_NAME, unique_id));
        Ok(Self(
            MqttListener::new(config, topic, &client_id, "event", "Subscriber").await?,
        ))
    }
}

#[async_trait]
impl MessageConsumer for MqttSubscriber {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.0.receive().await
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.0.receive_batch(max_messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
