use crate::models::MqttConfig;
use crate::traits::{
    into_batch_commit_func, BatchCommitFunc, BoxFuture, CommitFunc, MessageConsumer,
    MessagePublisher,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use rumqttc::{tokio_rustls::rustls, AsyncClient, Event, Incoming, MqttOptions, QoS, Transport};
use std::sync::Arc;
use std::time::Duration;
use std::{any::Any, future::Future};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info, trace, warn};
pub struct MqttPublisher {
    client: AsyncClient,
    topic: String,
    eventloop_handle: Arc<JoinHandle<()>>,
}

impl MqttPublisher {
    pub async fn new(config: &MqttConfig, topic: &str, bridge_id: &str) -> anyhow::Result<Self> {
        let (client, eventloop) = create_client_and_eventloop(config, bridge_id).await?;
        let (eventloop_handle, ready_rx) = run_eventloop(eventloop, "Publisher", None);

        // Wait for the eventloop to confirm connection.
        tokio::time::timeout(Duration::from_secs(10), ready_rx).await??;

        Ok(Self {
            client,
            topic: topic.to_string(),
            eventloop_handle: Arc::new(eventloop_handle),
        })
    }
    pub fn with_topic(&self, topic: &str) -> Self {
        Self {
            client: self.client.clone(),
            topic: topic.to_string(),
            eventloop_handle: self.eventloop_handle.clone(),
        }
    }

    pub async fn disconnect(&self) -> Result<(), rumqttc::ClientError> {
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
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        tracing::trace!(
            payload = %String::from_utf8_lossy(&message.payload),
            "Publishing MQTT message"
        );
        self.client
            .publish(&self.topic, QoS::AtLeastOnce, false, message.payload)
            .await?;
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MqttConsumer {
    _client: AsyncClient,
    // The receiver for incoming messages from the dedicated eventloop task
    eventloop_handle: Arc<JoinHandle<()>>,
    message_rx: mpsc::Receiver<rumqttc::Publish>,
}

impl MqttConsumer {
    pub async fn new(config: &MqttConfig, topic: &str, bridge_id: &str) -> anyhow::Result<Self> {
        let (client, eventloop) = create_client_and_eventloop(config, bridge_id).await?;
        // Use the configured queue_capacity for the internal MPSC channel.
        let queue_capacity = config.queue_capacity.unwrap_or(100);
        let (message_tx, message_rx) = mpsc::channel(queue_capacity);

        let (eventloop_handle, ready_rx) = run_eventloop(
            eventloop,
            "Consumer",
            Some((client.clone(), topic.to_string(), message_tx)),
        );

        // Wait for the eventloop to confirm connection and subscription.
        tokio::time::timeout(Duration::from_secs(10), ready_rx).await??;

        Ok(Self {
            _client: client,
            message_rx,
            eventloop_handle: Arc::new(eventloop_handle),
        })
    }
}

impl Drop for MqttConsumer {
    fn drop(&mut self) {
        // When the source is dropped, abort its background eventloop task.
        self.eventloop_handle.abort();
    }
}

#[async_trait]
impl MessageConsumer for MqttConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let p = self
            .message_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("MQTT source channel closed"))?;

        let canonical_message = CanonicalMessage::new(p.payload.to_vec());

        let commit = Box::new(move |_response| {
            Box::pin(async move {
                // With rumqttc, acks are handled internally for QoS 1.
                // This closure is called after successful processing.
                trace!(topic = %p.topic, "MQTT message processed");
            }) as BoxFuture<'static, ()>
        });

        Ok((canonical_message, commit))
    }

    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (msg, commit) = self.receive().await?;
        let commit = into_batch_commit_func(commit);
        Ok((vec![msg], commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Runs the MQTT event loop, handling connections, subscriptions, and message forwarding.
fn run_eventloop(
    mut eventloop: rumqttc::EventLoop,
    client_type: &'static str,
    consumer_info: Option<(AsyncClient, String, mpsc::Sender<rumqttc::Publish>)>,
) -> (
    JoinHandle<()>,
    impl Future<Output = Result<(), anyhow::Error>>,
) {
    let (ready_tx, ready_rx) = oneshot::channel();

    let handle = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);

        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(ack))) => {
                    if ack.code == rumqttc::ConnectReturnCode::Success {
                        info!("MQTT {} connected.", client_type);
                        // If this is a consumer, subscribe to the topic.
                        if let Some((client, topic, _)) = &consumer_info {
                            if let Err(e) = client.subscribe(topic, QoS::AtLeastOnce).await {
                                error!("MQTT {} failed to subscribe: {}. Halting.", client_type, e);
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(Err(e.into()));
                                }
                                break;
                            }
                            info!("MQTT {} subscribed to topic '{}'", client_type, topic);
                        }
                        // Signal that the client is ready.
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
                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                    if let Some((_, _, message_tx)) = &consumer_info {
                        if message_tx.send(publish).await.is_err() {
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
                        // If we haven't signaled readiness yet, signal an error.
                        let _ = tx.send(Err(e.into()));
                    }
                    // rumqttc handles reconnection, but a small delay prevents tight error loops.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        info!("MQTT {} eventloop finished.", client_type);
    });

    (handle, async {
        ready_rx
            .await
            .unwrap_or_else(|_| Err(anyhow!("Sender dropped")))
    })
}

async fn create_client_and_eventloop(
    config: &MqttConfig,
    bridge_id: &str,
) -> anyhow::Result<(AsyncClient, rumqttc::EventLoop)> {
    let (host, port) = parse_url(&config.url)?;
    // Use a unique client ID based on the bridge_id to prevent collisions.
    let client_id = sanitize_for_client_id(&format!("mq-multi-bridge-{}", bridge_id));
    let mut mqttoptions = MqttOptions::new(client_id, host, port);

    mqttoptions.set_keep_alive(Duration::from_secs(20));
    mqttoptions.set_clean_session(true);

    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        mqttoptions.set_credentials(username, password);
    }

    if config.tls.required {
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
        mqttoptions.set_transport(Transport::tls_with_config(client_config.into()));
    }

    let queue_capacity = config.queue_capacity.unwrap_or(100);
    let (client, eventloop) = AsyncClient::new(mqttoptions, queue_capacity);
    // The connection is established by the eventloop automatically.
    // We don't need a manual health check. If connection fails, the eventloop will error out.
    info!(url = %config.url, "MQTT client created. Eventloop will connect.");
    Ok((client, eventloop))
}

/// Sanitizes a string to be used as part of an MQTT client ID.
/// Replaces non-alphanumeric characters with a hyphen.
fn sanitize_for_client_id(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
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
