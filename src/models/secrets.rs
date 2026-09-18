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
        .filter(|key| {
            let key = key.to_ascii_lowercase();
            ["key", "token", "auth", "secret", "password", "cookie"]
                .iter()
                .any(|needle| key.contains(needle))
        })
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
pub fn extract_config_secrets(config: &mut Config) -> HashMap<String, String> {
    let mut secrets = HashMap::new();
    for (route_name, route) in config.iter_mut() {
        let prefix = sanitize_secret_key(&format!("MQB__{}", route_name));
        route.extract_secrets(&prefix, &mut secrets);
    }
    secrets
}
