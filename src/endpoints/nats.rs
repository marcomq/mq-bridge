use crate::models::NatsConfig;
use crate::traits::{BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_nats::{header::HeaderMap, jetstream, jetstream::stream, ConnectOptions};
use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring as rustls_ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::io::BufReader;
use std::sync::Arc;
use tokio::time::Duration;
use tracing::info;

pub struct NatsPublisher {
    jetstream: jetstream::Context,
    subject: String,
    await_ack: bool,
}

impl NatsPublisher {
    pub async fn new(
        config: &NatsConfig,
        subject: &str,
        stream_name: Option<&str>,
    ) -> anyhow::Result<Self> {
        let options = build_nats_options(config).await?;
        let client = options.connect(&config.url).await?;
        let jetstream = jetstream::new(client.clone());

        // Ensure the stream exists. This is idempotent.
        // The stream name is now passed in directly.
        if let Some(stream_name) = stream_name {
            info!(stream = %stream_name, "Ensuring NATS stream exists");
            jetstream
                .get_or_create_stream(stream::Config {
                    name: stream_name.to_string(),
                    // all subjects that start with the stream name.
                    subjects: vec![format!("{}.>", stream_name)], // e.g., "perf_stream_nats.>"
                    ..Default::default()
                })
                .await?;
        } else {
            info!("No default_stream configured for NATS sink, skipping stream creation. This may not work with a JetStream source.");
        }

        Ok(Self {
            jetstream,
            subject: subject.to_string(),
            await_ack: config.await_ack,
        })
    }

    pub fn with_subject(&self, subject: &str) -> Self {
        Self {
            jetstream: self.jetstream.clone(),
            subject: subject.to_string(),
            await_ack: self.await_ack,
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

        let ack_future = self
            .jetstream
            .publish_with_headers(self.subject.clone(), headers, message.payload.into())
            .await?;

        if self.await_ack {
            ack_future.await?; // Wait for the server acknowledgment
        }
        Ok(None)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub struct NatsConsumer {
    _jetstream: jetstream::Context,
    subscription: jetstream::consumer::pull::Stream,
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
        let jetstream = jetstream::new(client);

        // Ensure the stream exists. This is idempotent and safe to call from both
        // publisher and consumer. It prevents race conditions where the consumer
        // might start before the publisher has created the stream.
        jetstream
            .get_or_create_stream(stream::Config {
                name: stream_name.to_string(),
                subjects: vec![format!("{}.>", stream_name)],
                ..Default::default()
            })
            .await?;

        // Now that we've ensured the stream exists, we can get a handle to it.
        // This is more direct and less prone to race conditions than the previous retry loop.
        let stream = jetstream.get_stream(stream_name).await?;

        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                // Create a unique, durable consumer name based on stream and subject
                // to allow for multiple routes from the same stream.
                durable_name: Some(format!(
                    "mq-bridge-{}-{}",
                    stream_name,
                    subject.replace('.', "-")
                )),
                filter_subject: subject.to_string(),
                // Start from the beginning of the stream for all messages
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                // Set a reasonable max ack pending to ensure we don't lose messages
                max_ack_pending: 10000,
                ..Default::default()
            })
            .await?;

        // Give the consumer a moment to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        let subscription = consumer.messages().await?;
        info!(stream = %stream_name, subject = %subject, "NATS source subscribed to subject");

        Ok(Self {
            _jetstream: jetstream,
            subscription,
        })
    }
}

#[async_trait]
impl MessageConsumer for NatsConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let message = futures::StreamExt::next(&mut self.subscription)
            .await
            .ok_or_else(|| anyhow!("NATS subscription stream ended"))??;

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

        let commit = Box::new(move |_response| {
            Box::pin(async move {
                message
                    .ack()
                    .await
                    .unwrap_or_else(|e| tracing::error!("Failed to ACK NATS message: {:?}", e))
            }) as BoxFuture<'static, ()>
        });

        Ok((canonical_message, commit))
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
