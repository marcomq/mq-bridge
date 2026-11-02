//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Warnings for a connection whose settings leave it exposed.

use crate::models::CaRoots;
use tracing::warn;

fn parsed(url: &str) -> Option<url::Url> {
    if url.contains("://") {
        url::Url::parse(url).ok()
    } else {
        url::Url::parse(&format!("tcp://{url}")).ok()
    }
}

/// Whether every server in `url` (one URL, or a comma-separated list) is on this host.
pub(crate) fn is_loopback(url: &str) -> bool {
    url.split(',').all(|server| {
        let Some(server) = parsed(server.trim()) else {
            return false;
        };
        let host = server.host_str().unwrap_or_default();
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn url_has_password(url: &str) -> bool {
    url.split(',').any(|server| {
        parsed(server.trim()).is_some_and(|server| {
            server
                .password()
                .is_some_and(|password| !password.is_empty())
        })
    })
}

/// Whether a password or token would cross the network without encryption.
pub(crate) fn exposes_credentials(url: &str, has_credentials: bool, encrypted: bool) -> bool {
    !encrypted && (has_credentials || url_has_password(url)) && !is_loopback(url)
}

/// Warns when [`exposes_credentials`] holds. `has_credentials` covers the config fields; a
/// password in the URL is found here.
pub(crate) fn warn_plaintext_credentials(
    endpoint: &str,
    url: &str,
    has_credentials: bool,
    encrypted: bool,
) {
    if exposes_credentials(url, has_credentials, encrypted) {
        warn!(
            url = %crate::support::redact::url_password(url),
            "{endpoint} sends its credentials unencrypted; enable tls"
        );
    }
}

/// Warns that `tls.accept_invalid_certs` turns off the check of the server certificate.
// Not every single-feature build has a caller.
#[allow(dead_code)]
pub(crate) fn warn_unverified(endpoint: &str, accept_invalid_certs: bool) {
    if accept_invalid_certs {
        warn!("{endpoint} tls.accept_invalid_certs is set: the server certificate is not checked. Do not use it in production.");
    }
}

/// Warns that a listener has a certificate but serves without TLS, as `tls.required` is off.
#[allow(dead_code)]
pub(crate) fn warn_listener_cert_unused(endpoint: &str, tls: &crate::models::TlsConfig) {
    if !tls.required && tls.cert_file.is_some() {
        warn!("{endpoint} has tls.cert_file but tls.required is off: it listens without TLS.");
    }
}

/// `tls.ca_file` as a path for a driver that takes only paths and trusts `own_roots`
/// by itself: that keyword means no path, the other one is refused.
pub(crate) fn ca_file_path<'a>(
    endpoint: &str,
    tls: &'a crate::models::TlsConfig,
    own_roots: CaRoots<'static>,
) -> anyhow::Result<Option<&'a str>> {
    match tls.ca_roots() {
        CaRoots::Default => Ok(None),
        CaRoots::File(path) => Ok(Some(path)),
        roots if roots == own_roots => Ok(None),
        CaRoots::Os => anyhow::bail!(
            "{endpoint} cannot use tls.ca_file \"$.os\": its driver takes only a CA file path; \
             name the OS bundle file instead, e.g. /etc/ssl/certs/ca-certificates.crt"
        ),
        CaRoots::Bundled => anyhow::bail!(
            "{endpoint} cannot use tls.ca_file \"$.bundled\": its driver trusts the OS CA store; \
             leave ca_file unset or name a CA file"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_to_another_host_without_tls_is_exposed() {
        assert!(exposes_credentials("amqp://broker:5672", true, false));
        assert!(exposes_credentials(
            "redis://user:secret@cache:6379",
            false,
            false
        ));
        assert!(exposes_credentials(
            "nats://127.0.0.1:4222,nats://other:4222",
            true,
            false
        ));
        assert!(exposes_credentials("broker:1883", true, false));
    }

    #[test]
    fn tls_a_local_server_or_no_password_is_not_exposed() {
        assert!(!exposes_credentials("amqp://broker:5672", true, true));
        assert!(!exposes_credentials("amqp://broker:5672", false, false));
        assert!(!exposes_credentials(
            "redis://user@cache:6379",
            false,
            false
        ));
        assert!(!exposes_credentials(
            "redis://user:secret@localhost:6379",
            false,
            false
        ));
        assert!(!exposes_credentials(
            "mongodb://user:secret@[::1]:27017",
            false,
            false
        ));
        assert!(!exposes_credentials("127.0.0.1:4222", true, false));
    }

    #[test]
    fn a_path_only_driver_maps_its_own_roots_and_refuses_the_other() {
        use crate::models::TlsConfig;
        let with = |ca: &str| TlsConfig::new().with_ca_file(ca);
        let path = with("/ca.pem");
        assert_eq!(
            ca_file_path("x", &path, CaRoots::Os).unwrap(),
            Some("/ca.pem")
        );
        assert_eq!(
            ca_file_path("x", &TlsConfig::new(), CaRoots::Os).unwrap(),
            None
        );
        assert_eq!(ca_file_path("x", &with("$.os"), CaRoots::Os).unwrap(), None);
        assert_eq!(
            ca_file_path("x", &with("$.bundled"), CaRoots::Bundled).unwrap(),
            None
        );
        assert!(ca_file_path("x", &with("$.bundled"), CaRoots::Os).is_err());
        assert!(ca_file_path("x", &with("$.os"), CaRoots::Bundled).is_err());
    }
}
