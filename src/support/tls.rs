//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! rustls configs built from a [`TlsConfig`], shared by the endpoints that listen
//! or connect over TLS themselves.

use crate::models::TlsConfig;
use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tracing::warn;

/// A server config from `tls`: `cert_file` and `key_file`, and client certificates
/// checked against `ca_file` if that is set. ALPN is left to the caller.
pub(crate) fn server_config(tls_config: &TlsConfig) -> anyhow::Result<rustls::ServerConfig> {
    let cert_file = tls_config
        .cert_file
        .as_ref()
        .context("TLS cert_file not provided for server")?;
    let key_file = tls_config
        .key_file
        .as_ref()
        .context("TLS key_file not provided for server")?;

    let certs = load_certs(cert_file)?;
    let key = load_private_key(key_file)?;

    let config_builder =
        rustls::ServerConfig::builder_with_provider(crate::endpoints::get_crypto_provider()?)
            .with_safe_default_protocol_versions()?;

    let config = if let Some(ca_file) = &tls_config.ca_file {
        // mTLS: verify client certificates using the provided CA
        let mut client_auth_roots = rustls::RootCertStore::empty();
        let mut pem = BufReader::new(File::open(ca_file).with_context(|| {
            format!(
                "Failed to open CA file for client verification: {}",
                ca_file
            )
        })?);
        for cert in rustls_pemfile::certs(&mut pem) {
            client_auth_roots.add(cert?)?;
        }
        let client_verifier =
            rustls::server::WebPkiClientVerifier::builder(std::sync::Arc::new(client_auth_roots))
                .build()
                .context("Failed to build client certificate verifier")?;
        config_builder
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)
            .context("Failed to build rustls mTLS server config")?
    } else {
        // Standard TLS: no client certificate verification
        config_builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("Failed to build rustls server config")?
    };

    Ok(config)
}

/// A client config from `tls`: its CA or the public roots, and a client certificate if given.
pub(crate) fn client_config(tls_config: &TlsConfig) -> anyhow::Result<rustls::ClientConfig> {
    let mut root_cert_store = rustls::RootCertStore::empty();

    if let Some(ca_file) = &tls_config.ca_file {
        let mut pem = BufReader::new(
            File::open(ca_file).with_context(|| format!("Failed to open CA file: {}", ca_file))?,
        );
        for cert in rustls_pemfile::certs(&mut pem) {
            root_cert_store.add(cert?)?;
        }
    } else {
        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let config_builder =
        rustls::ClientConfig::builder_with_provider(crate::endpoints::get_crypto_provider()?)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_cert_store);

    let mut config =
        if let (Some(cert_file), Some(key_file)) = (&tls_config.cert_file, &tls_config.key_file) {
            let certs = load_certs(cert_file)?;
            let key = load_private_key(key_file)?;
            config_builder
                .with_client_auth_cert(certs, key)
                .context("Failed to build mTLS client config")?
        } else {
            config_builder.with_no_client_auth()
        };

    if tls_config.accept_invalid_certs {
        // INSECURE: disables server certificate and hostname verification, so the
        // connection is open to man-in-the-middle attacks. Honoured only because the
        // caller explicitly opted in via `accept_invalid_certs`; never for production.
        warn!("TLS client is configured to accept invalid certificates. This is insecure and must not be used in production.");
        let schemes = crate::endpoints::get_crypto_provider()?
            .signature_verification_algorithms
            .supported_schemes();
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoopClientCertVerifier {
                supported_schemes: schemes,
            }));
    }

    Ok(config)
}

/// A rustls server-certificate verifier that performs no validation. Used only when
/// `accept_invalid_certs` is set.
#[derive(Debug)]
struct NoopClientCertVerifier {
    supported_schemes: Vec<rustls::SignatureScheme>,
}

impl rustls::client::danger::ServerCertVerifier for NoopClientCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported_schemes.clone()
    }
}

/// Loads certificate chains from a PEM file.
pub(crate) fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let mut cert_file = BufReader::new(
        File::open(path).with_context(|| format!("Cannot open cert file {}", path))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_file).collect::<Result<Vec<_>, _>>()?;
    Ok(certs)
}

/// Loads a private key from a PEM file.
pub(crate) fn load_private_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let mut key_file =
        BufReader::new(File::open(path).with_context(|| format!("Cannot open key file {}", path))?);
    rustls_pemfile::private_key(&mut key_file)?.context("No private key found in file")
}
