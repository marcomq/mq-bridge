//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Collecting the secrets out of a configuration so they can be redacted or
//! resolved: the `SecretExtractor` trait, its impls, and the shared helpers.

use super::*;

/// Trait for extracting secrets from configuration structures.
pub trait SecretExtractor {
    /// Extracts secrets into the provided map using the given prefix, and clears them from self.
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>);
}

fn extract_sensitive_string_map_entries(
    values: &mut HashMap<String, String>,
    prefix: &str,
    field_name: &str,
    secrets: &mut HashMap<String, String>,
) {
    let secret_keys = values
        .keys()
        .filter(|key| is_sensitive_map_key(key))
        .cloned()
        .collect::<Vec<_>>();

    for key in secret_keys {
        if let Some(value) = values.remove(&key) {
            secrets.insert(
                sanitize_secret_key(&format!(
                    "{}__{}__{}",
                    prefix,
                    field_name,
                    encode_secret_map_key(&key)
                )),
                value,
            );
        }
    }
}

/// `key` and `auth` count only as whole words, so `X-Author` and `X-Keyboard` stay in place.
pub(super) fn is_sensitive_map_key(key: &str) -> bool {
    const CONTAINS: [&str; 4] = ["token", "secret", "password", "cookie"];
    const WORDS: [&str; 5] = ["key", "apikey", "auth", "authorization", "authentication"];
    let key = key.to_ascii_lowercase();
    CONTAINS.iter().any(|needle| key.contains(needle))
        || key
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| WORDS.contains(&word))
}

/// Fails when two of `names` map to one secret env key (`a-b` and `a_b`), where one secret
/// would overwrite the other. `kind` names what is checked, e.g. `route`.
pub fn check_secret_key_names<'a>(
    kind: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<()> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for name in names {
        if let Some(other) = seen.insert(sanitize_secret_key(name), name) {
            if other != name {
                anyhow::bail!(
                    "{kind} names '{other}' and '{name}' map to the same secret key. \
                     Rename one of them."
                );
            }
        }
    }
    Ok(())
}

/// Checks the route names of `config` and the `switch` cases inside every route with
/// [`check_secret_key_names`]. Call it before [`extract_config_secrets`].
pub fn check_config_secret_keys(config: &Config) -> anyhow::Result<()> {
    check_secret_key_names("route", config.keys().map(String::as_str))?;
    for route in config.values() {
        route.check_secret_keys()?;
    }
    Ok(())
}

impl Route {
    /// Checks the `switch` cases of this route with [`check_secret_key_names`].
    pub fn check_secret_keys(&self) -> anyhow::Result<()> {
        self.input.check_secret_keys()?;
        self.output.check_secret_keys()
    }
}

impl Endpoint {
    /// Checks the `switch` cases of this endpoint tree with [`check_secret_key_names`].
    pub fn check_secret_keys(&self) -> anyhow::Result<()> {
        for middleware in &self.middlewares {
            match middleware {
                Middleware::Dlq(cfg) => cfg.endpoint.check_secret_keys()?,
                Middleware::Lookup(cfg) => {
                    for from in cfg.from.iter().chain(cfg.entries.iter().map(|e| &e.from)) {
                        from.check_secret_keys()?;
                    }
                }
                _ => {}
            }
        }
        match &self.endpoint_type {
            EndpointType::Fanout(endpoints) => {
                endpoints.iter().try_for_each(Endpoint::check_secret_keys)
            }
            EndpointType::Switch(cfg) => {
                check_secret_key_names("switch case", cfg.cases.keys().map(String::as_str))?;
                cfg.cases
                    .values()
                    .chain(cfg.default.as_deref())
                    .try_for_each(Endpoint::check_secret_keys)
            }
            EndpointType::Sequence(cfg) => cfg
                .endpoints
                .iter()
                .try_for_each(Endpoint::check_secret_keys),
            EndpointType::Reader(endpoint) => endpoint.check_secret_keys(),
            EndpointType::Request(cfg) => {
                cfg.to.check_secret_keys()?;
                cfg.forward_to.check_secret_keys()
            }
            _ => Ok(()),
        }
    }
}

fn extract_binary_map_entries(
    values: &mut HashMap<String, Vec<u8>>,
    prefix: &str,
    field_name: &str,
    secrets: &mut HashMap<String, String>,
) {
    for (key, value) in std::mem::take(values) {
        secrets.insert(
            sanitize_secret_key(&format!(
                "{}__{}__{}",
                prefix,
                field_name,
                encode_secret_map_key(&key)
            )),
            serde_json::to_string(&value).expect("serializing bytes cannot fail"),
        );
    }
}

fn url_has_userinfo(url: &str) -> bool {
    let Some(authority_start) = url.find("://").map(|idx| idx + 3) else {
        return false;
    };
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|idx| authority_start + idx)
        .unwrap_or(url.len());
    url[authority_start..authority_end].contains('@')
}

fn sanitize_secret_key(key: &str) -> String {
    key.chars()
        .map(|ch| {
            let ch = ch.to_ascii_uppercase();
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Reverses [`encode_secret_map_key`]. A map key that round-tripped through
/// `extract_secrets` and back in from the environment arrives hex-encoded, so a
/// consumer has to decode it before using it as the original name.
#[cfg(feature = "grpc")]
pub(crate) fn decode_secret_map_key(key: &str) -> Option<String> {
    if key.is_empty() || key.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = key
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect();
    String::from_utf8(bytes?).ok()
}

fn encode_secret_map_key(key: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(key.len() * 2);
    for byte in key.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn extract_sensitive_url(
    url: &mut String,
    prefix: &str,
    field_name: &str,
    secrets: &mut HashMap<String, String>,
) {
    if !url.is_empty() && url_has_userinfo(url) {
        secrets.insert(
            sanitize_secret_key(&format!("{}__{}", prefix, field_name)),
            std::mem::take(url),
        );
    }
}

fn extract_sensitive_optional_url(
    url: &mut Option<String>,
    prefix: &str,
    field_name: &str,
    secrets: &mut HashMap<String, String>,
) {
    if url.as_ref().is_some_and(|url| url_has_userinfo(url)) {
        if let Some(url) = url.take() {
            secrets.insert(
                sanitize_secret_key(&format!("{}__{}", prefix, field_name)),
                url,
            );
        }
    }
}

/// Narrower than the header heuristic: `key` and `max_tokens` are ordinary connector fields.
fn is_sensitive_custom_field(key: &str) -> bool {
    const CONTAINS: [&str; 4] = ["password", "passphrase", "secret", "credentials"];
    const SUFFIXES: [&str; 7] = [
        "token",
        "apikey",
        "api_key",
        "access_key",
        "account_key",
        "private_key",
        "connection_string",
    ];
    CONTAINS.iter().any(|needle| key.contains(needle))
        || SUFFIXES.iter().any(|suffix| key.ends_with(suffix))
}

/// Whether an env-variable segment maps back onto exactly this key.
fn is_env_restorable_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('_')
        && !key.ends_with('_')
        && !key.contains("__")
        && key.parse::<usize>().is_err()
        && key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_credential_url(value: &str) -> bool {
    !value.contains(char::is_whitespace) && url_has_userinfo(value)
}

/// An env override of an untyped field comes back as a bool or number if it parses as one.
fn env_keeps_string(value: &str) -> bool {
    value.to_ascii_lowercase().parse::<bool>().is_err() && value.parse::<f64>().is_err()
}

fn is_custom_secret(key: &str, value: &str) -> bool {
    // A `${…}` value is a reference, not the secret itself.
    !value.is_empty()
        && !value.starts_with("${")
        && env_keeps_string(value)
        && (is_sensitive_custom_field(key)
            || is_credential_url(value)
            || (key == "dsn" && value.contains('@')))
}

/// Extracts credentials from a custom endpoint's or middleware's free-form config.
/// Keys an environment variable cannot name are left in place, with their subtree.
fn extract_custom_config_secrets(
    value: &mut serde_json::Value,
    prefix: &str,
    secrets: &mut HashMap<String, String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map
                .keys()
                .filter(|key| is_env_restorable_key(key))
                .cloned()
                .collect();
            for key in keys {
                let path = format!("{}__{}", prefix, key.to_ascii_uppercase());
                let is_secret = matches!(
                    map.get(&key),
                    Some(serde_json::Value::String(value)) if is_custom_secret(&key, value)
                );
                if is_secret {
                    if let Some(serde_json::Value::String(secret)) = map.remove(&key) {
                        secrets.insert(path, secret);
                    }
                } else if let Some(child) = map.get_mut(&key) {
                    extract_custom_config_secrets(child, &path, secrets);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let path = format!("{}__{}", prefix, i);
                match item {
                    serde_json::Value::String(url) if is_credential_url(url) => {
                        secrets.insert(path, std::mem::take(url));
                    }
                    _ => extract_custom_config_secrets(item, &path, secrets),
                }
            }
        }
        _ => {}
    }
}

impl SecretExtractor for Route {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        self.input
            .extract_secrets(&format!("{}__{}", prefix, "INPUT"), secrets);
        self.output
            .extract_secrets(&format!("{}__{}", prefix, "OUTPUT"), secrets);
    }
}

impl SecretExtractor for Endpoint {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        for (i, middleware) in self.middlewares.iter_mut().enumerate() {
            middleware.extract_secrets(&format!("{}__{}__{}", prefix, "MIDDLEWARES", i), secrets);
        }
        self.endpoint_type.extract_secrets(prefix, secrets);
    }
}

impl SecretExtractor for EndpointType {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        match self {
            EndpointType::Aws(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "AWS"), secrets)
            }
            EndpointType::Kafka(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "KAFKA"), secrets)
            }
            EndpointType::Nats(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "NATS"), secrets)
            }
            EndpointType::Amqp(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "AMQP"), secrets)
            }
            EndpointType::MongoDb(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "MONGODB"), secrets)
            }
            EndpointType::Mqtt(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "MQTT"), secrets)
            }
            EndpointType::Http(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "HTTP"), secrets)
            }
            EndpointType::WebSocket(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "WEBSOCKET"), secrets)
            }
            EndpointType::IbmMq(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "IBMMQ"), secrets)
            }
            EndpointType::ZeroMq(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "ZEROMQ"), secrets)
            }
            EndpointType::RedisStreams(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "REDIS_STREAMS"), secrets)
            }
            EndpointType::Sqlx(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "SQLX"), secrets)
            }
            EndpointType::ClickHouse(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "CLICKHOUSE"), secrets)
            }
            EndpointType::HttpBulk(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "HTTP_BULK"), secrets)
            }
            EndpointType::PostgresCdc(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "POSTGRES_CDC"), secrets)
            }
            EndpointType::Grpc(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "GRPC"), secrets)
            }
            EndpointType::Fanout(endpoints) => {
                for (i, ep) in endpoints.iter_mut().enumerate() {
                    ep.extract_secrets(&format!("{}__{}__{}", prefix, "FANOUT", i), secrets);
                }
            }
            EndpointType::Switch(cfg) => {
                for (key, ep) in cfg.cases.iter_mut() {
                    ep.extract_secrets(
                        &format!(
                            "{}__{}__{}",
                            prefix,
                            "SWITCH__CASES",
                            sanitize_secret_key(key)
                        ),
                        secrets,
                    );
                }
                if let Some(default) = &mut cfg.default {
                    default.extract_secrets(&format!("{}__{}", prefix, "SWITCH__DEFAULT"), secrets);
                }
            }
            EndpointType::Sequence(cfg) => {
                let prefix = format!("{}__{}", prefix, "SEQUENCE");
                for (i, ep) in cfg.endpoints.iter_mut().enumerate() {
                    ep.extract_secrets(&format!("{}__{}__{}", prefix, "ENDPOINTS", i), secrets);
                }
                extract_sensitive_optional_url(
                    &mut cfg.checkpoint_store,
                    &prefix,
                    "CHECKPOINT_STORE",
                    secrets,
                );
            }
            EndpointType::Reader(ep) => {
                ep.extract_secrets(&format!("{}__{}", prefix, "READER"), secrets)
            }
            EndpointType::Request(cfg) => {
                cfg.to
                    .extract_secrets(&format!("{}__{}", prefix, "REQUEST__TO"), secrets);
                cfg.forward_to
                    .extract_secrets(&format!("{}__{}", prefix, "REQUEST__FORWARD_TO"), secrets);
            }
            EndpointType::File(cfg) => {
                if let Some(enc) = &mut cfg.encryption {
                    enc.extract_secrets(&format!("{}__{}", prefix, "FILE__ENCRYPTION"), secrets);
                }
            }
            EndpointType::ObjectStore(cfg) => {
                if let Some(enc) = &mut cfg.encryption {
                    enc.extract_secrets(
                        &format!("{}__{}", prefix, "OBJECT_STORE__ENCRYPTION"),
                        secrets,
                    );
                }
            }
            EndpointType::Custom { config, .. } => extract_custom_config_secrets(
                config,
                &format!("{}__{}", prefix, "CUSTOM__CONFIG"),
                secrets,
            ),
            _ => {}
        }
    }
}

impl SecretExtractor for Middleware {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        match self {
            Middleware::Dlq(cfg) => {
                cfg.endpoint
                    .extract_secrets(&format!("{}__{}__{}", prefix, "DLQ", "ENDPOINT"), secrets);
            }
            Middleware::Encryption(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "ENCRYPTION"), secrets);
            }
            Middleware::Lookup(cfg) => {
                let prefix = format!("{}__{}", prefix, "LOOKUP");
                if let Some(from) = &mut cfg.from {
                    from.extract_secrets(&format!("{}__{}", prefix, "FROM"), secrets);
                }
                for (i, entry) in cfg.entries.iter_mut().enumerate() {
                    entry
                        .from
                        .extract_secrets(&format!("{}__ENTRIES__{}__FROM", prefix, i), secrets);
                }
            }
            Middleware::Deduplication(cfg) => extract_sensitive_optional_url(
                &mut cfg.store,
                &format!("{}__{}", prefix, "DEDUPLICATION"),
                "STORE",
                secrets,
            ),
            Middleware::Aggregate(cfg) => extract_sensitive_optional_url(
                &mut cfg.store,
                &format!("{}__{}", prefix, "AGGREGATE"),
                "STORE",
                secrets,
            ),
            Middleware::Custom { config, .. } => extract_custom_config_secrets(
                config,
                &format!("{}__{}", prefix, "CUSTOM__CONFIG"),
                secrets,
            ),
            _ => {}
        }
    }
}

impl SecretExtractor for EncryptionConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if !self.key.is_empty() {
            secrets.insert(
                sanitize_secret_key(&format!("{}__{}", prefix, "KEY")),
                std::mem::take(&mut self.key),
            );
        }
        for (id, k) in std::mem::take(&mut self.decrypt_keys) {
            secrets.insert(
                sanitize_secret_key(&format!(
                    "{}__{}__{}",
                    prefix,
                    "DECRYPT_KEYS",
                    encode_secret_map_key(&id)
                )),
                k,
            );
        }
    }
}

impl SecretExtractor for AwsConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if let Some(val) = self.access_key.take() {
            secrets.insert(format!("{}__{}", prefix, "ACCESS_KEY"), val);
        }
        if let Some(val) = self.secret_key.take() {
            secrets.insert(format!("{}__{}", prefix, "SECRET_KEY"), val);
        }
        if let Some(val) = self.session_token.take() {
            secrets.insert(format!("{}__{}", prefix, "SESSION_TOKEN"), val);
        }
        extract_sensitive_optional_url(&mut self.queue_url, prefix, "QUEUE_URL", secrets);
        extract_sensitive_optional_url(&mut self.endpoint_url, prefix, "ENDPOINT_URL", secrets);
    }
}

impl SecretExtractor for KafkaConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for NatsConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        if let Some(val) = self.token.take() {
            secrets.insert(format!("{}__{}", prefix, "TOKEN"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for AmqpConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for MongoDbConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        // The checkpoint store URL may embed connection credentials.
        extract_sensitive_optional_url(
            &mut self.checkpoint_store,
            prefix,
            "CHECKPOINT_STORE",
            secrets,
        );
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for MqttConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for HttpConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some((u, p)) = self.basic_auth.take() {
            secrets.insert(format!("{}__{}__{}", prefix, "BASIC_AUTH", 0), u);
            secrets.insert(format!("{}__{}__{}", prefix, "BASIC_AUTH", 1), p);
        }
        extract_sensitive_string_map_entries(
            &mut self.custom_headers,
            prefix,
            "CUSTOM_HEADERS",
            secrets,
        );
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
        if let Some(endpoint) = &mut self.stream_response_to {
            endpoint.extract_secrets(&format!("{}__{}", prefix, "STREAM_RESPONSE_TO"), secrets);
        }
    }
}

impl SecretExtractor for WebSocketConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for IbmMqConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for ZeroMqConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
    }
}

impl SecretExtractor for RedisStreamsConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for SqlxConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for ClickHouseConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.username.take() {
            secrets.insert(format!("{}__{}", prefix, "USERNAME"), val);
        }
        if let Some(val) = self.password.take() {
            secrets.insert(format!("{}__{}", prefix, "PASSWORD"), val);
        }
        if let Some(val) = self.checkpoint_store.take() {
            secrets.insert(format!("{}__{}", prefix, "CHECKPOINT_STORE"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for HttpBulkConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        extract_sensitive_string_map_entries(&mut self.headers, prefix, "HEADERS", secrets);
        if let Some(oauth2) = self.auth.as_mut().and_then(|auth| auth.oauth2.as_mut()) {
            secrets.insert(
                format!("{prefix}__AUTH__OAUTH2__CLIENT_SECRET"),
                std::mem::take(&mut oauth2.client_secret),
            );
        }
        if let Some(aws) = self.auth.as_mut().and_then(|auth| auth.aws_sigv4.as_mut()) {
            for (name, value) in [
                ("SECRET_KEY", &mut aws.secret_key),
                ("SESSION_TOKEN", &mut aws.session_token),
            ] {
                if let Some(value) = value.take() {
                    secrets.insert(format!("{prefix}__AUTH__AWS_SIGV4__{name}"), value);
                }
            }
        }
        if let Some(read) = self.read.as_mut() {
            extract_sensitive_optional_url(
                &mut read.checkpoint_store,
                prefix,
                "READ__CHECKPOINT_STORE",
                secrets,
            );
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for PostgresCdcConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        if let Some(val) = self.checkpoint_store.take() {
            secrets.insert(format!("{}__{}", prefix, "CHECKPOINT_STORE"), val);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for GrpcConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        extract_sensitive_url(&mut self.url, prefix, "URL", secrets);
        extract_sensitive_string_map_entries(&mut self.metadata, prefix, "METADATA", secrets);
        extract_binary_map_entries(
            &mut self.binary_metadata,
            prefix,
            "BINARY_METADATA",
            secrets,
        );
        if let Some(value) = self.bearer_token.take() {
            secrets.insert(format!("{}__BEARER_TOKEN", prefix), value);
        }
        if let Some(value) = self.api_key.take() {
            secrets.insert(format!("{}__API_KEY", prefix), value);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for TlsConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if let Some(val) = self.cert_password.take() {
            secrets.insert(format!("{}__{}", prefix, "CERT_PASSWORD"), val);
        }
    }
}

impl SecretExtractor for IbmTlsConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if let Some(val) = self.key_repository_password.take() {
            // Wire/env name matches the serde rename (`cert_password`), so the config
            // crate's env override resolves back to this field.
            secrets.insert(format!("{}__{}", prefix, "CERT_PASSWORD"), val);
        }
    }
}

/// Extracts sensitive values (passwords, keys, tokens) from the configuration
/// and returns them as a map of environment variables (key-value pairs).
/// The extracted fields in the configuration are set to `None`.
///
/// The keys in the returned map follow the `MQB__{ROUTE}__{ENDPOINT}__{FIELD}` pattern
/// compatible with the `config` crate's environment variable override mechanism.
///
/// Two names that differ only in non-alphanumeric characters share a key, and one secret
/// overwrites the other. [`check_config_secret_keys`] reports that case.
pub fn extract_config_secrets(config: &mut Config) -> HashMap<String, String> {
    let mut secrets = HashMap::new();
    for (route_name, route) in config.iter_mut() {
        let prefix = sanitize_secret_key(&format!("MQB__{}", route_name));
        route.extract_secrets(&prefix, &mut secrets);
    }
    secrets
}
