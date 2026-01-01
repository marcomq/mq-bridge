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
use rumqttc::{tokio_rustls::rustls, AsyncClient, MqttOptions, QoS, Transport};
use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, trace, warn};
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
                let payload = if !message.metadata.is_empty() {
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
        let qos = parse_qos(config.qos.unwrap_or(1));

        // The publisher needs a background event loop to handle keep-alives and other control packets.
        let eventloop_handle = tokio::spawn(run_eventloop(eventloop, None));

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

pub struct MqttConsumer(MqttListener);

impl MqttConsumer {
    pub async fn new(config: &MqttConfig, topic: &str, bridge_id: &str) -> anyhow::Result<Self> {
        let client_id = sanitize_for_client_id(&format!("{}-{}", APP_NAME, bridge_id));
        let listener = MqttListener::new(config, topic, &client_id, "consumer").await?;
        Ok(Self(listener))
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

#[derive(Debug)]
enum EventWrapper {
    V3(rumqttc::Event),
    V5(Box<rumqttc::v5::Event>),
}

enum EventLoop {
    V3(Box<rumqttc::EventLoop>),
    V5(Box<EventLoopV5>),
}

struct MqttListener {
    _client: Client,
    eventloop_handle: JoinHandle<()>,
    message_rx: mpsc::Receiver<CanonicalMessage>,
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
        let (tx, rx) = mpsc::channel(queue_capacity);

        let eventloop_handle = tokio::spawn(run_eventloop(eventloop, Some(tx)));

        client.subscribe(topic, qos).await?;
        info!("MQTT subscribed to {}", topic);

        Ok(Self {
            _client: client,
            eventloop_handle,
            message_rx: rx,
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
        let message = self
            .message_rx
            .recv()
            .await
            .ok_or(ConsumerError::EndOfStream)?;
        let commit = Box::new(|_| Box::pin(async {}) as BoxFuture<'static, ()>);
        Ok(Received { message, commit })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = Vec::with_capacity(max_messages);

        // Block for the first message
        match self.message_rx.recv().await {
            Some(msg) => messages.push(msg),
            None => return Err(ConsumerError::EndOfStream),
        }

        // Try to fill the batch with a small timeout to allow for accumulation.
        // This significantly improves throughput by reducing the number of small batches.
        if messages.len() < max_messages {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(5);
            while messages.len() < max_messages {
                match tokio::time::timeout_at(deadline, self.message_rx.recv()).await {
                    Ok(Some(msg)) => messages.push(msg),
                    Ok(None) => break, // Channel closed
                    Err(_) => break,   // Timeout
                }
            }
        }

        let commit = Box::new(|_| Box::pin(async {}) as BoxFuture<'static, ()>);
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

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
            (Client::V5(client), EventLoop::V5(Box::new(eventloop)))
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
            (Client::V3(client), EventLoop::V3(Box::new(eventloop)))
        }
    };

    info!(url = %config.url, "MQTT client created. Eventloop will connect.");
    Ok((client, eventloop))
}

async fn run_eventloop(
    mut eventloop: EventLoop,
    message_tx: Option<mpsc::Sender<CanonicalMessage>>,
) {
    loop {
        let event_result = match &mut eventloop {
            EventLoop::V3(el) => el
                .poll()
                .await
                .map(EventWrapper::V3)
                .map_err(anyhow::Error::new),
            EventLoop::V5(el) => el
                .poll()
                .await
                .map(|e| EventWrapper::V5(Box::new(e)))
                .map_err(anyhow::Error::new),
        };

        match event_result {
            Ok(event) => match event {
                EventWrapper::V3(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    if let Some(tx) = &message_tx {
                        let msg = publish_to_canonical_message_v3(p);
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
                EventWrapper::V5(event) => {
                    if let rumqttc::v5::Event::Incoming(rumqttc::v5::Incoming::Publish(p)) = *event
                    {
                        if let Some(tx) = &message_tx {
                            let msg = publish_to_canonical_message_v5(p);
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                _ => {}
            },
            Err(e) => {
                error!("MQTT EventLoop error: {}. Reconnecting...", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn publish_to_canonical_message_v5(p: PublishV5) -> CanonicalMessage {
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

fn publish_to_canonical_message_v3(p: rumqttc::Publish) -> CanonicalMessage {
    if let Ok(msg) = serde_json::from_slice::<CanonicalMessage>(&p.payload) {
        return msg;
    }
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
        let listener = MqttListener::new(config, topic, &client_id, "subscriber").await?;
        Ok(Self(listener))
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
