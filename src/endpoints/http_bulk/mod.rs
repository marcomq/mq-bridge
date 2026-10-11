//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! HTTP APIs that take many JSON documents in one request; the input is in `consumer`.
//!
//! Search engines and similar stores differ in three things only: the body an
//! upsert carries, how documents are deleted, and how the outcome is reported.
//! All three are configuration here, so a new target is a recipe, not code.
//! Messages are sent in order: a batch is cut into runs of upserts and deletes,
//! and after a request that may be retried nothing later in the batch is sent.

mod auth;
mod consumer;
mod presets;
mod publisher;
mod query;
pub(crate) use consumer::cursor_checkpoint;
pub use consumer::HttpBulkConsumer;
pub use presets::{
    base_url as preset_base_url, preset_names, register_preset, register_preset_with,
    schema as preset_schema, segment as preset_segment,
};
pub use publisher::HttpBulkPublisher;

use crate::models::HttpBulkConfig;
use anyhow::{bail, Context};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;
use tracing::warn;

const JSON: &str = "application/json";
/// Most characters of a response body quoted in an error.
const QUOTED_RESPONSE_CHARS: usize = 500;

/// The client, base URL, headers and credentials both directions of the endpoint share.
struct Connection {
    http: reqwest::Client,
    base: String,
    headers: HeaderMap,
    auth: auth::Auth,
}

impl Connection {
    fn new(config: &HttpBulkConfig) -> anyhow::Result<Self> {
        let url = url::Url::parse(&config.url).context("Invalid http_bulk URL")?;
        if !url.has_host() || !matches!(url.scheme(), "http" | "https") {
            bail!("http_bulk URL must be an absolute http(s) URL, e.g. 'http://localhost:7700'");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("http_bulk URL must not carry a query or fragment; put it into the request path");
        }
        if config.tls.required && url.scheme() != "https" {
            bail!("http_bulk tls.required needs an https URL");
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &config.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("http_bulk header name '{name}' is not valid"))?,
                HeaderValue::from_str(value)
                    .with_context(|| format!("http_bulk header '{name}' has an invalid value"))?,
            );
        }
        // No redirect following: reqwest would resend custom credential headers to another host.
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(
                config.connect_timeout_ms.unwrap_or(10_000),
            ));
        if let Some(ms) = config.request_timeout_ms {
            let timeout = Duration::from_millis(ms);
            // An open stream has no total duration: the timeout bounds each silence.
            builder = match config.read.as_ref().and_then(|read| read.stream) {
                Some(_) => builder.read_timeout(timeout),
                None => builder.timeout(timeout),
            };
        }
        crate::support::tls_check::warn_unverified("http_bulk", config.tls.accept_invalid_certs);
        if config.tls.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(ca) = &config.tls.ca_file {
            let pem = std::fs::read(ca)
                .with_context(|| format!("Failed to read http_bulk CA file '{ca}'"))?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&pem)
                    .with_context(|| format!("Invalid http_bulk CA certificate '{ca}'"))?,
            );
        }
        if config.tls.cert_password.is_some() {
            bail!("http_bulk tls.cert_password is not supported; use an unencrypted key");
        }
        match (&config.tls.cert_file, &config.tls.key_file) {
            (Some(cert), key) => {
                let mut pem = std::fs::read(cert)
                    .with_context(|| format!("Failed to read http_bulk cert file '{cert}'"))?;
                if let Some(key) = key {
                    pem.push(b'\n');
                    pem.extend(
                        std::fs::read(key).with_context(|| {
                            format!("Failed to read http_bulk key file '{key}'")
                        })?,
                    );
                }
                builder =
                    builder.identity(reqwest::Identity::from_pem(&pem).with_context(|| {
                        format!("Invalid http_bulk client certificate '{cert}'")
                    })?);
            }
            (None, Some(_)) => bail!("http_bulk tls.key_file needs tls.cert_file"),
            (None, None) => {}
        }
        let auth = auth::Auth::new(config.auth.as_ref(), config.tls.required)?;
        if sends_credentials_in_clear(&url, &headers, &auth) {
            warn!(
                host = url.host_str(),
                "http_bulk sends its headers and credentials unencrypted; use an https URL"
            );
        }
        Ok(Self {
            http: builder
                .build()
                .context("Failed to build http_bulk client")?,
            base: url.as_str().trim_end_matches('/').to_string(),
            headers,
            auth,
        })
    }

    /// A request to `url` with the configured headers.
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.http.request(method, url).headers(self.headers.clone())
    }

    /// Sends `request` authenticated; every request of the endpoint goes through here.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, auth::SendError> {
        self.auth.send(&self.http, request).await
    }
}

/// Whether the URL names this machine, where clear-text credentials never leave it.
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn sends_credentials_in_clear(url: &url::Url, headers: &HeaderMap, auth: &auth::Auth) -> bool {
    url.scheme() == "http"
        && !is_loopback(url)
        && (!headers.is_empty() || !matches!(auth, auth::Auth::None))
}

fn quoted(text: &str) -> String {
    match text.char_indices().nth(QUOTED_RESPONSE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod clear_text_tests {
    use super::*;

    #[test]
    fn only_remote_http_with_headers_or_auth_sends_credentials_in_clear() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        let clear = |url: &str, headers: &HeaderMap| {
            sends_credentials_in_clear(&url::Url::parse(url).unwrap(), headers, &auth::Auth::None)
        };
        assert!(clear("http://search.example:7700", &headers));
        assert!(!clear("http://search.example:7700", &HeaderMap::new()));
        assert!(!clear("https://search.example:7700", &headers));
        assert!(!clear("http://localhost:7700", &headers));
        assert!(!clear("http://127.0.0.1:7700", &headers));
        assert!(!clear("http://[::1]:7700", &headers));
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn the_recipes_in_the_book_are_valid_configurations() {
        let book = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/book/connectors/");
        for (name, recipes) in [
            ("http-bulk.md", 10),
            ("typesense.md", 2),
            ("elasticsearch.md", 2),
            ("postgrest.md", 2),
        ] {
            // The book is not part of the published crate.
            let Ok(page) = std::fs::read_to_string(format!("{book}{name}")) else {
                return;
            };
            // A Windows checkout may hold the page with CRLF line ends.
            let page = page.replace("\r\n", "\n");
            let blocks = page.split("```yaml\n").skip(1);
            let mut found = 0;
            for recipe in blocks.filter_map(|rest| rest.split("```").next()) {
                let route: Value = serde_yaml_ng::from_str(recipe).expect("yaml");
                // Either an `input:` or `output:` fragment or a whole named route.
                let route = match route.get("output").or(route.get("input")) {
                    Some(_) => &route,
                    None => route.as_object().unwrap().values().next().unwrap(),
                };
                found += 1;
                // An `input:` fragment is a read recipe; its consumer is built without a server.
                if let Some(input) = route.get("input").and_then(|i| i.get("http_bulk")) {
                    let mut config: HttpBulkConfig = serde_json::from_value(input.clone())
                        .unwrap_or_else(|error| panic!("{name}: {error}"));
                    if let Some(read) = &mut config.read {
                        read.checkpoint_store = None;
                    }
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(HttpBulkConsumer::new(&config, true))
                        .unwrap_or_else(|error| panic!("{name}: {error}"));
                    continue;
                }
                let output = &route["output"];
                let config = match output.get("custom") {
                    Some(custom) => {
                        presets::resolve(custom["name"].as_str().unwrap(), &custom["config"])
                    }
                    None => serde_json::from_value(output["http_bulk"].clone()).map_err(Into::into),
                };
                let config: HttpBulkConfig =
                    config.unwrap_or_else(|error| panic!("{name}: {error:#}"));
                HttpBulkPublisher::new(&config).unwrap_or_else(|error| panic!("{name}: {error}"));
            }
            assert_eq!(found, recipes, "{name}");
        }
    }
}
