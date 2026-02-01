//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{collections::HashMap, sync::Arc};

use crate::traits::Handler;
use tracing::trace;

/// The top-level configuration is a map of named routes.
/// The key is the route name (e.g., "kafka_to_nats").
///
/// # Examples
///
/// Deserializing a complex configuration from YAML:
///
/// ```
/// use mq_bridge::models::{Config, EndpointType, Middleware};
///
/// let yaml = r#"
/// kafka_to_nats:
///   concurrency: 10
///   input:
///     middlewares:
///       - deduplication:
///           sled_path: "/tmp/mq-bridge/dedup_db"
///           ttl_seconds: 3600
///       - metrics: {}
///     kafka:
///       topic: "input-topic"
///       url: "localhost:9092"
///       group_id: "my-consumer-group"
///   output:
///     nats:
///       subject: "output-subject"
///       url: "nats://localhost:4222"
/// "#;
///
/// let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
/// let route = config.get("kafka_to_nats").unwrap();
///
/// assert_eq!(route.options.concurrency, 10);
/// // Check input middleware
/// assert!(route.input.middlewares.iter().any(|m| matches!(m, Middleware::Deduplication(_))));
/// // Check output endpoint
/// assert!(matches!(route.output.endpoint_type, EndpointType::Nats(_)));
/// ```
pub type Config = HashMap<String, Route>;

/// A configuration map for named publishers (endpoints).
/// The key is the publisher name.
pub type PublisherConfig = HashMap<String, Endpoint>;

/// Defines a single message processing route from an input to an output.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// The input/source endpoint for the route.
    pub input: Endpoint,
    /// The output/sink endpoint for the route.
    #[serde(default = "default_output_endpoint")]
    pub output: Endpoint,
    /// (Optional) Fine-tuning options for the route's execution.
    #[serde(flatten, default)]
    pub options: RouteOptions,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            input: Endpoint::null(),
            output: Endpoint::null(),
            options: RouteOptions::default(),
        }
    }
}

/// Fine-tuning options for a route's execution.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteOptions {
    /// (Optional) Number of concurrent processing tasks for this route. Defaults to 1.
    #[serde(default = "default_concurrency")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub concurrency: usize,
    /// (Optional) Number of messages to process in a single batch. Defaults to 1.
    #[serde(default = "default_batch_size")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub batch_size: usize,
    /// (Optional) The maximum number of concurrent commit tasks allowed. Defaults to 4096.
    #[serde(default = "default_commit_concurrency_limit")]
    pub commit_concurrency_limit: usize,
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            batch_size: default_batch_size(),
            commit_concurrency_limit: default_commit_concurrency_limit(),
        }
    }
}

pub(crate) fn default_concurrency() -> usize {
    1
}

pub(crate) fn default_batch_size() -> usize {
    1
}

pub(crate) fn default_commit_concurrency_limit() -> usize {
    4096
}

fn default_output_endpoint() -> Endpoint {
    Endpoint::new(EndpointType::Null)
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
fn default_clean_session() -> bool {
    false
}

fn is_known_endpoint_name(name: &str) -> bool {
    matches!(
        name,
        "aws"
            | "kafka"
            | "nats"
            | "file"
            | "static"
            | "memory"
            | "amqp"
            | "mongodb"
            | "mqtt"
            | "http"
            | "ibm-mq"
            | "ibmmq"
            | "zeromq"
            | "fanout"
            | "switch"
            | "response"
    )
}

/// Represents a connection point for messages, which can be a source (input) or a sink (output).
#[derive(Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// (Optional) A list of middlewares to apply to the endpoint.
    #[serde(default)]
    pub middlewares: Vec<Middleware>,

    /// The specific endpoint implementation, determined by the configuration key (e.g., "kafka", "nats").
    #[serde(flatten)]
    pub endpoint_type: EndpointType,

    #[serde(skip_serializing)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    /// Internal handler for processing messages (not serialized).
    pub handler: Option<Arc<dyn Handler>>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("middlewares", &self.middlewares)
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

                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if key == "middlewares" {
                        middlewares_val = Some(value);
                    } else {
                        temp_map.insert(key, value);
                    }
                }

                // Deserialize the rest of the map into the flattened EndpointType.
                let temp_val = serde_json::Value::Object(temp_map);
                let endpoint_type: EndpointType = match serde_json::from_value(temp_val.clone()) {
                    Ok(et) => et,
                    Err(original_err) => {
                        if let serde_json::Value::Object(map) = &temp_val {
                            if map.len() == 1 {
                                let (name, config) = map.iter().next().unwrap();
                                if is_known_endpoint_name(name) {
                                    return Err(serde::de::Error::custom(original_err));
                                }
                                trace!("Falling back to Custom endpoint for key: {}", name);
                                EndpointType::Custom {
                                    name: name.clone(),
                                    config: config.clone(),
                                }
                            } else if map.is_empty() {
                                EndpointType::Null
                            } else {
                                return Err(serde::de::Error::custom(
                                    "Invalid endpoint configuration: multiple keys found or unknown endpoint type",
                                ));
                            }
                        } else {
                            return Err(serde::de::Error::custom("Invalid endpoint configuration"));
                        }
                    }
                };

                // Deserialize the extracted middlewares value using the existing helper logic.
                let middlewares = match middlewares_val {
                    Some(val) => {
                        deserialize_middlewares_from_value(val).map_err(serde::de::Error::custom)?
                    }
                    None => Vec::new(),
                };

                Ok(Endpoint {
                    middlewares,
                    endpoint_type,
                    handler: None,
                })
            }
        }

        deserializer.deserialize_any(EndpointVisitor)
    }
}

fn is_known_middleware_name(name: &str) -> bool {
    matches!(
        name,
        "deduplication"
            | "metrics"
            | "dlq"
            | "retry"
            | "random_panic"
            | "delay"
            | "weak_join"
            | "custom"
    )
}

/// Deserialize middlewares from a generic serde_json::Value.
///
/// This logic was extracted from `deserialize_middlewares_from_map_or_seq` to be reused by the custom `Endpoint` deserializer.
fn deserialize_middlewares_from_value(value: serde_json::Value) -> anyhow::Result<Vec<Middleware>> {
    let arr = match value {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(map) => {
            let mut middlewares: Vec<_> = map
                .into_iter()
                // The config crate can produce maps with numeric string keys ("0", "1", ...)
                // from environment variables. We need to sort by these keys to maintain order.
                .filter_map(|(key, value)| key.parse::<usize>().ok().map(|index| (index, value)))
                .collect();
            middlewares.sort_by_key(|(index, _)| *index);

            middlewares.into_iter().map(|(_, value)| value).collect()
        }
        _ => return Err(anyhow::anyhow!("Expected an array or object")),
    };

    let mut middlewares = Vec::new();
    for item in arr {
        // Check if it is a map with a single key that matches a known middleware
        let known_name = if let serde_json::Value::Object(map) = &item {
            if map.len() == 1 {
                let (name, _) = map.iter().next().unwrap();
                if is_known_middleware_name(name) {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(name) = known_name {
            match serde_json::from_value::<Middleware>(item.clone()) {
                Ok(m) => middlewares.push(m),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to deserialize known middleware '{}': {}",
                        name,
                        e
                    ))
                }
            }
        } else if let Ok(m) = serde_json::from_value::<Middleware>(item.clone()) {
            middlewares.push(m);
        } else if let serde_json::Value::Object(map) = &item {
            if map.len() == 1 {
                let (name, config) = map.iter().next().unwrap();
                middlewares.push(Middleware::Custom {
                    name: name.clone(),
                    config: config.clone(),
                });
            } else {
                return Err(anyhow::anyhow!(
                    "Invalid middleware configuration: {:?}",
                    item
                ));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Invalid middleware configuration: {:?}",
                item
            ));
        }
    }
    Ok(middlewares)
}

/// An enumeration of all supported endpoint types.
/// `#[serde(rename_all = "lowercase")]` ensures that the keys in the config (e.g., "kafka")
/// match the enum variants.
///
/// # Examples
///
/// Configuring a Fanout endpoint in YAML:
/// ```
/// use mq_bridge::models::{Endpoint, EndpointType};
///
/// let yaml = r#"
/// fanout:
///   - memory: { topic: "out1" }
///   - memory: { topic: "out2" }
/// "#;
///
/// let endpoint: Endpoint = serde_yaml_ng::from_str(yaml).unwrap();
/// if let EndpointType::Fanout(targets) = endpoint.endpoint_type {
///     assert_eq!(targets.len(), 2);
/// }
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum EndpointType {
    Aws(AwsConfig),
    Kafka(KafkaConfig),
    Nats(NatsConfig),
    File(FileConfig),
    Static(String),
    Memory(MemoryConfig),
    Amqp(AmqpConfig),
    MongoDb(MongoDbConfig),
    Mqtt(MqttConfig),
    Http(HttpConfig),
    IbmMq(IbmMqConfig),
    ZeroMq(ZeroMqConfig),
    Fanout(Vec<Endpoint>),
    Switch(SwitchConfig),
    Response(ResponseConfig),
    Custom {
        name: String,
        config: serde_json::Value,
    },
    #[default]
    Null,
}

impl EndpointType {
    pub fn name(&self) -> &'static str {
        match self {
            EndpointType::Aws(_) => "aws",
            EndpointType::Kafka(_) => "kafka",
            EndpointType::Nats(_) => "nats",
            EndpointType::File(_) => "file",
            EndpointType::Static(_) => "static",
            EndpointType::Memory(_) => "memory",
            EndpointType::Amqp(_) => "amqp",
            EndpointType::MongoDb(_) => "mongodb",
            EndpointType::Mqtt(_) => "mqtt",
            EndpointType::Http(_) => "http",
            EndpointType::IbmMq(_) => "ibmmq",
            EndpointType::ZeroMq(_) => "zeromq",
            EndpointType::Fanout(_) => "fanout",
            EndpointType::Switch(_) => "switch",
            EndpointType::Response(_) => "response",
            EndpointType::Custom { .. } => "custom",
            EndpointType::Null => "null",
        }
    }

    pub fn is_core(&self) -> bool {
        matches!(
            self,
            EndpointType::File(_)
                | EndpointType::Static(_)
                | EndpointType::Memory(_)
                | EndpointType::Fanout(_)
                | EndpointType::Switch(_)
                | EndpointType::Response(_)
                | EndpointType::Custom { .. }
                | EndpointType::Null
        )
    }
}

/// An enumeration of all supported middleware types.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Middleware {
    Deduplication(DeduplicationMiddleware),
    Metrics(MetricsMiddleware),
    Dlq(Box<DeadLetterQueueMiddleware>),
    Retry(RetryMiddleware),
    RandomPanic(RandomPanicMiddleware),
    Delay(DelayMiddleware),
    WeakJoin(WeakJoinMiddleware),
    Custom {
        name: String,
        config: serde_json::Value,
    },
}

/// Deduplication middleware configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    /// Path to the Sled database directory.
    pub sled_path: String,
    /// Time-to-live for deduplication entries in seconds.
    pub ttl_seconds: u64,
}

/// Metrics middleware configuration. It's currently a struct without fields
/// but can be extended later. Its presence in the config enables the middleware.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MetricsMiddleware {}

/// Dead-Letter Queue (DLQ) middleware configuration. It is recommended that the
/// endpoint is also using a retry to avoid message loss
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeadLetterQueueMiddleware {
    /// The endpoint to send failed messages to.
    pub endpoint: Endpoint,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RetryMiddleware {
    /// Maximum number of retry attempts. Defaults to 3.
    #[serde(default = "default_retry_attempts")]
    pub max_attempts: usize,
    /// Initial retry interval in milliseconds. Defaults to 100ms.
    #[serde(default = "default_initial_interval_ms")]
    pub initial_interval_ms: u64,
    /// Maximum retry interval in milliseconds. Defaults to 5000ms.
    #[serde(default = "default_max_interval_ms")]
    pub max_interval_ms: u64,
    /// Multiplier for exponential backoff. Defaults to 2.0.
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DelayMiddleware {
    /// Delay duration in milliseconds.
    pub delay_ms: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WeakJoinMiddleware {
    /// The metadata key to group messages by (e.g., "correlation_id").
    pub group_by: String,
    /// The number of messages to wait for.
    pub expected_count: usize,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RandomPanicMiddleware {
    /// Probability of panic (0.0 to 1.0).
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

// --- AWS Specific Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    /// The SQS queue URL. Required for Consumer. Optional for Publisher if `topic_arn` is set.
    pub queue_url: Option<String>,
    /// (Publisher only) The SNS topic ARN.
    pub topic_arn: Option<String>,
    /// AWS Region (e.g., "us-east-1").
    pub region: Option<String>,
    /// Custom endpoint URL (e.g., for LocalStack).
    pub endpoint_url: Option<String>,
    /// AWS Access Key ID.
    pub access_key: Option<String>,
    /// AWS Secret Access Key.
    pub secret_key: Option<String>,
    /// AWS Session Token.
    pub session_token: Option<String>,
    /// (Consumer only) Maximum number of messages to receive in a batch (1-10).
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 10)))]
    pub max_messages: Option<i32>,
    /// (Consumer only) Wait time for long polling in seconds (0-20).
    #[cfg_attr(feature = "schema", schemars(range(min = 0, max = 20)))]
    pub wait_time_seconds: Option<i32>,
}

// --- Kafka Specific Configuration ---

/// General Kafka connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct KafkaConfig {
    /// Comma-separated list of Kafka broker URLs.
    #[serde(alias = "brokers")]
    pub url: String,
    /// The Kafka topic to produce to or consume from.
    pub topic: Option<String>,
    /// Optional username for SASL authentication.
    pub username: Option<String>,
    /// Optional password for SASL authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// (Consumer only) Consumer group ID.
    /// If not provided, the consumer acts in **Subscriber mode**: it generates a unique, ephemeral group ID and starts consuming from the latest offset.
    pub group_id: Option<String>,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// (Publisher only) Additional librdkafka producer configuration options (key-value pairs).
    #[serde(default)]
    pub producer_options: Option<Vec<(String, String)>>,
    /// (Consumer only) Additional librdkafka consumer configuration options (key-value pairs).
    #[serde(default)]
    pub consumer_options: Option<Vec<(String, String)>>,
}

// --- File Specific Configuration ---

#[derive(Debug, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileConfig {
    /// Path to the file.
    pub path: String,
    /// (Consumer only) If true, acts in **Subscriber mode** (like `tail -f`), reading new lines as they are written.
    /// If false (default), acts in Consumer mode, reading lines and removing them from the file (queue behavior).
    #[serde(default)]
    pub subscribe_mode: bool,
    /// (Consumer only) If true, lines are removed from the file after being processed by all subscribers.
    /// Defaults to true if subscribe_mode is false, and false if subscribe_mode is true.
    pub delete: Option<bool>,
}

impl<'de> Deserialize<'de> for FileConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FileConfigVisitor;
        impl<'de> Visitor<'de> for FileConfigVisitor {
            type Value = FileConfig;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("string or map")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(FileConfig {
                    path: value.to_string(),
                    subscribe_mode: false,
                    delete: None,
                })
            }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut path = None;
                let mut consume = true;
                let mut subscribe_mode = None;
                let mut delete = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => {
                            if path.is_some() {
                                return Err(serde::de::Error::duplicate_field("path"));
                            }
                            path = Some(map.next_value()?);
                        }
                        "consume" => {
                            consume = map.next_value()?;
                        }
                        "subscribe_mode" => {
                            if subscribe_mode.is_some() {
                                return Err(serde::de::Error::duplicate_field("subscribe_mode"));
                            }
                            subscribe_mode = Some(map.next_value()?);
                        }
                        "delete" => {
                            if delete.is_some() {
                                return Err(serde::de::Error::duplicate_field("delete"));
                            }
                            delete = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                Ok(FileConfig {
                    path,
                    subscribe_mode: subscribe_mode.unwrap_or(!consume),
                    delete,
                })
            }
        }
        deserializer.deserialize_any(FileConfigVisitor)
    }
}

// --- NATS Specific Configuration ---

/// General NATS connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NatsConfig {
    /// Comma-separated list of NATS server URLs (e.g., "nats://localhost:4222,nats://localhost:4223").
    pub url: String,
    /// The NATS subject to publish to or subscribe to.
    pub subject: Option<String>,
    /// (Consumer only). The JetStream stream name. Required for Consumers.
    pub stream: Option<String>,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Optional token for authentication.
    pub token: Option<String>,
    /// (Publisher only) If true, the publisher uses the request-reply pattern.
    /// It sends a request and waits for a response (using `core_client.request_with_headers()`).
    /// Defaults to false.
    #[serde(default)]
    pub request_reply: bool,
    /// (Publisher only) Timeout for request-reply operations in milliseconds. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// If no_jetstream: true, use Core NATS (fire-and-forget) instead of JetStream. Defaults to false.
    #[serde(default)]
    pub no_jetstream: bool,
    /// (Consumer only) If true, use ephemeral **Subscriber mode**. Defaults to false (durable consumer).
    #[serde(default)]
    pub subscriber_mode: bool,
    /// (Publisher only) Maximum number of messages in the stream (if created by the bridge). Defaults to 1,000,000.
    pub stream_max_messages: Option<i64>,
    /// (Publisher only) Maximum total bytes in the stream (if created by the bridge). Defaults to 1GB.
    pub stream_max_bytes: Option<i64>,
    /// (Consumer only) Number of messages to prefetch from the consumer. Defaults to 10000.
    pub prefetch_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// The topic name for the in-memory channel.
    pub topic: String,
    /// The capacity of the channel. Defaults to 100.
    pub capacity: Option<usize>,
    /// (Publisher only) If true, send() waits for a response.
    #[serde(default)]
    pub request_reply: bool,
    /// (Publisher only) Timeout for request-reply operations in milliseconds. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Consumer only) If true, act as a **Subscriber** (fan-out). Defaults to false (queue).
    #[serde(default)]
    pub subscribe_mode: bool,
    /// (Consumer only) If true, enables NACK support (re-queuing), which requires cloning messages. Defaults to false.
    #[serde(default)]
    pub enable_nack: bool,
}

impl MemoryConfig {
    pub fn new(topic: impl Into<String>, capacity: Option<usize>) -> Self {
        Self {
            topic: topic.into(),
            capacity,
            ..Default::default()
        }
    }
    pub fn with_subscribe(self, subscribe_mode: bool) -> Self {
        Self {
            subscribe_mode,
            ..self
        }
    }
}

// --- AMQP Specific Configuration ---

/// General AMQP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmqpConfig {
    /// AMQP connection URI. The `lapin` client connects to a single host specified in the URI.
    /// For high availability, provide the address of a load balancer or use DNS resolution
    /// that points to multiple brokers. Example: "amqp://localhost:5672/vhost".
    pub url: String,
    /// The AMQP queue name.
    pub queue: Option<String>,
    /// (Consumer only) If true, act as a **Subscriber** (fan-out). Defaults to false.
    #[serde(default)]
    pub subscribe_mode: bool,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// The exchange to publish to or bind the queue to.
    pub exchange: Option<String>,
    /// (Consumer only) Number of messages to prefetch. Defaults to 100.
    pub prefetch_count: Option<u16>,
    /// If true, declare queues as non-durable (transient). Defaults to false. Affects both Consumer (queue durability) and Publisher (message persistence).
    #[serde(default)]
    pub no_persistence: bool,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
}

// --- MongoDB Specific Configuration ---

/// General MongoDB connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MongoDbConfig {
    /// MongoDB connection string URI. Can contain a comma-separated list of hosts for a replica set.
    /// Credentials provided via the separate `username` and `password` fields take precedence over any credentials embedded in the URL.
    pub url: String,
    /// The MongoDB collection name.
    pub collection: Option<String>,
    /// Optional username. Takes precedence over any credentials embedded in the `url`.
    /// Use embedded URL credentials for simple one-off connections but prefer explicit username/password fields (or environment-sourced secrets) for clarity and secret management in production.
    pub username: Option<String>,
    /// Optional password. Takes precedence over any credentials embedded in the `url`.
    /// Use embedded URL credentials for simple one-off connections but prefer explicit username/password fields (or environment-sourced secrets) for clarity and secret management in production.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// The database name.
    pub database: String,
    /// (Consumer only) Polling interval in milliseconds for the consumer (when not using Change Streams). Defaults to 100ms.
    pub polling_interval_ms: Option<u64>,
    /// (Publisher only) Polling interval in milliseconds for the publisher when waiting for a reply. Defaults to 50ms.
    pub reply_polling_ms: Option<u64>,
    /// (Publisher only) If true, the publisher will wait for a response in a dedicated collection. Defaults to false.

    #[serde(default)]
    pub request_reply: bool,
    /// (Consumer only) If true, use Change Streams (**Subscriber mode**). Defaults to false (polling/consumer mode).
    #[serde(default)]
    pub change_stream: bool,
    /// (Publisher only) Timeout for request-reply operations in milliseconds. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Publisher only) TTL in seconds for documents created by the publisher. If set, a TTL index is created.
    pub ttl_seconds: Option<u64>,
    /// (Publisher only) If set, creates a capped collection with this size in bytes.
    pub capped_size_bytes: Option<i64>,
}

// --- MQTT Specific Configuration ---

/// General MQTT connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    /// MQTT broker URL (e.g., "tcp://localhost:1883"). Does not support multiple hosts.
    pub url: String,
    /// The MQTT topic.
    pub topic: Option<String>,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Optional client ID. If not provided, one is generated or derived from route name.
    pub client_id: Option<String>,
    /// Capacity of the internal channel for incoming messages. Defaults to 100.
    pub queue_capacity: Option<usize>,
    /// Maximum number of inflight messages.
    pub max_inflight: Option<u16>,
    /// Quality of Service level (0, 1, or 2). Defaults to 1.
    pub qos: Option<u8>,
    /// (Consumer only) If true, start with a clean session. Defaults to false (persistent session). Setting this to true effectively enables **Subscriber mode** (ephemeral).
    #[serde(default = "default_clean_session")]
    pub clean_session: bool,
    /// Keep-alive interval in seconds. Defaults to 20.
    pub keep_alive_seconds: Option<u64>,
    /// MQTT protocol version (V3 or V5). Defaults to V5.
    #[serde(default)]
    pub protocol: MqttProtocol,
    /// Session expiry interval in seconds (MQTT v5 only).
    pub session_expiry_interval: Option<u32>,
    /// (Publisher only) If true, messages are acknowledged immediately upon receipt (auto-ack).
    /// If false (default), messages are acknowledged after processing (manual-ack).
    #[serde(default)]
    pub delayed_ack: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum MqttProtocol {
    #[default]
    V5,
    V3,
}

// --- ZeroMQ Specific Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ZeroMqConfig {
    /// The ZeroMQ URL (e.g., "tcp://127.0.0.1:5555").
    pub url: String,
    /// The socket type (PUSH, PULL, PUB, SUB, REQ, REP).
    #[serde(default)]
    pub socket_type: Option<ZeroMqSocketType>,
    /// (Consumer only) The ZeroMQ topic (for SUB sockets).
    pub topic: Option<String>,
    /// If true, bind to the address. If false, connect.
    #[serde(default)]
    pub bind: bool,
    /// Internal buffer size for the channel. Defaults to 128.
    #[serde(default)]
    pub internal_buffer_size: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ZeroMqSocketType {
    Push,
    Pull,
    Pub,
    Sub,
    Req,
    Rep,
}

// --- HTTP Specific Configuration ---

/// General HTTP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// For consumers, the listen address (e.g., "0.0.0.0:8080"). For publishers, the target URL.
    pub url: String,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// (Consumer only) Number of worker threads to use. Defaults to 0 for unlimited.
    pub workers: Option<usize>,
    /// (Consumer only) Header key to extract the message ID from. Defaults to "message-id".
    pub message_id_header: Option<String>,
    /// (Consumer only) Timeout for request-reply operations in milliseconds. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
}

// --- IBM MQ Specific Configuration ---

/// Connection settings for the IBM MQ Queue Manager.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IbmMqConfig {
    /// Required. Connection URL in `host(port)` format. Supports comma-separated list for failover (e.g., `host1(1414),host2(1414)`).
    pub url: String,
    /// Target Queue name for point-to-point messaging. Optional if `topic` is set; defaults to route name if omitted.
    pub queue: Option<String>,
    /// Target Topic string for Publish/Subscribe. If set, enables **Subscriber mode** (Consumer) or publishes to a topic (Publisher). Optional if `queue` is set.
    pub topic: Option<String>,
    /// Required. Name of the Queue Manager to connect to (e.g., `QM1`).
    pub queue_manager: String,
    /// Required. Server Connection (SVRCONN) Channel name defined on the QM.
    pub channel: String,
    /// Username for authentication. Optional; required if the channel enforces authentication.
    pub user: Option<String>,
    /// Password for authentication. Optional; required if the channel enforces authentication.
    pub password: Option<String>,
    /// TLS CipherSpec (e.g., `ANY_TLS12`). Optional; required for encrypted connections.
    pub cipher_spec: Option<String>,
    /// TLS configuration settings (e.g., keystore paths). Optional.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Maximum message size in bytes (default: 4MB). Optional.
    #[serde(default = "default_max_message_size")]
    pub max_message_size: usize,
    /// (Consumer only) Polling timeout in milliseconds (default: 1000ms). Optional.
    #[serde(default = "default_wait_timeout_ms")]
    pub wait_timeout_ms: i32,
    /// Internal buffer size for the channel. Defaults to 100.
    #[serde(default)]
    pub internal_buffer_size: Option<usize>,
}

fn default_max_message_size() -> usize {
    4 * 1024 * 1024 // 4MB default
}

fn default_wait_timeout_ms() -> i32 {
    1000 // 1 second default
}

// --- Switch/Router Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    /// The metadata key to inspect for routing decisions.
    pub metadata_key: String,
    /// A map of values to endpoints.
    pub cases: HashMap<String, Endpoint>,
    /// The default endpoint if no case matches.
    pub default: Option<Box<Endpoint>>,
}

// --- Response Endpoint Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    // This struct is a marker and currently has no fields.
}

// --- Common Configuration ---

/// TLS configuration for secure connections.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// If true, enable TLS/SSL.
    pub required: bool,
    /// Path to the CA certificate file.
    pub ca_file: Option<String>,
    /// Path to the client certificate file (PEM).
    pub cert_file: Option<String>,
    /// Path to the client private key file (PEM).
    pub key_file: Option<String>,
    /// Password for the private key (if encrypted).
    pub cert_password: Option<String>,
    /// If true, disable server certificate verification (insecure).
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
      url: "localhost:9092"
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

        assert_eq!(route.options.concurrency, 10);

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
                Middleware::Custom { .. } => {}
                Middleware::Dlq(dlq) => {
                    assert!(dlq.endpoint.middlewares.is_empty());
                    if let EndpointType::Nats(nats_cfg) = &dlq.endpoint.endpoint_type {
                        assert_eq!(nats_cfg.subject, Some("dlq-subject".to_string()));
                        assert_eq!(nats_cfg.url, "nats://localhost:4222");
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
                Middleware::Delay(_) => {}
                Middleware::WeakJoin(_) => {}
            }
        }

        if let EndpointType::Kafka(kafka) = &input.endpoint_type {
            assert_eq!(kafka.topic, Some("input-topic".to_string()));
            assert_eq!(kafka.url, "localhost:9092");
            assert_eq!(kafka.group_id, Some("my-consumer-group".to_string()));
            let tls = &kafka.tls;
            assert!(tls.required);
            assert_eq!(tls.ca_file.as_deref(), Some("/path_to_ca"));
            assert!(tls.accept_invalid_certs);
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
            assert_eq!(nats.url, "nats://localhost:4222");
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
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__URL", "localhost:9092");
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
        assert_eq!(config.get("kafka_to_nats").unwrap().options.concurrency, 10);
        if let EndpointType::Kafka(k) = &config.get("kafka_to_nats").unwrap().input.endpoint_type {
            assert_eq!(k.topic, Some("input-topic".to_string()));
            assert!(k.tls.required);
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
}

#[cfg(all(test, feature = "schema"))]
mod schema_tests {
    use super::*;

    #[test]
    fn generate_json_schema() {
        let schema = schemars::schema_for!(Config);
        let schema_json = serde_json::to_string_pretty(&schema).unwrap();

        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("mq-bridge.schema.json");
        std::fs::write(path, schema_json).expect("Failed to write schema file");
    }
}
