use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{MqttConfig, MqttProtocol};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
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
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
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
        message: CanonicalMessage,
    ) -> anyhow::Result<()> {
        match self {
            Client::V5(client) => {
                let mut props = PublishProperties::default();
                if !message.metadata.is_empty() {
                    if let Some(rt) = message.metadata.get("reply_to") {
                        props.response_topic = Some(rt.clone());
                    }
                    if let Some(cd) = message.metadata.get("correlation_id") {
                        props.correlation_data = Some(cd.as_bytes().to_vec().into());
                    }
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
    _stop_tx: mpsc::Sender<()>,
    qos: QoS,
}

impl MqttPublisher {
    pub async fn new(config: &MqttConfig) -> anyhow::Result<Self> {
        let topic = config
            .topic
            .as_deref()
            .ok_or_else(|| anyhow!("Topic is required for MQTT publisher"))?;
        let client_id = config.client_id.clone().unwrap_or_else(|| {
            sanitize_for_client_id(&format!("{}-{}", APP_NAME, fast_uuid_v7::gen_id_string()))
        });

        let (client, eventloop) = create_client_and_eventloop(config, &client_id).await?;
        let qos = parse_qos(config.qos.unwrap_or(1));
        let (stop_tx, stop_rx) = mpsc::channel(1);

        // The publisher needs a background event loop to handle keep-alives and other control packets.
        tokio::spawn(run_eventloop(
            eventloop,
            None::<Sender<MqttInternalMessage>>,
            stop_rx,
            None,
            !config.delayed_ack,
        ));

        Ok(Self {
            client,
            topic: topic.to_string(),
            _stop_tx: stop_tx,
            qos,
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
    pub async fn new(config: &MqttConfig) -> anyhow::Result<Self> {
        let topic = config
            .topic
            .as_deref()
            .ok_or_else(|| anyhow!("Topic is required for MQTT consumer"))?;
        let client_id = config.client_id.clone().unwrap_or_else(|| {
            sanitize_for_client_id(&format!("{}-{}", APP_NAME, fast_uuid_v7::gen_id_string()))
        });

        let listener = MqttListener::new(config, topic, &client_id, "consumer").await?;
        warn!("Known issue: Messages might be lost in rare cases if the MQTT broker is restarted while the consumer is running.");
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

        let sub_info = Some((client.clone(), topic.to_string(), qos));
        tokio::spawn(run_eventloop(
            eventloop,
            Some(tx),
            stop_rx,
            sub_info,
            !config.delayed_ack,
        ));

        client.subscribe(topic, qos).await?;
        info!("MQTT subscribed to {}", topic);

        Ok(Self {
            client,
            _stop_tx: stop_tx,
            message_rx: rx,
        })
    }
}

#[async_trait]
impl MessageConsumer for MqttListener {
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

        // Block for the first message
        match self.message_rx.recv().await {
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
                for (((reply_topic, correlation_data), ack), disposition) in reply_infos
                    .into_iter()
                    .zip(acks.into_iter())
                    .zip(dispositions.into_iter())
                {
                    match disposition {
                        MessageDisposition::Reply(resp) => {
                            handle_mqtt_reply(&client, reply_topic, correlation_data, resp).await?;
                            if let Err(e) = client.ack(&ack).await {
                                error!("Failed to ack MQTT message in batch: {}", e);
                                return Err(e);
                            }
                        }
                        MessageDisposition::Ack => {
                            if let Err(e) = client.ack(&ack).await {
                                error!("Failed to ack MQTT message in batch: {}", e);
                                return Err(e);
                            }
                        }
                        MessageDisposition::Nack => {}
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        Ok(ReceivedBatch { messages, commit })
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

    let (client, eventloop) = match config.protocol {
        MqttProtocol::V5 => {
            let mut mqttoptions = MqttOptionsV5::new(client_id, host, port);
            mqttoptions
                .set_keep_alive(Duration::from_secs(config.keep_alive_seconds.unwrap_or(20)));
            mqttoptions.set_manual_acks(!config.delayed_ack);
            if let Some(inflight) = config.max_inflight {
                mqttoptions.set_outgoing_inflight_upper_limit(inflight);
            }
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
            mqttoptions.set_manual_acks(!config.delayed_ack);
            if let Some(inflight) = config.max_inflight {
                mqttoptions.set_inflight(inflight);
            }
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
    message_tx: Option<Sender<MqttInternalMessage>>,
    mut stop_rx: mpsc::Receiver<()>,
    subscription_info: Option<(Client, String, QoS)>,
    manual_acks: bool,
) {
    let mut stopping = false;
    // A future that is always pending until we decide to start the timeout
    let mut flush_timeout: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(futures::future::pending());

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
            Ok(event) => match event {
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
                        if !ack.session_present {
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
                    }
                    _ => {}
                },
                EventWrapper::V5(event) => {
                    match *event {
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
                            if !ack.session_present {
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
                        }
                        _ => {}
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
    canonical_message
}

fn publish_to_canonical_message_v3(p: &rumqttc::Publish) -> CanonicalMessage {
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
