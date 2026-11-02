//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! rustls configs built from a [`TlsConfig`], shared by the endpoints that listen
//! or connect over TLS themselves.

use crate::models::{CaRoots, TlsConfig};
use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tracing::warn;

/// A server config from `tls`: `cert_file` and `key_file`, and client certificates
/// checked against `ca_file` if that is set. ALPN is left to the caller.
#[cfg(any(feature = "http", feature = "websocket"))]
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
        if !matches!(tls_config.ca_roots(), CaRoots::File(_)) {
            anyhow::bail!(
                "tls.ca_file \"{ca_file}\" is only for clients; a server needs a CA file"
            );
        }
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
#[cfg(any(feature = "http", feature = "websocket", feature = "nats"))]
pub(crate) fn client_config(tls_config: &TlsConfig) -> anyhow::Result<rustls::ClientConfig> {
    check_client_pair(tls_config)?;
    if tls_config.accept_invalid_certs {
        warn!("TLS client is configured to accept invalid certificates. This is insecure and must not be used in production.");
    }
    client_config_with(tls_config, crate::endpoints::get_crypto_provider()?)
}

/// TLS for a reqwest client. Where reqwest's OS verifier works, it stays as before,
/// `ca_file` adding to the OS roots; elsewhere this builds the rustls config itself.
#[cfg(any(feature = "http-bulk", feature = "clickhouse"))]
pub(crate) fn reqwest_tls(
    builder: reqwest::ClientBuilder,
    tls_config: &TlsConfig,
    endpoint: &str,
) -> anyhow::Result<reqwest::ClientBuilder> {
    if tls_config.cert_password.is_some() {
        anyhow::bail!("{endpoint} tls.cert_password is not supported; use an unencrypted key");
    }
    if tls_config.cert_file.is_none() && tls_config.key_file.is_some() {
        anyhow::bail!("{endpoint} tls.key_file needs tls.cert_file");
    }
    if uses_os_verifier(tls_config.ca_roots(), cfg!(target_os = "android")) {
        return os_verifier_tls(builder, tls_config, endpoint);
    }
    let mut tls_config = tls_config.clone();
    if tls_config.key_file.is_none() {
        tls_config.key_file = tls_config.cert_file.clone();
    }
    // Same fallback as reqwest when the application installed no provider.
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut config = client_config_with(&tls_config, provider)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(builder.tls_backend_preconfigured(config))
}

/// reqwest's OS verifier panics on Android unless the app initialised it through JNI.
#[cfg(any(feature = "http-bulk", feature = "clickhouse"))]
fn uses_os_verifier(roots: CaRoots<'_>, android: bool) -> bool {
    !android && !matches!(roots, CaRoots::Bundled)
}

#[cfg(any(feature = "http-bulk", feature = "clickhouse"))]
fn os_verifier_tls(
    mut builder: reqwest::ClientBuilder,
    tls_config: &TlsConfig,
    endpoint: &str,
) -> anyhow::Result<reqwest::ClientBuilder> {
    if tls_config.accept_invalid_certs {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let CaRoots::File(ca) = tls_config.ca_roots() {
        let pem = std::fs::read(ca)
            .with_context(|| format!("Failed to read {endpoint} CA file '{ca}'"))?;
        builder = builder.add_root_certificate(
            reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("Invalid {endpoint} CA certificate '{ca}'"))?,
        );
    }
    if let Some(cert) = &tls_config.cert_file {
        let mut pem = std::fs::read(cert)
            .with_context(|| format!("Failed to read {endpoint} cert file '{cert}'"))?;
        if let Some(key) = &tls_config.key_file {
            pem.push(b'\n');
            pem.extend(
                std::fs::read(key)
                    .with_context(|| format!("Failed to read {endpoint} key file '{key}'"))?,
            );
        }
        builder = builder.identity(
            reqwest::Identity::from_pem(&pem)
                .with_context(|| format!("Invalid {endpoint} client certificate '{cert}'"))?,
        );
    }
    Ok(builder)
}

/// [`client_config`] with an explicit crypto provider.
pub(crate) fn client_config_with(
    tls_config: &TlsConfig,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> anyhow::Result<rustls::ClientConfig> {
    check_client_pair(tls_config)?;
    let root_cert_store = root_store(tls_config)?;
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let config_builder = rustls::ClientConfig::builder_with_provider(provider)
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
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoopClientCertVerifier {
                supported_schemes: schemes,
            }));
    }

    Ok(config)
}

fn check_client_pair(tls_config: &TlsConfig) -> anyhow::Result<()> {
    if tls_config.cert_file.is_some() != tls_config.key_file.is_some() {
        anyhow::bail!("a TLS client certificate needs both cert_file and key_file");
    }
    Ok(())
}

/// The roots `tls.ca_file` names: a CA file, `$.os` or `$.bundled`; unset, the bundled roots.
pub(crate) fn root_store(tls_config: &TlsConfig) -> anyhow::Result<rustls::RootCertStore> {
    let mut store = rustls::RootCertStore::empty();
    match tls_config.ca_roots() {
        CaRoots::Default | CaRoots::Bundled => {
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        CaRoots::Os => {
            let roots = crate::support::os_roots::os_roots()?;
            let (added, ignored) = store.add_parsable_certificates(roots.iter().cloned());
            if ignored > 0 {
                warn!(
                    added,
                    ignored, "Ignored unparsable certificates in the OS CA store"
                );
            }
        }
        CaRoots::File(ca_file) => {
            let mut pem = BufReader::new(
                File::open(ca_file)
                    .with_context(|| format!("Failed to open CA file: {}", ca_file))?,
            );
            for cert in rustls_pemfile::certs(&mut pem) {
                store.add(cert?)?;
            }
        }
    }
    Ok(store)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// With one of the pair missing no certificate was sent, and only the server said so.
    #[cfg(any(feature = "http", feature = "websocket", feature = "nats"))]
    #[test]
    fn half_a_client_certificate_is_refused() {
        for (cert_file, key_file) in [(Some("c.pem"), None), (None, Some("k.pem"))] {
            let tls = TlsConfig {
                required: true,
                cert_file: cert_file.map(str::to_owned),
                key_file: key_file.map(str::to_owned),
                ..Default::default()
            };
            let error = client_config(&tls).unwrap_err().to_string();
            assert!(error.contains("cert_file and key_file"), "{error}");
        }
    }

    #[test]
    fn ca_file_keywords_pick_the_roots() {
        let bundled = webpki_roots::TLS_SERVER_ROOTS.len();
        for ca_file in [None, Some("$.bundled")] {
            let tls = TlsConfig {
                ca_file: ca_file.map(str::to_owned),
                ..Default::default()
            };
            assert_eq!(root_store(&tls).unwrap().len(), bundled);
        }
        #[cfg(not(target_os = "android"))]
        match crate::support::os_roots::os_roots() {
            Ok(_) => assert!(!root_store(&TlsConfig::new().with_ca_file("$.os"))
                .unwrap()
                .is_empty()),
            Err(e) => eprintln!("skipping $.os: {e}"),
        }
        let missing = root_store(&TlsConfig::new().with_ca_file("$nope")).unwrap_err();
        assert!(missing.to_string().contains("$nope"), "{missing}");
    }

    #[cfg(any(feature = "http-bulk", feature = "clickhouse"))]
    #[test]
    fn reqwest_keeps_the_os_verifier_except_on_android_or_for_bundled_roots() {
        for roots in [CaRoots::Default, CaRoots::Os, CaRoots::File("ca.pem")] {
            assert!(uses_os_verifier(roots, false));
            assert!(!uses_os_verifier(roots, true));
        }
        assert!(!uses_os_verifier(CaRoots::Bundled, false));
    }
}
