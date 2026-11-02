//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The operating system's CA certificates, for `tls.ca_file: "$.os"`.

use rustls::pki_types::CertificateDer;
use std::sync::OnceLock;

#[cfg(target_os = "android")]
const UNAVAILABLE: &str = "tls.ca_file \"$.os\" is not available on Android: its CA store is only \
    reachable through Java; leave ca_file unset or name a CA file";

/// The OS CA certificates, loaded once per process; a failed load is tried again next time.
pub(crate) fn os_roots() -> anyhow::Result<&'static [CertificateDer<'static>]> {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    if let Some(roots) = ROOTS.get() {
        return Ok(roots);
    }
    let roots = load().map_err(|reason| anyhow::anyhow!("{reason}"))?;
    Ok(ROOTS.get_or_init(|| roots))
}

/// The OS CA certificates as one PEM bundle, for drivers that take PEM bytes.
#[allow(dead_code)]
pub(crate) fn os_roots_pem() -> anyhow::Result<String> {
    let mut pem = String::new();
    for cert in os_roots()? {
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = crate::support::base64_engine::encode(cert.as_ref());
        for line in encoded.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
    }
    Ok(pem)
}

#[cfg(target_os = "android")]
fn load() -> Result<Vec<CertificateDer<'static>>, String> {
    Err(UNAVAILABLE.to_string())
}

#[cfg(not(target_os = "android"))]
fn load() -> Result<Vec<CertificateDer<'static>>, String> {
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        rustls_native_certs::load_native_certs();
    for error in &errors {
        tracing::warn!(%error, "Skipped part of the OS CA store");
    }
    if certs.is_empty() {
        return Err(format!(
            "tls.ca_file \"$.os\": the OS CA store has no certificates ({} errors)",
            errors.len()
        ));
    }
    Ok(certs)
}

#[cfg(all(test, feature = "rustls-pemfile", not(target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn the_os_store_is_not_empty_and_encodes_as_pem() {
        let roots = match os_roots() {
            Ok(roots) => roots,
            Err(e) => return eprintln!("skipping, no OS CA store: {e}"),
        };
        let pem = os_roots_pem().unwrap();
        let parsed = rustls_pemfile::certs(&mut pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(parsed.len(), roots.len());
    }
}
