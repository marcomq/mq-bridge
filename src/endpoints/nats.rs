use crate::models::NatsConfig;
use crate::traits::{BatchCommitFunc, BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_nats::{header::HeaderMap, jetstream, jetstream::stream, ConnectOptions};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring as rustls_ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::io::BufReader;
use std::sync::Arc;
use tokio::time::Duration;
use tracing::{info, warn};

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
        subject: &str,
        stream_name: Option<&str>,
    ) -> anyhow::Result<Self> {
        let options = build_nats_options(config).await?;
        let nats_client = options.connect(&config.url).await?;

        let client = if !config.no_jetstream {
            let jetstream = jetstream::new(nats_client);
            // Ensure the stream exists. This is idempotent.
            if let Some(stream_name) = stream_name {
                info!(stream = %stream_name, "Ensuring NATS JetStream stream exists");
                jetstream
                    .get_or_create_stream(stream::Config {
                        name: stream_name.to_string(),
                        subjects: vec![format!("{}.>", stream_name)],
                        ..Default::default()
                    })
                    .await?;
            } else {
                warn!("NATS publisher is in JetStream mode but no 'stream' is configured. Publishing may fail if stream does not exist.");
            }
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

    pub fn with_subject(&self, subject: &str) -> Self {
        // This clone is cheap because NatsClient holds an Arc-like client internally.
        let client = match &self.client {
            NatsClient::Core(c) => NatsClient::Core(c.clone()),
            NatsClient::JetStream(j) => NatsClient::JetStream(j.clone()),
        };
        Self {
            client,
            subject: subject.to_string(),
            delayed_ack: self.delayed_ack,
        }
    }
}

#[async_trait]
impl MessagePublisher for NatsPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let headers = if let Some(metadata) = &message.metadata {
            let mut headers = HeaderMap::new();
            for (key, value) in metadata {
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
                    .await?;

                if !self.delayed_ack {
                    ack_future.await?; // Wait for the server acknowledgment
                }
            }
            NatsClient::Core(client) => {
                client
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await?;
            }
        }

        Ok(None)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
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

            let consumer = stream
                .create_consumer(jetstream::consumer::pull::Config {
                    durable_name: Some(format!(
                        "mq-bridge-{}-{}",
                        stream_name,
                        subject.replace('.', "-")
                    )),
                    filter_subject: subject.to_string(),
                    deliver_policy: jetstream::consumer::DeliverPolicy::All,
                    max_ack_pending: 10000,
                    ..Default::default()
                })
                .await?;

            tokio::time::sleep(Duration::from_millis(100)).await;

            let stream = consumer.messages().await?;
            info!(stream = %stream_name, subject = %subject, "NATS JetStream source subscribed");
            NatsSubscription::JetStream(Box::new(stream))
        } else {
            info!(subject = %subject, "NATS consumer is in Core mode (non-persistent).");
            // For Core NATS, we use a queue group to load-balance messages.
            // The queue group name can be derived from the stream/subject to be unique per route.
            let queue_group = format!("mq-bridge-{}", stream_name.replace('.', "-")); // The queue_group can be a String
            let sub = client
                .queue_subscribe(subject.to_string(), queue_group.clone())
                .await?;
            info!(subject = %subject, queue_group = %queue_group, "NATS Core source subscribed");
            NatsSubscription::Core(sub)
        };

        Ok(Self { subscription })
    }
    fn create_canonical_message(message: &async_nats::Message) -> CanonicalMessage {
        let mut canonical_message = CanonicalMessage::new(message.payload.to_vec());

        if let Some(headers) = &message.headers {
            if !headers.is_empty() {
                let mut metadata = std::collections::HashMap::new();
                for (key, value) in headers.iter() {
                    // A header key can have multiple values. We'll just take the first one.
                    if let Some(first_value) = value.iter().next() {
                        metadata.insert(key.to_string(), first_value.to_string());
                    }
                }
                canonical_message.metadata = Some(metadata);
            }
        }
        canonical_message
    }
}

#[async_trait]
impl MessageConsumer for NatsConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let (message, commit) = match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                let message = futures::StreamExt::next(stream)
                    .await
                    .ok_or_else(|| anyhow!("NATS JetStream subscription ended"))??;
                let msg = Self::create_canonical_message(&message);
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
                    .ok_or_else(|| anyhow!("NATS Core subscription ended"))?;

                // Core NATS has no ack, so the commit is a no-op.
                let commit: CommitFunc = Box::new(move |_| Box::pin(async {}));
                let msg = Self::create_canonical_message(&message);
                (msg, commit)
            }
        };

        Ok((message, commit))
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        if max_messages == 0 {
            return Ok((Vec::new(), Box::new(|_| Box::pin(async {}))));
        }

        match &mut self.subscription {
            NatsSubscription::JetStream(stream) => {
                let mut canonical_messages = Vec::with_capacity(max_messages);
                let mut jetstream_messages = Vec::with_capacity(max_messages);

                // Use a short timeout to make the batch fetch non-blocking if no messages are available.
                let message_stream = stream.next().await;

                // Process the first message if it exists
                if let Some(Ok(first_message)) = message_stream {
                    canonical_messages.push(Self::create_canonical_message(&first_message));
                    jetstream_messages.push(first_message);

                    // Greedily fetch the rest of the batch
                    while canonical_messages.len() < max_messages {
                        if let Ok(Some(message)) = stream.try_next().await {
                            canonical_messages.push(Self::create_canonical_message(&message));
                            jetstream_messages.push(message);
                        } else {
                            break; // No more messages in the buffer
                        }
                    }
                }

                let commit_closure: BatchCommitFunc = Box::new(move |_responses| {
                        Box::pin(async move {
                            // Acknowledge messages concurrently.
                            // A concurrency limit of 1000 is chosen to balance parallelism
                            // with not overwhelming the NATS server or spawning too many tasks.
                            futures::stream::iter(jetstream_messages)
                                .for_each_concurrent(
                                    Some(1000),
                                    |message| async move {
                                        if let Err(e) = message.ack().await {
                                            tracing::error!("Failed to ACK NATS message: {:?}", e);
                                        }
                                    })
                                .await;
                        }) as BoxFuture<'static, ()>
                    });

                Ok((canonical_messages, commit_closure))
            }
            NatsSubscription::Core(sub) => {
                let mut messages = Vec::new();
                // Core NATS has no ack, so the commit is a no-op.
                // Just read one message for now, optimize it later when needed
                let commit_closure: BatchCommitFunc = Box::new(|_| Box::pin(async {}));

                if let Some(message) = sub.next().await {
                    messages.push(Self::create_canonical_message(&message));
                }
                Ok((messages, commit_closure))
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
