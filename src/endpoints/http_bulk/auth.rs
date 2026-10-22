//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Credentials `http_bulk` fetches or computes per request: a bearer token from an
//! OAuth2 client credentials grant or a JSON login, or an AWS SigV4 signature.

use super::quoted;
use crate::models::{HttpBulkAuth, HttpBulkLogin, HttpBulkOAuth2};
use crate::support::http_status;
use anyhow::{bail, Context};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::Value;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::warn;

/// A token is replaced this long before it expires, or at half its lifetime if shorter.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Why a request got no response.
#[derive(Debug)]
pub(super) struct SendError {
    pub retryable: bool,
    text: String,
}

impl SendError {
    fn transport(error: reqwest::Error) -> Self {
        Self {
            retryable: true,
            text: error.to_string(),
        }
    }

    fn permanent(text: String) -> Self {
        Self {
            retryable: false,
            text,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

pub(super) enum Auth {
    None,
    Bearer(Box<TokenSource>),
    #[cfg(feature = "aws")]
    SigV4(Box<sigv4::SigV4>),
}

impl Auth {
    pub(super) fn new(config: Option<&HttpBulkAuth>, tls_required: bool) -> anyhow::Result<Self> {
        let Some(auth) = config else {
            return Ok(Self::None);
        };
        match (&auth.oauth2, &auth.aws_sigv4, &auth.login) {
            (None, None, None) => Ok(Self::None),
            (Some(oauth2), None, None) => Ok(Self::Bearer(Box::new(TokenSource::oauth2(
                oauth2,
                tls_required,
            )?))),
            (None, None, Some(login)) => Ok(Self::Bearer(Box::new(TokenSource::login(
                login,
                tls_required,
            )?))),
            #[cfg(feature = "aws")]
            (None, Some(aws), None) => Ok(Self::SigV4(Box::new(sigv4::SigV4::new(aws)?))),
            #[cfg(not(feature = "aws"))]
            (None, Some(_), None) => {
                bail!("http_bulk auth.aws_sigv4 needs a build with the 'aws' feature")
            }
            _ => bail!("http_bulk auth sets more than one of 'oauth2', 'aws_sigv4' and 'login'"),
        }
    }

    /// Sends `request` with the credentials on it. A 401 for a cached token gets a
    /// new token and one more try.
    pub(super) async fn send(
        &self,
        http: &reqwest::Client,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, SendError> {
        match self {
            Self::None => request.send().await.map_err(SendError::transport),
            Self::Bearer(source) => {
                let again = request.try_clone();
                let token = source.token(http).await?;
                let response = request
                    .header(AUTHORIZATION, token.clone())
                    .send()
                    .await
                    .map_err(SendError::transport)?;
                match again {
                    Some(again) if response.status() == StatusCode::UNAUTHORIZED => {
                        source.forget(&token).await;
                        let token = source.token(http).await?;
                        let again = again.header(AUTHORIZATION, token);
                        again.send().await.map_err(SendError::transport)
                    }
                    _ => Ok(response),
                }
            }
            #[cfg(feature = "aws")]
            Self::SigV4(signer) => {
                let mut request = request.build().map_err(SendError::transport)?;
                signer.sign(&mut request).await?;
                http.execute(request).await.map_err(SendError::transport)
            }
        }
    }
}

struct Token {
    header: HeaderValue,
    refresh_at: Option<Instant>,
}

/// Where the token response tells how long the token lives.
enum Expiry {
    /// Seconds from now.
    In(String),
    /// Unix seconds.
    At(String),
}

/// A request that hands out a bearer token: OAuth2 client credentials or a JSON login.
pub(super) struct TokenSource {
    /// Names the request in errors, e.g. `OAuth2 token`.
    label: &'static str,
    url: String,
    content_type: &'static str,
    body: String,
    /// JSON pointer to the token in the response.
    pointer: String,
    expiry: Option<Expiry>,
    token: Mutex<Option<Token>>,
}

/// Checks a token or login URL; `field` names it in errors.
fn check_url(raw: &str, field: &str, tls_required: bool) -> anyhow::Result<()> {
    let url = url::Url::parse(raw).with_context(|| format!("http_bulk {field} is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("http_bulk {field} must be an http(s) URL");
    }
    if url.scheme() == "http" {
        if tls_required {
            bail!("http_bulk tls.required needs an https {field}");
        }
        if !super::is_loopback(&url) {
            warn!(url = %raw, "http_bulk sends credentials unencrypted to {field}; use https");
        }
    }
    Ok(())
}

impl TokenSource {
    fn oauth2(config: &HttpBulkOAuth2, tls_required: bool) -> anyhow::Result<Self> {
        check_url(&config.token_url, "auth.oauth2.token_url", tls_required)?;
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials")
            .append_pair("client_id", &config.client_id)
            .append_pair("client_secret", &config.client_secret);
        if let Some(scope) = &config.scope {
            form.append_pair("scope", scope);
        }
        Ok(Self {
            label: "OAuth2 token",
            url: config.token_url.clone(),
            content_type: "application/x-www-form-urlencoded",
            body: form.finish(),
            pointer: "/access_token".to_string(),
            expiry: Some(Expiry::In("/expires_in".to_string())),
            token: Mutex::new(None),
        })
    }

    fn login(config: &HttpBulkLogin, tls_required: bool) -> anyhow::Result<Self> {
        check_url(&config.url, "auth.login.url", tls_required)?;
        serde_json::from_str::<serde::de::IgnoredAny>(&config.body)
            .context("http_bulk auth.login.body is not JSON")?;
        let pointers = [
            Some(&config.token),
            config.expires_at.as_ref(),
            config.expires_in.as_ref(),
        ];
        if pointers.into_iter().flatten().any(|p| !p.starts_with('/')) {
            bail!("http_bulk auth.login token, expires_at and expires_in are JSON pointers starting with '/'");
        }
        let expiry = match (&config.expires_at, &config.expires_in) {
            (Some(_), Some(_)) => {
                bail!("http_bulk auth.login sets both 'expires_at' and 'expires_in'")
            }
            (Some(at), None) => Some(Expiry::At(at.clone())),
            (None, Some(lifetime)) => Some(Expiry::In(lifetime.clone())),
            (None, None) => None,
        };
        Ok(Self {
            label: "login",
            url: config.url.clone(),
            content_type: "application/json",
            body: config.body.clone(),
            pointer: config.token.clone(),
            expiry,
            token: Mutex::new(None),
        })
    }

    /// The cached token, or a new one. The lock is held while fetching, so
    /// concurrent requests wait for one token request.
    async fn token(&self, http: &reqwest::Client) -> Result<HeaderValue, SendError> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref() {
            if token.refresh_at.is_none_or(|at| Instant::now() < at) {
                return Ok(token.header.clone());
            }
        }
        let token = self.fetch(http).await?;
        let header = token.header.clone();
        *cached = Some(token);
        Ok(header)
    }

    /// Drops the cached token if it is still the one that was refused.
    async fn forget(&self, refused: &HeaderValue) {
        let mut cached = self.token.lock().await;
        if cached.as_ref().is_some_and(|token| token.header == refused) {
            *cached = None;
        }
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<Token, SendError> {
        let label = self.label;
        let failed = |error: reqwest::Error| SendError {
            retryable: true,
            text: format!("the {label} request failed: {error}"),
        };
        let response = http
            .post(&self.url)
            .header(CONTENT_TYPE, self.content_type)
            .body(self.body.clone())
            .send()
            .await
            .map_err(failed)?;
        let status = response.status();
        let text = response.text().await.map_err(failed)?;
        if !status.is_success() {
            return Err(SendError {
                retryable: http_status::is_retryable(status.as_u16()),
                text: format!("the {label} request answered {status}: {}", quoted(&text)),
            });
        }
        let answer: Value = serde_json::from_str(&text).unwrap_or_default();
        let Some(token) = answer.pointer(&self.pointer).and_then(Value::as_str) else {
            return Err(SendError::permanent(format!(
                "the {label} response has no token at '{}'",
                self.pointer
            )));
        };
        let mut header = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            SendError::permanent(format!("the {label} token is not a valid header value"))
        })?;
        header.set_sensitive(true);
        let seconds = |pointer: &str| match answer.pointer(pointer) {
            Some(Value::Number(seconds)) => seconds
                .as_u64()
                .or_else(|| seconds.as_f64().filter(|s| *s >= 0.0).map(|s| s as u64)),
            Some(Value::String(seconds)) => seconds.parse().ok().or_else(|| {
                seconds
                    .parse::<f64>()
                    .ok()
                    .filter(|s| *s >= 0.0)
                    .map(|s| s as u64)
            }),
            _ => None,
        };
        let lifetime = match &self.expiry {
            Some(Expiry::In(pointer)) => seconds(pointer),
            Some(Expiry::At(pointer)) => seconds(pointer).map(|at| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                at.saturating_sub(now.as_secs())
            }),
            None => None,
        };
        let refresh_at = lifetime.map(Duration::from_secs).map(|lifetime| {
            Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN).max(lifetime / 2)
        });
        Ok(Token { header, refresh_at })
    }
}

#[cfg(feature = "aws")]
mod sigv4 {
    use super::SendError;
    use crate::endpoints::aws::load_aws_config;
    use crate::models::{AwsConfig, HttpBulkAwsSigV4};
    use anyhow::bail;
    use aws_sdk_sns::config::{Credentials, ProvideCredentials, SharedCredentialsProvider};
    use aws_sigv4::http_request::{
        sign, PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings,
    };
    use aws_sigv4::sign::v4;
    use reqwest::header::{HeaderName, HeaderValue};
    use std::time::{Duration, SystemTime};
    use tokio::sync::{Mutex, OnceCell};

    pub(in super::super) struct SigV4 {
        config: AwsConfig,
        region: String,
        service: String,
        provider: OnceCell<SharedCredentialsProvider>,
        credentials: Mutex<Option<Credentials>>,
    }

    fn failed(what: &str, error: impl std::fmt::Display) -> SendError {
        SendError::permanent(format!("AWS SigV4 {what}: {error}"))
    }

    impl SigV4 {
        pub(super) fn new(config: &HttpBulkAwsSigV4) -> anyhow::Result<Self> {
            if config.region.is_empty() || config.service.is_empty() {
                bail!("http_bulk auth.aws_sigv4 needs 'region' and 'service'");
            }
            if config.access_key.is_some() != config.secret_key.is_some() {
                bail!(
                    "http_bulk auth.aws_sigv4 needs both 'access_key' and 'secret_key', or neither"
                );
            }
            Ok(Self {
                config: AwsConfig {
                    region: Some(config.region.clone()),
                    access_key: config.access_key.clone(),
                    secret_key: config.secret_key.clone(),
                    session_token: config.session_token.clone(),
                    ..Default::default()
                },
                region: config.region.clone(),
                service: config.service.clone(),
                provider: OnceCell::new(),
                credentials: Mutex::new(None),
            })
        }

        /// Cached until a minute before they expire; static keys never do.
        async fn credentials(&self) -> Result<Credentials, SendError> {
            let mut cached = self.credentials.lock().await;
            let soon = SystemTime::now() + Duration::from_secs(60);
            if let Some(credentials) = cached.as_ref() {
                if credentials.expiry().is_none_or(|expiry| soon < expiry) {
                    return Ok(credentials.clone());
                }
            }
            let provider = self
                .provider
                .get_or_try_init(|| async {
                    let config = load_aws_config(&self.config).await;
                    config.credentials_provider().ok_or("none are configured")
                })
                .await
                .map_err(|error| failed("found no credentials", error))?;
            let credentials = provider
                .provide_credentials()
                .await
                .map_err(|error| SendError {
                    retryable: true,
                    text: format!("AWS SigV4 could not load credentials: {error}"),
                })?;
            *cached = Some(credentials.clone());
            Ok(credentials)
        }

        pub(super) async fn sign(&self, request: &mut reqwest::Request) -> Result<(), SendError> {
            let identity = self.credentials().await?.into();
            let mut settings = SigningSettings::default();
            settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
            let params = v4::SigningParams::builder()
                .identity(&identity)
                .region(&self.region)
                .name(&self.service)
                .time(SystemTime::now())
                .settings(settings)
                .build()
                .map_err(|error| failed("parameters are incomplete", error))?
                .into();
            let headers = request
                .headers()
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
            let body = request.body().and_then(|body| body.as_bytes());
            let signable = SignableRequest::new(
                request.method().as_str(),
                request.url().as_str(),
                headers,
                SignableBody::Bytes(body.unwrap_or_default()),
            )
            .map_err(|error| failed("could not read the request", error))?;
            let (instructions, _) = sign(signable, &params)
                .map_err(|error| failed("could not sign the request", error))?
                .into_parts();
            let mut signed = Vec::new();
            for (name, value) in instructions.headers() {
                let name = HeaderName::from_bytes(name.as_bytes());
                let value = HeaderValue::from_str(value);
                match (name, value) {
                    (Ok(name), Ok(value)) => signed.push((name, value)),
                    _ => return Err(failed("produced an invalid header", "")),
                }
            }
            for (name, value) in signed {
                request.headers_mut().insert(name, value);
            }
            Ok(())
        }
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use crate::endpoints::http_bulk::HttpBulkPublisher;
    use crate::errors::PublisherError;
    use crate::models::HttpBulkConfig;
    use crate::outcomes::SentBatch;
    use crate::plugin::test_support::{StubHttpServer, StubRequest};
    use crate::traits::MessagePublisher;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn server(
        respond: impl Fn(&StubRequest) -> (u16, String) + Send + Sync + 'static,
    ) -> StubHttpServer {
        StubHttpServer::start(respond).await.expect("stub server")
    }

    fn publisher(server: &StubHttpServer, auth: Value) -> HttpBulkPublisher {
        let config = json!({"url": server.url(), "upsert": {"path": "/docs"}, "auth": auth});
        let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
        HttpBulkPublisher::new(&config).expect("publisher")
    }

    fn oauth2(server: &StubHttpServer) -> Value {
        json!({"oauth2": {
            "token_url": format!("{}/token", server.url()),
            "client_id": "bridge",
            "client_secret": "s3 cr&t",
            "scope": "write"
        }})
    }

    /// Hands out `token-1`, `token-2`, … and accepts only the tokens in `accepted`.
    fn issuer(
        expires_in: u64,
        accepted: &'static [&'static str],
    ) -> impl Fn(&StubRequest) -> (u16, String) + Send + Sync + 'static {
        let issued = Arc::new(AtomicUsize::new(0));
        move |request| {
            if request.target == "/token" {
                let number = issued.fetch_add(1, Ordering::SeqCst) + 1;
                let token =
                    json!({"access_token": format!("token-{number}"), "expires_in": expires_in});
                return (200, token.to_string());
            }
            let bearer = request.header("authorization").unwrap_or_default();
            match accepted
                .iter()
                .any(|token| bearer == format!("Bearer {token}"))
            {
                true => (200, "{}".to_string()),
                false => (401, "token expired".to_string()),
            }
        }
    }

    /// Sends one document and answers why it failed.
    async fn refused(publisher: &HttpBulkPublisher) -> PublisherError {
        let sent = publisher.send_batch(vec![r#"{"id":1}"#.into()]).await;
        match sent.unwrap() {
            SentBatch::Partial { mut failed, .. } => failed.remove(0).1,
            SentBatch::Ack => panic!("the document was accepted"),
        }
    }

    fn targets(server: &StubHttpServer) -> Vec<(String, String)> {
        let sent = server.requests().into_iter();
        sent.map(|r| {
            let bearer = r.header("authorization").unwrap_or_default().to_string();
            (r.target, bearer)
        })
        .collect()
    }

    #[tokio::test]
    async fn a_token_is_fetched_once_and_reused() {
        let server = server(issuer(3600, &["token-1"])).await;
        let publisher = publisher(&server, oauth2(&server));
        publisher
            .send_batch(vec![r#"{"id":1}"#.into()])
            .await
            .unwrap();
        publisher
            .send_batch(vec![r#"{"id":2}"#.into()])
            .await
            .unwrap();

        let token = &server.requests()[0];
        assert_eq!(
            String::from_utf8_lossy(&token.body),
            "grant_type=client_credentials&client_id=bridge&client_secret=s3+cr%26t&scope=write"
        );
        assert_eq!(
            targets(&server),
            vec![
                ("/token".to_string(), String::new()),
                ("/docs".to_string(), "Bearer token-1".to_string()),
                ("/docs".to_string(), "Bearer token-1".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_token_is_refreshed_once_and_the_request_sent_again() {
        let server = server(issuer(3600, &["token-2"])).await;
        let publisher = publisher(&server, oauth2(&server));
        publisher
            .send_batch(vec![r#"{"id":1}"#.into()])
            .await
            .unwrap();

        assert_eq!(
            targets(&server),
            vec![
                ("/token".to_string(), String::new()),
                ("/docs".to_string(), "Bearer token-1".to_string()),
                ("/token".to_string(), String::new()),
                ("/docs".to_string(), "Bearer token-2".to_string()),
            ]
        );
        assert_eq!(server.requests()[3].body, server.requests()[1].body);
    }

    #[tokio::test]
    async fn a_token_that_stays_refused_fails_after_one_refresh() {
        let server = server(issuer(3600, &[])).await;
        let publisher = publisher(&server, oauth2(&server));
        let error = refused(&publisher).await;
        assert!(error.to_string().contains("401"), "{error}");
        assert_eq!(server.requests().len(), 4);
    }

    #[tokio::test]
    async fn an_expired_token_is_replaced_before_the_request() {
        // A lifetime of zero is due for refresh at once.
        let server = server(issuer(0, &["token-1", "token-2"])).await;
        let publisher = publisher(&server, oauth2(&server));
        publisher
            .send_batch(vec![r#"{"id":1}"#.into()])
            .await
            .unwrap();
        publisher
            .send_batch(vec![r#"{"id":2}"#.into()])
            .await
            .unwrap();

        let bearers: Vec<String> = targets(&server).into_iter().map(|(_, b)| b).collect();
        assert_eq!(bearers, ["", "Bearer token-1", "", "Bearer token-2"]);
    }

    #[tokio::test]
    async fn concurrent_batches_share_one_token_request() {
        let server = server(issuer(3600, &["token-1"])).await;
        let publisher = Arc::new(publisher(&server, oauth2(&server)));
        let batches = (0..8).map(|id| {
            let publisher = publisher.clone();
            tokio::spawn(async move {
                let document = format!(r#"{{"id":{id}}}"#);
                publisher.send_batch(vec![document.as_str().into()]).await
            })
        });
        for batch in futures::future::join_all(batches).await {
            batch.unwrap().unwrap();
        }

        let sent = targets(&server);
        let tokens = sent.iter().filter(|(target, _)| target == "/token").count();
        assert_eq!(tokens, 1);
        assert_eq!(sent.len(), 9);
    }

    #[tokio::test]
    async fn a_failing_token_endpoint_fails_the_batch_by_its_status() {
        for (status, retryable) in [(503, true), (400, false)] {
            let server = server(move |_| (status, "no".to_string())).await;
            let publisher = publisher(&server, oauth2(&server));
            let error = refused(&publisher).await;
            let retried = matches!(error, PublisherError::Retryable(_));
            assert_eq!(retried, retryable, "{error}");
            assert!(error.to_string().contains("OAuth2 token"), "{error}");
        }
    }

    #[test]
    fn both_schemes_at_once_are_refused() {
        let config = json!({
            "url": "http://localhost:1", "upsert": {"path": "/docs"},
            "auth": {
                "oauth2": {"token_url": "http://localhost:1/t", "client_id": "a", "client_secret": "b"},
                "aws_sigv4": {"region": "eu-central-1", "service": "es"}
            }
        });
        let config: HttpBulkConfig = serde_json::from_value(config).unwrap();
        let error = HttpBulkPublisher::new(&config).map(|_| ()).unwrap_err();
        assert!(error.to_string().contains("more than one"), "{error}");
    }

    /// A login answering like Iggy: the token and its absolute expiry, nested.
    fn login_issuer(lifetime: u64) -> impl Fn(&StubRequest) -> (u16, String) + Send + Sync {
        let issued = Arc::new(AtomicUsize::new(0));
        move |request| {
            if request.target != "/users/login" {
                return (200, "{}".to_string());
            }
            let number = issued.fetch_add(1, Ordering::SeqCst) + 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let answer = json!({"user_id": 1, "access_token": {
                "token": format!("token-{number}"), "expiry": now + lifetime
            }});
            (200, answer.to_string())
        }
    }

    fn login(server: &StubHttpServer) -> Value {
        json!({"login": {
            "url": format!("{}/users/login", server.url()),
            "body": r#"{"username":"bridge","password":"pw"}"#,
            "token": "/access_token/token",
            "expires_at": "/access_token/expiry"
        }})
    }

    #[tokio::test]
    async fn a_login_token_is_read_by_pointer_and_replaced_at_its_expiry() {
        for (lifetime, logins) in [(3600, 1), (0, 2)] {
            let server = server(login_issuer(lifetime)).await;
            let publisher = publisher(&server, login(&server));
            for id in 1..=2 {
                let document = format!(r#"{{"id":{id}}}"#);
                publisher
                    .send_batch(vec![document.as_str().into()])
                    .await
                    .unwrap();
            }

            let requests = server.requests();
            assert_eq!(requests[0].header("content-type"), Some("application/json"));
            assert_eq!(
                requests[0].body,
                br#"{"username":"bridge","password":"pw"}"#
            );
            let sent = targets(&server);
            let count = sent.iter().filter(|(t, _)| t == "/users/login").count();
            assert_eq!(count, logins, "lifetime {lifetime}: {sent:?}");
            assert_eq!(sent[1].1, "Bearer token-1");
        }
    }

    #[test]
    fn a_bad_login_configuration_is_refused() {
        let login = |extra: Value| {
            let mut login = json!({"url": "http://localhost:1/login", "body": "{}", "token": "/t"});
            login
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            login
        };
        for (auth, reason) in [
            (
                json!({"login": login(json!({"body": "user=a"}))}),
                "not JSON",
            ),
            (
                json!({"login": login(json!({"token": "t"}))}),
                "JSON pointers",
            ),
            (
                json!({"login": login(json!({"expires_at": "/a", "expires_in": "/b"}))}),
                "both",
            ),
            (
                json!({"login": login(json!({})), "oauth2": {
                    "token_url": "http://localhost:1/t", "client_id": "a", "client_secret": "b"
                }}),
                "more than one",
            ),
        ] {
            let config =
                json!({"url": "http://localhost:1", "upsert": {"path": "/d"}, "auth": auth});
            let config: HttpBulkConfig = serde_json::from_value(config).unwrap();
            let error = HttpBulkPublisher::new(&config).map(|_| ()).unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{error:#}");
        }
    }

    #[cfg(feature = "aws")]
    #[tokio::test]
    async fn sigv4_signs_the_path_exactly_as_it_is_sent() {
        use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
        let server = server(|_| (200, "{}".to_string())).await;
        let auth = json!({"aws_sigv4": {
            "region": "us-east-1", "service": "bedrock",
            "access_key": "AKIDEXAMPLE", "secret_key": "secret"
        }});
        let config =
            json!({"url": server.url(), "upsert": {"path": "/model/a:b%2Fc/invoke"}, "auth": auth});
        let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
        let publisher = HttpBulkPublisher::new(&config).expect("publisher");
        publisher
            .send_batch(vec![r#"{"id":1}"#.into()])
            .await
            .unwrap();

        // Sign again what the server received, as AWS does, and compare.
        let request = &server.requests()[0];
        assert_eq!(request.target, "/model/a:b%2Fc/invoke");
        let authorization = request.header("authorization").expect("signed");
        let names = authorization.split("SignedHeaders=").nth(1).unwrap();
        let names: Vec<&str> = names.split(',').next().unwrap().split(';').collect();
        let headers = request
            .headers
            .iter()
            .filter(|(name, _)| names.contains(&name.as_str()) && !name.starts_with("x-amz-"));
        let date = request.header("x-amz-date").unwrap();
        let time = chrono::NaiveDateTime::parse_from_str(date, "%Y%m%dT%H%M%SZ").unwrap();
        let identity =
            aws_sdk_sns::config::Credentials::new("AKIDEXAMPLE", "secret", None, None, "t");
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
        let identity = identity.into();
        let params = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region("us-east-1")
            .name("bedrock")
            .time(time.and_utc().into())
            .settings(settings)
            .build()
            .unwrap()
            .into();
        let url = format!("{}{}", server.url(), request.target);
        let headers = headers.map(|(name, value)| (name.as_str(), value.as_str()));
        let signable =
            SignableRequest::new("POST", &url, headers, SignableBody::Bytes(&request.body))
                .unwrap();
        let (expected, _) = sign(signable, &params).unwrap().into_parts();
        let expected = expected
            .headers()
            .find(|(name, _)| *name == "authorization");
        assert_eq!(expected.map(|(_, value)| value), Some(authorization));
    }

    #[cfg(feature = "aws")]
    #[tokio::test]
    async fn sigv4_signs_every_request_with_the_static_keys() {
        let server = server(|_| (200, "{}".to_string())).await;
        let publisher = publisher(
            &server,
            json!({"aws_sigv4": {
                "region": "eu-central-1", "service": "es",
                "access_key": "AKIDEXAMPLE", "secret_key": "secret", "session_token": "session"
            }}),
        );
        publisher
            .send_batch(vec![r#"{"id":1}"#.into()])
            .await
            .unwrap();

        let request = &server.requests()[0];
        let authorization = request.header("authorization").expect("signed");
        let scope = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/";
        assert!(authorization.starts_with(scope), "{authorization}");
        assert!(
            authorization.contains("/eu-central-1/es/aws4_request"),
            "{authorization}"
        );
        assert!(
            authorization.contains("x-amz-content-sha256"),
            "{authorization}"
        );
        assert_eq!(request.header("x-amz-security-token"), Some("session"));
        assert!(request.header("x-amz-date").is_some());
        // sha256 of the body that was sent
        assert_eq!(
            request.header("x-amz-content-sha256").map(str::len),
            Some(64)
        );
    }
}
