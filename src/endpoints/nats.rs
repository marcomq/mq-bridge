use crate::models::NatsConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, CommitFunc, ConsumerError, MessageConsumer, MessagePublisher,
    PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::{anyhow, Context};
use async_nats::{header::HeaderMap, jetstream, jetstream::stream, ConnectOptions};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring as rustls_ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::io::BufReader;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

enum NatsClient {
    Core(async_nats::Client),
    JetStream(jetstream::Context),
}

pub struct NatsPublisher {
    client: NatsClient,
    subject: String,
    // If false, wait for JetStream acknowledgment; if true, fire-and-forget.
    delayed_ack: bool,
}

impl NatsPublisher {
    pub async fn new(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
    ) -> anyhow::Result<Self> {
        let options = build_nats_options(config).await?;
        let nats_client = options.connect(&config.url).await?;

        let client = if !config.no_jetstream {
            let jetstream = jetstream::new(nats_client);
            info!(stream = %stream_name, "Ensuring NATS JetStream stream exists");
            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![format!("{}.>", stream_name)],
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
            subject: subject.to_string(),
            delayed_ack: config.delayed_ack,
        })
    }
}

#[async_trait]
impl MessagePublisher for NatsPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let headers = if !message.metadata.is_empty() {
            let mut headers = HeaderMap::new();
            for (key, value) in &message.metadata {
                headers.insert(key.as_str(), value.as_str());
            }
            headers
        } else {
            HeaderMap::new()
        };

        match &self.client {
            NatsClient::JetStream(jetstream) => {
                let ack_future = jetstream
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await
                    .context("Failed to publish to NATS JetStream")?;

                if !self.delayed_ack {
                    ack_future
                        .await
                        .context("Failed to get NATS JetStream publish acknowledgement")?;
                    // Wait for the server acknowledgment
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
        // not a real bulk, but fast enough
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

enum NatsSubscription {
    Core(async_nats::Subscriber),
    JetStream(Box<jetstream::consumer::pull::Stream>),
}

pub struct NatsConsumer {
    subscription: NatsSubscription,
}
use std::any::Any;

impl NatsConsumer {
    pub async fn new(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
    ) -> anyhow::Result<Self> {
        let options = build_nats_options(config).await?;
        let client = options.connect(&config.url).await?;

        let subscription = if !config.no_jetstream {
            let jetstream = jetstream::new(client);
            info!(stream = %stream_name, subject = %subject, "NATS consumer is in JetStream mode.");

            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![format!("{}.>", stream_name)],
                    ..Default::default()
                })
                .await?;

            let stream = jetstream.get_stream(stream_name).await?;

            let max_ack_pending = config.prefetch_count.unwrap_or(10000) as i64;
            let consumer = stream
                .create_consumer(jetstream::consumer::pull::Config {
                    durable_name: Some(format!(
                        "{}-{}-{}",
                        APP_NAME,
                        stream_name,
                        subject.replace('.', "-")
                    )),
                    filter_subject: subject.to_string(),
                    deliver_policy: jetstream::consumer::DeliverPolicy::All,
                    max_ack_pending,
                    ..Default::default()
                })
                .await?;

            let stream = consumer.messages().await?;
            info!(stream = %stream_name, subject = %subject, "NATS JetStream source subscribed");
            NatsSubscription::JetStream(Box::new(stream))
        } else {
            info!(subject = %subject, "NATS consumer is in Core mode (non-persistent).");
            // For Core NATS, we use a queue group to load-balance messages.
            // The queue group name can be derived from the stream/subject to be unique per route.
            let queue_group = format!("{}-{}", APP_NAME, stream_name.replace('.', "-")); // The queue_group can be a String
            let sub = client
                .queue_subscribe(subject.to_string(), queue_group.clone())
                .await?;
            info!(subject = %subject, queue_group = %queue_group, "NATS Core source subscribed");
            NatsSubscription::Core(sub)
        };

        Ok(Self { subscription })
    }
    fn create_canonical_message(
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
                    // A header key can have multiple values. We'll just take the first one.
                    if let Some(first_value) = value.iter().next() {
                        metadata.insert(key.to_string(), first_value.to_string());
                    }
                }
                canonical_message.metadata = metadata;
            }
        }
        canonical_message
    }
}

#[async_trait]
impl MessageConsumer for NatsConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let (message, commit) = match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                let message = futures::StreamExt::next(stream)
                    .await
                    .ok_or(ConsumerError::EndOfStream)?
                    .context("Failed to get message from NATS JetStream")?;
                let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                let msg = Self::create_canonical_message(&message, sequence);
                let commit: CommitFunc = Box::new(move |_response| {
                    Box::pin(async move {
                        message.ack().await.unwrap_or_else(|e| {
                            tracing::error!("Failed to ACK NATS message: {:?}", e)
                        });
                    }) as BoxFuture<'static, ()>
                });
                (msg, commit)
            }
            NatsSubscription::Core(sub) => {
                let message = futures::StreamExt::next(sub)
                    .await
                    .ok_or(ConsumerError::EndOfStream)?;

                // Core NATS has no ack, so the commit is a no-op.
                let commit: CommitFunc = Box::new(move |_| Box::pin(async {}));
                let msg = Self::create_canonical_message(&message, None);
                (msg, commit)
            }
        };

        Ok(Received { message, commit })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async {})),
            });
        }

        match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                let mut canonical_messages = Vec::with_capacity(max_messages);
                let mut jetstream_messages = Vec::with_capacity(max_messages);

                // Use a short timeout to make the batch fetch non-blocking if no messages are available.
                let message_stream = stream.next().await;

                // Process the first message if it exists
                if let Some(Ok(first_message)) = message_stream {
                    let sequence = first_message.info().ok().map(|meta| meta.stream_sequence);
                    canonical_messages
                        .push(Self::create_canonical_message(&first_message, sequence));
                    jetstream_messages.push(first_message);

                    // Greedily fetch the rest of the batch
                    while canonical_messages.len() < max_messages {
                        if let Ok(Some(message)) = stream.try_next().await {
                            let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                            canonical_messages
                                .push(Self::create_canonical_message(&message, sequence));
                            jetstream_messages.push(message);
                        } else {
                            break; // No more messages in the buffer
                        }
                    }
                }

                let commit_closure: BatchCommitFunc = Box::new(move |_responses| {
                    Box::pin(async move {
                        // Acknowledge messages concurrently.
                        // A concurrency limit of 100 is chosen to balance parallelism
                        // with not overwhelming the NATS server or spawning too many tasks.
                        futures::stream::iter(jetstream_messages)
                            // Limit concurrent acks to avoid overwhelming the server
                            .for_each_concurrent(Some(100), |message| async move {
                                if let Err(e) = message.ack().await {
                                    tracing::error!("Failed to ACK NATS message: {:?}", e);
                                }
                            })
                            .await;
                    }) as BoxFuture<'static, ()>
                });

                if canonical_messages.is_empty() {
                    Err(ConsumerError::EndOfStream)
                } else {
                    Ok(ReceivedBatch {
                        messages: canonical_messages,
                        commit: commit_closure,
                    })
                }
            }
            NatsSubscription::Core(sub) => {
                let mut messages = Vec::new();
                // Core NATS has no ack, so the commit is a no-op.
                // Just read one message for now, optimize it later when needed
                let commit_closure: BatchCommitFunc = Box::new(|_| Box::pin(async {}));

                if let Some(message) = sub.next().await {
                    messages.push(Self::create_canonical_message(&message, None));
                }
                if messages.is_empty() {
                    Err(ConsumerError::EndOfStream)
                } else {
                    Ok(ReceivedBatch {
                        messages,
                        commit: commit_closure,
                    })
                }
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct NatsSubscriber {
    subscription: NatsSubscription,
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
        let options = build_nats_options(config).await?;
        let client = options.connect(&config.url).await?;

        let subscription = if !config.no_jetstream {
            let jetstream = jetstream::new(client);
            info!(stream = %stream_name, subject = %subject, "NATS event subscriber is in JetStream mode.");

            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![format!("{}.>", stream_name)],
                    ..Default::default()
                })
                .await?;

            let stream = jetstream.get_stream(stream_name).await?;

            let max_ack_pending = config.prefetch_count.unwrap_or(10000) as i64;
            let consumer = stream
                .create_consumer(jetstream::consumer::pull::Config {
                    // Ephemeral consumer (no durable_name)
                    filter_subject: subject.to_string(),
                    deliver_policy: jetstream::consumer::DeliverPolicy::New, // Only new messages
                    max_ack_pending,
                    ..Default::default()
                })
                .await?;

            let stream = consumer.messages().await?;
            info!(stream = %stream_name, subject = %subject, "NATS JetStream event subscriber subscribed");
            NatsSubscription::JetStream(Box::new(stream))
        } else {
            info!(subject = %subject, "NATS event subscriber is in Core mode.");
            // For Core NATS, we use a standard subscription (no queue group) for broadcast.
            let sub = client.subscribe(subject.to_string()).await?;
            info!(subject = %subject, "NATS Core event subscriber subscribed");
            NatsSubscription::Core(sub)
        };

        Ok(Self { subscription })
    }
}

#[async_trait]
impl MessageConsumer for NatsSubscriber {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        // Logic is identical to NatsConsumer, delegating to the subscription
        let (message, commit) = match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                let message = futures::StreamExt::next(stream)
                    .await
                    .ok_or(ConsumerError::EndOfStream)?
                    .context("Failed to get message from NATS JetStream")?;
                let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                let msg = NatsConsumer::create_canonical_message(&message, sequence);
                let commit: CommitFunc = Box::new(move |_response| {
                    Box::pin(async move {
                        message.ack().await.unwrap_or_else(|e| {
                            tracing::error!("Failed to ACK NATS message: {:?}", e)
                        });
                    }) as BoxFuture<'static, ()>
                });
                (msg, commit)
            }
            NatsSubscription::Core(sub) => {
                let message = futures::StreamExt::next(sub)
                    .await
                    .ok_or(ConsumerError::EndOfStream)?;
                let commit: CommitFunc = Box::new(move |_| Box::pin(async {}));
                let msg = NatsConsumer::create_canonical_message(&message, None);
                (msg, commit)
            }
        };
        Ok(Received { message, commit })
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // Logic is identical to NatsConsumer
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async {})),
            });
        }
        match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                // ... (Same implementation as NatsConsumer::receive_batch for JetStream)
                // Since we can't easily share the code without refactoring NatsConsumer into a generic,
                // and the request is to add the subscriber, we reuse the logic by instantiating a NatsConsumer
                // temporarily or duplicating. Duplication is cleaner for now to avoid breaking existing code.
                // However, to save space in this response, I will implement a simplified batch fetch for the subscriber
                // which delegates to receive() loop or implements the same logic.
                // Let's implement the same logic for correctness.
                let mut canonical_messages = Vec::with_capacity(max_messages);
                let mut jetstream_messages = Vec::with_capacity(max_messages);
                let message_stream = stream.next().await;
                if let Some(Ok(first_message)) = message_stream {
                    let sequence = first_message.info().ok().map(|meta| meta.stream_sequence);
                    canonical_messages.push(NatsConsumer::create_canonical_message(
                        &first_message,
                        sequence,
                    ));
                    jetstream_messages.push(first_message);
                    while canonical_messages.len() < max_messages {
                        if let Ok(Some(message)) = stream.try_next().await {
                            let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                            canonical_messages
                                .push(NatsConsumer::create_canonical_message(&message, sequence));
                            jetstream_messages.push(message);
                        } else {
                            break;
                        }
                    }
                }
                let commit_closure: BatchCommitFunc = Box::new(move |_responses| {
                    Box::pin(async move {
                        futures::stream::iter(jetstream_messages)
                            .for_each_concurrent(Some(100), |message| async move {
                                if let Err(e) = message.ack().await {
                                    tracing::error!("Failed to ACK NATS message: {:?}", e);
                                }
                            })
                            .await;
                    }) as BoxFuture<'static, ()>
                });
                if canonical_messages.is_empty() {
                    Err(ConsumerError::EndOfStream)
                } else {
                    Ok(ReceivedBatch {
                        messages: canonical_messages,
                        commit: commit_closure,
                    })
                }
            }
            NatsSubscription::Core(sub) => {
                let mut messages = Vec::new();
                let commit_closure: BatchCommitFunc = Box::new(|_| Box::pin(async {}));
                if let Some(message) = sub.next().await {
                    messages.push(NatsConsumer::create_canonical_message(&message, None));
                }
                if messages.is_empty() {
                    Err(ConsumerError::EndOfStream)
                } else {
                    Ok(ReceivedBatch {
                        messages,
                        commit: commit_closure,
                    })
                }
            }
        }
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
