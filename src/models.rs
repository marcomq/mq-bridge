//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{collections::HashMap, sync::Arc};

use crate::{
    endpoints::memory::{get_or_create_channel, MemoryChannel},
    traits::Handler,
};

/// The top-level configuration is a map of named routes.
/// The key is the route name (e.g., "kafka_to_nats").
pub type Config = HashMap<String, Route>;

/// Defines a single message processing route from an input to an output.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// (Optional) Number of concurrent processing tasks for this route. Defaults to 1.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// (Optional) Number of messages to process in a single batch. Defaults to 128.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// The input/source endpoint for the route.
    pub input: Endpoint,
    /// The output/sink endpoint for the route.
    #[serde(default = "default_output_endpoint")]
    pub output: Endpoint,
}

pub(crate) fn default_concurrency() -> usize {
    1
}

pub(crate) fn default_batch_size() -> usize {
    128
}

fn default_output_endpoint() -> Endpoint {
    Endpoint::new(EndpointType::Null)
}

fn default_dlq_retry_attempts() -> usize {
    3
}

fn default_true() -> bool {
    true
}

fn default_retry_attempts() -> usize {
    3
}
fn default_initial_interval_ms() -> u64 {
    100
}
fn default_max_interval_ms() -> u64 {
    5000
}
fn default_multiplier() -> f64 {
    2.0
}

/// Represents a connection point for messages, which can be a source (input) or a sink (output).
#[derive(Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// (Optional) A list of middlewares to apply to the endpoint.
    #[serde(default)]
    pub middlewares: Vec<Middleware>,

    #[serde(default)]
    pub mode: ConsumerMode,

    /// The specific endpoint implementation, determined by the configuration key (e.g., "kafka", "nats").
    #[serde(flatten)]
    pub endpoint_type: EndpointType,

    #[serde(skip_serializing)]
    pub handler: Option<Arc<dyn Handler>>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("middlewares", &self.middlewares)
            .field("mode", &self.mode)
            .field("endpoint_type", &self.endpoint_type)
            .field(
                "handler",
                &if self.handler.is_some() {
                    "Some(<Handler>)"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EndpointVisitor;

        impl<'de> Visitor<'de> for EndpointVisitor {
            type Value = Endpoint;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map representing an endpoint or null")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Endpoint::new(EndpointType::Null))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                // Buffer the map into a temporary serde_json::Map.
                // This allows us to separate the `middlewares` field from the rest.
                let mut temp_map = serde_json::Map::new();
                let mut middlewares_val = None;
                let mut mode_val = None;

                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if key == "middlewares" {
                        middlewares_val = Some(value);
                    } else if key == "mode" {
                        mode_val = Some(value);
                    } else {
                        temp_map.insert(key, value);
                    }
                }

                // Deserialize the rest of the map into the flattened EndpointType.
                let endpoint_type: EndpointType =
                    serde_json::from_value(serde_json::Value::Object(temp_map))
                        .map_err(serde::de::Error::custom)?;

                // Deserialize the extracted middlewares value using the existing helper logic.
                let middlewares = match middlewares_val {
                    Some(val) => {
                        deserialize_middlewares_from_value(val).map_err(serde::de::Error::custom)?
                    }
                    None => Vec::new(),
                };

                let mode = match mode_val {
                    Some(val) => serde_json::from_value(val).map_err(serde::de::Error::custom)?,
                    None => ConsumerMode::default(),
                };
                Ok(Endpoint {
                    middlewares,
                    mode,
                    endpoint_type,
                    handler: None,
                })
            }
        }

        deserializer.deserialize_any(EndpointVisitor)
    }
}

impl Endpoint {
    pub fn new(endpoint_type: EndpointType) -> Self {
        Self {
            middlewares: Vec::new(),
            mode: ConsumerMode::default(),
            endpoint_type,
            handler: None,
        }
    }
    pub fn new_memory(topic: &str, capacity: usize) -> Self {
        Self::new(EndpointType::Memory(MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(capacity),
            ..Default::default()
        }))
    }
    pub fn add_middleware(mut self, middleware: Middleware) -> Self {
        self.middlewares.push(middleware);
        self
    }
    ///
    /// Returns a reference to the in-memory channel associated with this Endpoint.
    /// This function will only succeed if the Endpoint is of type EndpointType::Memory.
    /// If the Endpoint is not a memory endpoint, this function will return an error.
    /// This function is primarily used for testing purposes where a Queue is needed.
    pub fn channel(&self) -> anyhow::Result<MemoryChannel> {
        match &self.endpoint_type {
            EndpointType::Memory(cfg) => Ok(get_or_create_channel(cfg)),
            _ => Err(anyhow::anyhow!("channel() called on non-memory Endpoint")),
        }
    }
}

/// Deserialize middlewares from a generic serde_json::Value.
///
/// This logic was extracted from `deserialize_middlewares_from_map_or_seq` to be reused by the custom `Endpoint` deserializer.
fn deserialize_middlewares_from_value(value: serde_json::Value) -> anyhow::Result<Vec<Middleware>> {
    Ok(match value {
        serde_json::Value::Array(arr) => serde_json::from_value(serde_json::Value::Array(arr))?,
        serde_json::Value::Object(map) => {
            let mut middlewares: Vec<_> = map
                .into_iter()
                // The config crate can produce maps with numeric string keys ("0", "1", ...)
                // from environment variables. We need to sort by these keys to maintain order.
                .filter_map(|(key, value)| key.parse::<usize>().ok().map(|index| (index, value)))
                .collect();
            middlewares.sort_by_key(|(index, _)| *index);

            let sorted_values = middlewares.into_iter().map(|(_, value)| value).collect();
            serde_json::from_value(serde_json::Value::Array(sorted_values))?
        }
        _ => return Err(anyhow::anyhow!("Expected an array or object")),
    })
}

/// An enumeration of all supported endpoint types.
/// `#[serde(rename_all = "lowercase")]` ensures that the keys in the config (e.g., "kafka")
/// match the enum variants.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum EndpointType {
    Kafka(KafkaEndpoint),
    Nats(NatsEndpoint),
    File(String),
    Static(String),
    Memory(MemoryConfig),
    Amqp(AmqpEndpoint),
    MongoDb(MongoDbEndpoint),
    Mqtt(MqttEndpoint),
    Http(HttpEndpoint),
    Fanout(Vec<Endpoint>),
    Null,
}

/// An enumeration of all supported middleware types.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Middleware {
    Deduplication(DeduplicationMiddleware),
    Metrics(MetricsMiddleware),
    Dlq(Box<DeadLetterQueueMiddleware>),
    Retry(RetryMiddleware),
    RandomPanic(RandomPanicMiddleware),
}

/// Deduplication middleware configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    pub sled_path: String,
    pub ttl_seconds: u64,
}

/// Metrics middleware configuration. It's currently a struct without fields
/// but can be extended later. Its presence in the config enables the middleware.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct MetricsMiddleware {}

/// Dead-Letter Queue (DLQ) middleware configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DeadLetterQueueMiddleware {
    pub endpoint: Endpoint,
    /// Number of retry attempts for the DLQ send. Defaults to 3.
    #[serde(default = "default_dlq_retry_attempts")]
    pub dlq_retry_attempts: usize,
    #[serde(default = "default_initial_interval_ms")]
    pub dlq_initial_interval_ms: u64,
    #[serde(default = "default_max_interval_ms")]
    pub dlq_max_interval_ms: u64,
    #[serde(default = "default_multiplier")]
    pub dlq_multiplier: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RetryMiddleware {
    #[serde(default = "default_retry_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_initial_interval_ms")]
    pub initial_interval_ms: u64,
    #[serde(default = "default_max_interval_ms")]
    pub max_interval_ms: u64,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RandomPanicMiddleware {
    #[serde(deserialize_with = "deserialize_probability")]
    pub probability: f64,
}

fn deserialize_probability<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "probability must be between 0.0 and 1.0",
        ));
    }
    Ok(value)
}

// --- Kafka Specific Configuration ---

/// Kafka endpoint configuration, combining connection and topic details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct KafkaEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: KafkaConfig,
}

/// General Kafka connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct KafkaConfig {
    pub brokers: String,
    pub group_id: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>, // Consider using a secret management type
    #[serde(default)]
    pub tls: TlsConfig,
    /// If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    #[serde(default)]
    pub producer_options: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub consumer_options: Option<Vec<(String, String)>>,
}

// --- NATS Specific Configuration ---

/// NATS endpoint configuration, combining connection and subject details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NatsEndpoint {
    pub subject: Option<String>,
    pub stream: Option<String>,
    #[serde(flatten)]
    pub config: NatsConfig,
}

/// General NATS connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct NatsConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    /// If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// If no_jetstream: true, use Core NATS (fire-and-forget) instead of JetStream. Defaults to false.
    #[serde(default)]
    pub no_jetstream: bool,
    pub default_stream: Option<String>,
    pub prefetch_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerMode {
    #[default]
    Consume,
    Subscribe,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub topic: String,
    pub capacity: Option<usize>,
}

// --- AMQP Specific Configuration ---

/// AMQP endpoint configuration, combining connection and queue details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AmqpEndpoint {
    pub queue: Option<String>,
    #[serde(flatten)]
    pub config: AmqpConfig,
}

/// General AMQP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct AmqpConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub exchange: Option<String>,
    pub prefetch_count: Option<u16>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub no_persistence: bool,
    /// If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
}

// --- MongoDB Specific Configuration ---

/// MongoDB endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct MongoDbEndpoint {
    pub collection: Option<String>,
    #[serde(flatten)]
    pub config: MongoDbConfig,
}

/// General MongoDB connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MongoDbConfig {
    pub url: String,
    pub database: String,
    pub polling_interval_ms: Option<u64>,
    pub ttl_seconds: Option<u64>,
}

// --- MQTT Specific Configuration ---

/// MQTT endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: MqttConfig,
}

/// General MQTT connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    pub queue_capacity: Option<usize>,
    pub qos: Option<u8>,
    #[serde(default = "default_true")]
    pub clean_session: bool,
    pub keep_alive_seconds: Option<u64>,
}

// --- HTTP Specific Configuration ---

/// HTTP endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct HttpEndpoint {
    #[serde(flatten)]
    pub config: HttpConfig,
}

/// General HTTP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub url: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    pub response_out: Option<Box<Endpoint>>,
}

// --- Common Configuration ---

/// TLS configuration for secure connections.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub required: bool,
    pub ca_file: Option<String>,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub cert_password: Option<String>,
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

impl TlsConfig {
    pub fn is_mtls_client_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }
    pub fn is_tls_server_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config as ConfigBuilder, Environment};

    const TEST_YAML: &str = r#"
kafka_to_nats:
  concurrency: 10
  input:
    middlewares:
      - deduplication:
          sled_path: "/tmp/mq-bridge/dedup_db"
          ttl_seconds: 3600
      - metrics: {}
      - retry:
          max_attempts: 5
          initial_interval_ms: 200
      - random_panic:
          probability: 0.1
      - dlq:
          endpoint:
            nats:
              subject: "dlq-subject"
              url: "nats://localhost:4222"
    kafka:
      topic: "input-topic"
      brokers: "localhost:9092"
      group_id: "my-consumer-group"
      tls:
        required: true
        ca_file: "/path_to_ca"
        cert_file: "/path_to_cert"
        key_file: "/path_to_key"
        cert_password: "password"
        accept_invalid_certs: true
  output:
    middlewares:
      - metrics: {}
    nats:
      subject: "output-subject"
      url: "nats://localhost:4222"
"#;

    fn assert_config_values(config: &Config) {
        assert_eq!(config.len(), 1);
        let route = config.get("kafka_to_nats").expect("Route should exist");

        assert_eq!(route.concurrency, 10);

        // --- Assert Input ---
        let input = &route.input;
        assert_eq!(input.middlewares.len(), 5);

        let mut has_dedup = false;
        let mut has_metrics = false;
        let mut has_dlq = false;
        let mut has_retry = false;
        let mut has_random_panic = false;
        for middleware in &input.middlewares {
            match middleware {
                Middleware::Deduplication(dedup) => {
                    assert_eq!(dedup.sled_path, "/tmp/mq-bridge/dedup_db");
                    assert_eq!(dedup.ttl_seconds, 3600);
                    has_dedup = true;
                }
                Middleware::Metrics(_) => {
                    has_metrics = true;
                }
                Middleware::Dlq(dlq) => {
                    assert!(dlq.endpoint.middlewares.is_empty());
                    if let EndpointType::Nats(nats_cfg) = &dlq.endpoint.endpoint_type {
                        assert_eq!(nats_cfg.subject, Some("dlq-subject".to_string()));
                        assert_eq!(nats_cfg.config.url, "nats://localhost:4222");
                    }
                    has_dlq = true;
                }
                Middleware::Retry(retry) => {
                    assert_eq!(retry.max_attempts, 5);
                    assert_eq!(retry.initial_interval_ms, 200);
                    has_retry = true;
                }
                Middleware::RandomPanic(rp) => {
                    assert!((rp.probability - 0.1).abs() < f64::EPSILON);
                    has_random_panic = true;
                }
            }
        }

        if let EndpointType::Kafka(kafka) = &input.endpoint_type {
            assert_eq!(kafka.topic, Some("input-topic".to_string()));
            assert_eq!(kafka.config.brokers, "localhost:9092");
            assert_eq!(kafka.config.group_id, Some("my-consumer-group".to_string()));
            let tls = &kafka.config.tls;
            assert_eq!(tls.required, true);
            assert_eq!(tls.ca_file.as_deref(), Some("/path_to_ca"));
            assert_eq!(tls.accept_invalid_certs, true);
        } else {
            panic!("Input endpoint should be Kafka");
        }
        assert!(has_dedup);
        assert!(has_metrics);
        assert!(has_dlq);
        assert!(has_retry);
        assert!(has_random_panic);

        // --- Assert Output ---
        let output = &route.output;
        assert_eq!(output.middlewares.len(), 1);
        assert!(matches!(output.middlewares[0], Middleware::Metrics(_)));

        if let EndpointType::Nats(nats) = &output.endpoint_type {
            assert_eq!(nats.subject, Some("output-subject".to_string()));
            assert_eq!(nats.config.url, "nats://localhost:4222");
        } else {
            panic!("Output endpoint should be NATS");
        }
    }

    #[test]
    fn test_deserialize_from_yaml() {
        // We use serde_yaml directly here because the `config` crate's processing
        // can interfere with complex deserialization logic.
        let result: Result<Config, _> = serde_yaml_ng::from_str(TEST_YAML);
        println!("Deserialized from YAML: {:#?}", result);
        let config = result.expect("Failed to deserialize TEST_YAML");
        assert_config_values(&config);
    }

    #[test]
    fn test_deserialize_from_env() {
        // Set environment variables based on README
        unsafe {
            std::env::set_var("MQB__KAFKA_TO_NATS__CONCURRENCY", "10");
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC", "input-topic");
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__BROKERS",
                "localhost:9092",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__GROUP_ID",
                "my-consumer-group",
            );
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__REQUIRED", "true");
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__CA_FILE",
                "/path_to_ca",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__ACCEPT_INVALID_CERTS",
                "true",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__OUTPUT__NATS__SUBJECT",
                "output-subject",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__OUTPUT__NATS__URL",
                "nats://localhost:4222",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__SUBJECT",
                "dlq-subject",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__URL",
                "nats://localhost:4222",
            );
        }

        let builder = ConfigBuilder::builder()
            // Enable automatic type parsing for values from environment variables.
            .add_source(
                Environment::with_prefix("MQB")
                    .separator("__")
                    .try_parsing(true),
            );

        let config: Config = builder
            .build()
            .expect("Failed to build config")
            .try_deserialize()
            .expect("Failed to deserialize config");

        // We can't test all values from env, but we can check the ones we set.
        assert_eq!(config.get("kafka_to_nats").unwrap().concurrency, 10);
        if let EndpointType::Kafka(k) = &config.get("kafka_to_nats").unwrap().input.endpoint_type {
            assert_eq!(k.topic, Some("input-topic".to_string()));
            assert!(k.config.tls.required);
        } else {
            panic!("Expected Kafka endpoint");
        }

        let input = &config.get("kafka_to_nats").unwrap().input;
        assert_eq!(input.middlewares.len(), 1);
        if let Middleware::Dlq(_) = &input.middlewares[0] {
            // Correctly parsed
        } else {
            panic!("Expected DLQ middleware");
        }
    }

    #[test]
    fn test_deserialize_fanout_yaml() {
        let yaml = r#"
fanout_route:
  input:
    memory:
      topic: "input"
  output:
    fanout:
      - memory:
          topic: "out1"
      - memory:
          topic: "out2"
"#;
        let config: Config =
            serde_yaml_ng::from_str(yaml).expect("Failed to deserialize fanout config");
        let route = config.get("fanout_route").expect("Route should exist");

        if let EndpointType::Fanout(endpoints) = &route.output.endpoint_type {
            assert_eq!(endpoints.len(), 2);
            if let EndpointType::Memory(m) = &endpoints[0].endpoint_type {
                assert_eq!(m.topic, "out1");
            } else {
                panic!("Expected memory endpoint 1");
            }
            if let EndpointType::Memory(m) = &endpoints[1].endpoint_type {
                assert_eq!(m.topic, "out2");
            } else {
                panic!("Expected memory endpoint 2");
            }
        } else {
            panic!("Expected Fanout endpoint");
        }
    }

    #[test]
    fn test_deserialize_subscribe_mode() {
        let yaml = r#"
subscribe_route:
  input:
    mode: subscribe
    memory:
      topic: "events"
  output:
    null: null
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
        let route = config.get("subscribe_route").expect("Route not found");
        assert_eq!(route.input.mode, ConsumerMode::Subscribe);
    }

    #[test]
    fn test_deserialize_null_endpoint_shorthand() {
        let yaml = r#"
null_route:
  input:
    memory: { topic: "in" }
  output: null
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
        let route = config.get("null_route").expect("Route not found");
        assert!(matches!(route.output.endpoint_type, EndpointType::Null));
    }
}
