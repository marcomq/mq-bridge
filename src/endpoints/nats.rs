use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::NatsConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
use async_nats::{header::HeaderMap, jetstream, jetstream::stream, ConnectOptions};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt, TryStreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring as rustls_ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::io::BufReader;
use std::sync::Arc;
use tracing::{info, trace, warn};
use uuid::Uuid;

enum NatsClient {
    Core(async_nats::Client),
    JetStream(jetstream::Context),
}

pub struct NatsPublisher {
    client: NatsClient,
    core_client: async_nats::Client,
    subject: String,
    // If false, wait for JetStream acknowledgment; if true, fire-and-forget.
    delayed_ack: bool,
    request_reply: bool,
    request_timeout: std::time::Duration,
}

impl NatsPublisher {
    pub async fn new(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
    ) -> anyhow::Result<Self> {
        let options = build_nats_options(config).await?;
        let nats_client = options.connect(&config.url).await?;
        let core_client = nats_client.clone();

        let client = if !config.no_jetstream {
            let jetstream = jetstream::new(nats_client);
            info!(stream = %stream_name, "Ensuring NATS JetStream stream exists");
            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![format!("{}.>", stream_name)],
                    max_messages: config.stream_max_messages.unwrap_or(1_000_000),
                    max_bytes: config.stream_max_bytes.unwrap_or(1024 * 1024 * 1024), // 1GB
                    ..Default::default()
                })
                .await?;
            NatsClient::JetStream(jetstream)
        } else {
            info!("NATS publisher is in Core mode (non-persistent).");
            if config.delayed_ack {
                tracing::debug!("'delayed_ack' is true but NATS is in Core mode, which always performs fire and forget. The flag will be ignored.");
            }
            NatsClient::Core(nats_client)
        };

        Ok(Self {
            client,
            core_client,
            subject: subject.to_string(),
            delayed_ack: config.delayed_ack,
            request_reply: config.request_reply,
            request_timeout: std::time::Duration::from_millis(
                config.request_timeout_ms.unwrap_or(2000),
            ),
        })
    }
}

#[async_trait]
impl MessagePublisher for NatsPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            subject = %self.subject,
            message_id = %format!("{:032x}", message.message_id),
            payload_size = message.payload.len(),
            "Publishing NATS message"
        );
        let headers = if !message.metadata.is_empty() {
            let mut headers = HeaderMap::new();
            for (key, value) in &message.metadata {
                headers.insert(key.as_str(), value.as_str());
            }
            headers
        } else {
            HeaderMap::new()
        };

        if self.request_reply {
            let response = tokio::time::timeout(
                self.request_timeout,
                self.core_client.request_with_headers(
                    self.subject.clone(),
                    headers,
                    message.payload,
                ),
            )
            .await
            .map_err(|_| PublisherError::Retryable(anyhow!("NATS request timed out")))?
            .map_err(|e| PublisherError::Retryable(anyhow!("NATS request failed: {}", e)))?;

            let response_msg = create_nats_canonical_message(&response, None);
            return Ok(Sent::Response(response_msg));
        }

        match &self.client {
            NatsClient::JetStream(jetstream) => {
                tracing::trace!("Publishing to NATS JetStream subject: {}", self.subject);
                let ack_future = jetstream
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await
                    .context("Failed to publish to NATS JetStream")?;
                tracing::trace!("Published to NATS JetStream, waiting for ack");

                if !self.delayed_ack {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), ack_future).await
                    {
                        Ok(Ok(_)) => tracing::trace!("Ack received"),
                        Ok(Err(e)) => {
                            return Err(PublisherError::Retryable(anyhow!(
                                "NATS Ack failed: {}",
                                e
                            )))
                        }
                        Err(_) => {
                            return Err(PublisherError::Retryable(anyhow!("NATS Ack timed out")))
                        }
                    }
                }
            }
            NatsClient::Core(client) => {
                client
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await
                    .context("Failed to publish to NATS Core")?;
            }
        }

        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(
            subject = %self.subject,
            count = messages.len(),
            message_ids = ?LazyMessageIds(&messages),
            "Publishing batch of NATS messages"
        );

        if self.request_reply {
            // For request-reply, we must send individually and gather responses.
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        match &self.client {
            NatsClient::JetStream(_jetstream) => {
                // Use send_batch_helper to send messages sequentially.
                // This avoids overwhelming the NATS client buffer with too many in-flight messages when using JetStream with acks.
                crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
            }
            NatsClient::Core(_) => {
                // Core NATS is fire-and-forget, so the helper is efficient enough.
                crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.core_client
            .flush()
            .await
            .map_err(|e| anyhow!("NATS flush failed: {}", e))
    }
}

enum NatsCore {
    Ephemeral(async_nats::Subscriber),
    JetStream(Box<jetstream::consumer::pull::Stream>),
}

pub struct NatsConsumer {
    core: NatsCore,
    client: async_nats::Client,
    subject: String,
}
use std::any::Any;

impl NatsConsumer {
    pub async fn new(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
    ) -> anyhow::Result<Self> {
        let durable_name = Some(format!(
            "{}-{}-{}",
            APP_NAME,
            stream_name,
            subject.replace('.', "-")
        ));
        let queue_group = Some(format!("{}-{}", APP_NAME, stream_name.replace('.', "-")));

        let (core, client) = NatsCore::connect(
            config,
            stream_name,
            subject,
            durable_name,
            jetstream::consumer::DeliverPolicy::All,
            queue_group,
        )
        .await?;
        Ok(Self {
            core,
            client,
            subject: subject.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for NatsConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.core
            .receive_batch(max_messages, &self.subject, &self.client)
            .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct NatsSubscriber {
    core: NatsCore,
    client: async_nats::Client,
    subject: String,
}

impl NatsSubscriber {
    /// Creates a new NATS event subscriber.
    ///
    /// If the `no_jetstream` option is set to `false`, the subscriber will use NATS JetStream for efficient event consumption.
    /// If the `no_jetstream` option is set to `true`, the subscriber will use NATS Core for event consumption.
    ///
    /// The subscriber will subscribe to the specified subject and watch for new events.
    /// If the `prefetch_count` option is set, the subscriber will prefetch up to that number of events.
    /// If the `prefetch_count` option is not set, the subscriber will prefetch up to 10000 events.
    pub async fn new(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
    ) -> anyhow::Result<Self> {
        let (core, client) = NatsCore::connect(
            config,
            stream_name,
            subject,
            None,
            jetstream::consumer::DeliverPolicy::New,
            None,
        )
        .await?;
        Ok(Self {
            core,
            client,
            subject: subject.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for NatsSubscriber {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.core
            .receive_batch(max_messages, &self.subject, &self.client)
            .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ... rest of the file
async fn build_nats_options(config: &NatsConfig) -> anyhow::Result<ConnectOptions> {
    let mut options = if let Some(token) = &config.token {
        ConnectOptions::with_token(token.clone())
    } else if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        ConnectOptions::with_user_and_password(user.clone(), pass.clone())
    } else {
        ConnectOptions::new()
    };

    if !config.tls.required {
        return Ok(options);
    }

    let mut root_store = rustls::RootCertStore::empty();
    if let Some(ca_file) = &config.tls.ca_file {
        let mut pem = BufReader::new(std::fs::File::open(ca_file)?);
        for cert in rustls_pemfile::certs(&mut pem) {
            root_store.add(cert?)?;
        }
    }

    let tls_config = if config.tls.is_mtls_client_configured() {
        let cert_file = config.tls.cert_file.as_ref().unwrap();
        let key_file = config.tls.key_file.as_ref(); // key_file is optional for some certs
        let mut client_auth_certs = Vec::new();
        let mut pem = BufReader::new(std::fs::File::open(cert_file)?);
        for cert in rustls_pemfile::certs(&mut pem) {
            client_auth_certs.push(cert?);
        }

        let mut client_auth_key = None;
        if let Some(key_file) = key_file {
            let key_bytes = tokio::fs::read(key_file).await?;
            let mut keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut key_bytes.as_slice())
                .collect::<Result<_, _>>()?;
            if !keys.is_empty() {
                client_auth_key = Some(PrivateKeyDer::Pkcs8(keys.remove(0)));
            }
        }

        let provider = rustls_ring::default_provider(); // Corrected line
        let tls_config_builder = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(root_store);

        let tls_config_builder = tls_config_builder.with_client_auth_cert(
            client_auth_certs,
            client_auth_key
                .ok_or_else(|| anyhow!("Client key is required but not found or invalid"))?,
        )?;
        tls_config_builder
    } else {
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    if config.tls.accept_invalid_certs {
        #[derive(Debug)]
        struct NoopServerCertVerifier {
            supported_schemes: Vec<SignatureScheme>,
        }
        impl ServerCertVerifier for NoopServerCertVerifier {
            fn verify_server_cert(
                &self,
                _end_entity: &CertificateDer<'_>,
                _intermediates: &[CertificateDer<'_>],
                _server_name: &rustls::pki_types::ServerName,
                _ocsp_response: &[u8],
                _now: UnixTime,
            ) -> Result<ServerCertVerified, RustlsError> {
                Ok(ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, RustlsError> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn verify_tls13_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, RustlsError> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                self.supported_schemes.clone()
            }
        }
        let schemes = rustls_ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes();
        let verifier = NoopServerCertVerifier {
            supported_schemes: schemes,
        };
        let mut new_tls_config = tls_config;
        new_tls_config
            .dangerous()
            .set_certificate_verifier(Arc::new(verifier));
        options = options.tls_client_config(new_tls_config);
    } else {
        options = options.tls_client_config(tls_config);
    }

    Ok(options)
}

impl NatsCore {
    async fn connect(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
        durable_name: Option<String>,
        deliver_policy: jetstream::consumer::DeliverPolicy,
        queue_group: Option<String>,
    ) -> anyhow::Result<(Self, async_nats::Client)> {
        let options = build_nats_options(config).await?;
        let client = options.connect(&config.url).await?;
        let client_clone = client.clone();

        if !config.no_jetstream {
            let jetstream = jetstream::new(client);
            info!(stream = %stream_name, subject = %subject, "NATS endpoint is in JetStream mode.");

            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![format!("{}.>", stream_name)],
                    max_messages: config.stream_max_messages.unwrap_or(1_000_000),
                    max_bytes: config.stream_max_bytes.unwrap_or(1024 * 1024 * 1024), // 1GB
                    ..Default::default()
                })
                .await?;

            let stream = jetstream.get_stream(stream_name).await?;

            let max_ack_pending = config.prefetch_count.unwrap_or(10000) as i64;
            let consumer = stream
                .create_consumer(jetstream::consumer::pull::Config {
                    durable_name,
                    filter_subject: subject.to_string(),
                    deliver_policy,
                    max_ack_pending,
                    ..Default::default()
                })
                .await?;

            let stream = consumer.messages().await?;
            info!(stream = %stream_name, subject = %subject, "NATS JetStream subscribed");
            Ok((NatsCore::JetStream(Box::new(stream)), client_clone))
        } else {
            info!(subject = %subject, "NATS endpoint is in Core mode.");
            let sub = if let Some(qg) = queue_group {
                info!(queue_group = %qg, "Using queue subscription");
                client.queue_subscribe(subject.to_string(), qg).await?
            } else {
                client.subscribe(subject.to_string()).await?
            };
            info!(subject = %subject, "NATS Core subscribed");
            Ok((NatsCore::Ephemeral(sub), client_clone))
        }
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
        subject: &str,
        client: &async_nats::Client,
    ) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        match self {
            NatsCore::JetStream(stream) => {
                let mut canonical_messages = Vec::with_capacity(max_messages);
                let mut jetstream_messages = Vec::with_capacity(max_messages);

                tracing::trace!("Waiting for next NATS JetStream message");
                let message_stream = stream.next().await;
                tracing::trace!("Received NATS JetStream message");

                // Process the first message if it exists
                match message_stream {
                    Some(Ok(first_message)) => {
                        let sequence = first_message.info().ok().map(|meta| meta.stream_sequence);
                        canonical_messages
                            .push(create_nats_canonical_message(&first_message, sequence));
                        jetstream_messages.push(first_message);
                    }
                    Some(Err(e)) => return Err(ConsumerError::Connection(anyhow::anyhow!(e))),
                    None => {
                        return Err(ConsumerError::Connection(anyhow::anyhow!(
                            "NATS JetStream ended"
                        )))
                    }
                }

                // Greedily fetch the rest of the batch
                while canonical_messages.len() < max_messages {
                    match stream.try_next().now_or_never() {
                        Some(Ok(Some(message))) => {
                            let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                            canonical_messages
                                .push(create_nats_canonical_message(&message, sequence));
                            jetstream_messages.push(message);
                        }
                        _ => break, // No more messages in the buffer or stream ended/errored
                    }
                }

                trace!(count = canonical_messages.len(), subject = %subject, message_ids = ?LazyMessageIds(&canonical_messages), "Received batch of NATS JetStream messages");
                let client = client.clone();
                let commit_closure: BatchCommitFunc = Box::new(move |dispositions| {
                    Box::pin(async move {
                        // Handle replies if responses are provided

                        if dispositions.len() != jetstream_messages.len() {
                            tracing::warn!(
                                    "NATS JetStream batch reply count mismatch: received {} messages but got {} responses. Pairing up to the shorter length.",
                                    jetstream_messages.len(),
                                    dispositions.len()
                                );
                        }
                        for (msg, disposition) in jetstream_messages.iter().zip(dispositions) {
                            if let (Some(reply), MessageDisposition::Reply(resp)) =
                                (msg.reply.as_ref(), disposition)
                            {
                                let publish_result = tokio::time::timeout(
                                    std::time::Duration::from_secs(60),
                                    client.publish(reply.clone(), resp.payload),
                                )
                                .await;

                                match publish_result {
                                    Err(_) => {
                                        tracing::error!(
                                            subject = %reply,
                                            "Failed to publish NATS reply (timeout)"
                                        );
                                    }
                                    Ok(Err(e)) => {
                                        tracing::error!(
                                            subject = %reply,
                                            error = %e,
                                            "Failed to publish NATS reply"
                                        );
                                    }
                                    Ok(Ok(_)) => {}
                                }
                            }
                        }

                        // Acknowledge messages concurrently.
                        // A concurrency limit of 100 is chosen to balance parallelism
                        // with not overwhelming the NATS server or spawning too many tasks.
                        let ack_futures =
                            jetstream_messages.into_iter().map(|message| async move {
                                message.ack().await.map_err(|e| {
                                    anyhow::anyhow!(
                                        "Failed to ACK NATS message subject {}: {}",
                                        message.subject,
                                        e
                                    )
                                })
                            });

                        let results: Vec<Result<(), anyhow::Error>> =
                            futures::stream::iter(ack_futures)
                                .buffer_unordered(100)
                                .collect()
                                .await;

                        for res in results {
                            if let Err(e) = res {
                                tracing::error!(error = %e, "NATS JetStream ack failed");
                                return Err(e);
                            }
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                Ok(ReceivedBatch {
                    messages: canonical_messages,
                    commit: commit_closure,
                })
            }
            NatsCore::Ephemeral(sub) => {
                let mut messages = Vec::with_capacity(max_messages);
                let mut reply_subjects = Vec::with_capacity(max_messages);

                if let Some(message) = sub.next().await {
                    reply_subjects.push(message.reply.clone());
                    messages.push(create_nats_canonical_message(&message, None));

                    while messages.len() < max_messages {
                        match sub.next().now_or_never() {
                            Some(Some(message)) => {
                                reply_subjects.push(message.reply.clone());
                                messages.push(create_nats_canonical_message(&message, None))
                            }
                            _ => break,
                        }
                    }
                } else {
                    return Err(ConsumerError::Connection(anyhow::anyhow!(
                        "NATS Core subscription ended"
                    )));
                }

                let client = client.clone();
                let commit_closure: BatchCommitFunc = Box::new(move |dispositions| {
                    Box::pin(async move {
                        if dispositions.len() != reply_subjects.len() {
                            tracing::warn!(
                                    "NATS Core batch reply count mismatch: received {} messages but got {} responses. Pairing up to the shorter length.",
                                    reply_subjects.len(),
                                    dispositions.len()
                                );
                        }
                        for (reply_opt, disposition) in reply_subjects.iter().zip(dispositions) {
                            if let (Some(reply), MessageDisposition::Reply(resp)) =
                                (reply_opt, disposition)
                            {
                                let publish_result = tokio::time::timeout(
                                    std::time::Duration::from_secs(60),
                                    client.publish(reply.clone(), resp.payload),
                                )
                                .await;

                                match publish_result {
                                    Err(_) => {
                                        tracing::error!(
                                            subject = %reply,
                                            "Failed to publish NATS reply (timeout)"
                                        );
                                    }
                                    Ok(Err(e)) => {
                                        tracing::error!(
                                            subject = %reply,
                                            error = %e,
                                            "Failed to publish NATS reply"
                                        );
                                    }
                                    Ok(Ok(_)) => {}
                                }
                            }
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                trace!(count = messages.len(), subject = %subject, message_ids = ?LazyMessageIds(&messages), "Received batch of NATS Core messages");
                Ok(ReceivedBatch {
                    messages,
                    commit: commit_closure,
                })
            }
        }
    }
}

fn create_nats_canonical_message(
    message: &async_nats::Message,
    sequence: Option<u64>,
) -> CanonicalMessage {
    // The most reliable ID is the JetStream sequence number.
    let mut message_id: Option<u128> = sequence.map(|s| s as u128);

    // If no sequence is available (e.g., Core NATS), fall back to the Nats-Msg-Id header.
    if message_id.is_none() {
        if let Some(headers) = &message.headers {
            if let Some(msg_id_header) = headers.get("Nats-Msg-Id") {
                let id_str = msg_id_header.as_str();
                // Attempt to parse the ID as a UUID or a raw u128.
                if let Ok(uuid) = Uuid::parse_str(id_str) {
                    message_id = Some(uuid.as_u128());
                } else if let Ok(n) = id_str.parse::<u128>() {
                    message_id = Some(n);
                } else {
                    warn!(header_value = %id_str, "Could not parse 'Nats-Msg-Id' header as a UUID or u128");
                }
            }
        }
    }

    let mut canonical_message = CanonicalMessage::new(message.payload.to_vec(), message_id);
    if let Some(headers) = &message.headers {
        if !headers.is_empty() {
            let mut metadata = std::collections::HashMap::new();
            for (key, value) in headers.iter() {
                // Join multiple values with comma to avoid data loss
                let joined_value = value
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined_value.is_empty() {
                    metadata.insert(key.to_string(), joined_value);
                }
            }
            canonical_message.metadata = metadata;
        }
    }
    if let Some(reply) = &message.reply {
        canonical_message
            .metadata
            .insert("reply_to".to_string(), reply.to_string());
    }
    canonical_message
}
