//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicUsize, Arc},
};

use crate::traits::{Handler, StreamingHandler};
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
///
/// These options control concurrency, batching, and commit behavior for message processing.
///
/// # Examples
///
/// ```
/// use mq_bridge::models::RouteOptions;
///
/// let options = RouteOptions {
///     description: "My Route".to_string(),
///     concurrency: 10,
///     batch_size: 5,
///     commit_concurrency_limit: 1024,
/// };
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteOptions {
    /// A human-readable description of the route's purpose. Defaults to an empty string.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
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
            description: String::new(),
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
            | "sled"
            | "amqp"
            | "mongodb"
            | "mqtt"
            | "http"
            | "ibmmq"
            | "zeromq"
            | "grpc"
            | "fanout"
            | "ref"
            | "switch"
            | "response"
            | "sqlx"
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
                Ok(Endpoint {
                    middlewares: Vec::new(),
                    endpoint_type: EndpointType::Null,
                    handler: None,
                })
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
    Ref(String),
    Memory(MemoryConfig),
    Sled(SledConfig),
    Amqp(AmqpConfig),
    MongoDb(MongoDbConfig),
    Mqtt(MqttConfig),
    Http(HttpConfig),
    IbmMq(IbmMqConfig),
    ZeroMq(ZeroMqConfig),
    Grpc(GrpcConfig),
    Sqlx(SqlxConfig),
    Fanout(Vec<Endpoint>),
    Switch(SwitchConfig),
    Response(ResponseConfig),
    StreamingHandler(Box<StreamingHandlerConfig>),
    Reader(Box<Endpoint>),
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
            EndpointType::Ref(_) => "ref",
            EndpointType::Memory(_) => "memory",
            EndpointType::Sled(_) => "sled",
            EndpointType::Amqp(_) => "amqp",
            EndpointType::MongoDb(_) => "mongodb",
            EndpointType::Mqtt(_) => "mqtt",
            EndpointType::Http(_) => "http",
            EndpointType::IbmMq(_) => "ibmmq",
            EndpointType::ZeroMq(_) => "zeromq",
            EndpointType::Grpc(_) => "grpc",
            EndpointType::Sqlx(_) => "sqlx",
            EndpointType::Fanout(_) => "fanout",
            EndpointType::Switch(_) => "switch",
            EndpointType::Response(_) => "response",
            EndpointType::StreamingHandler(_) => "streaming_handler",
            EndpointType::Reader(_) => "reader",
            EndpointType::Custom { .. } => "custom",
            EndpointType::Null => "null",
        }
    }

    pub fn is_core(&self) -> bool {
        matches!(
            self,
            EndpointType::File(_)
                | EndpointType::Static(_)
                | EndpointType::Ref(_)
                | EndpointType::Memory(_)
                | EndpointType::Fanout(_)
                | EndpointType::Switch(_)
                | EndpointType::Response(_)
                | EndpointType::StreamingHandler(_)
                | EndpointType::Reader(_)
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
///
/// Prevents duplicate messages from being processed using a Sled-backed database.
/// Messages are identified by their deduplication key and removed after the TTL expires.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    /// Path to the Sled database directory.
    pub sled_path: String,
    /// Time-to-live for deduplication entries in seconds.
    pub ttl_seconds: u64,
}

/// Metrics middleware configuration.
///
/// Enables collection and reporting of message processing metrics such as throughput,
/// latency, and error rates. The presence of this middleware in the configuration
/// enables metrics collection for the endpoint.
///
/// Metrics are typically exported via Prometheus or similar monitoring systems.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MetricsMiddleware {}

/// Dead-Letter Queue (DLQ) middleware configuration.
///
/// Routes failed messages to a designated endpoint for later analysis and recovery.
/// It is recommended to pair this with the Retry middleware to avoid message loss.
///
/// Failed messages are sent to the configured endpoint when they are exhausted after retry attempts.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeadLetterQueueMiddleware {
    /// The endpoint to send failed messages to.
    pub endpoint: Endpoint,
}

/// Retry middleware configuration.
///
/// Implements exponential backoff retry logic for failed message processing.
/// Failed messages are automatically retried with increasing delays between attempts.
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

/// Delay middleware configuration.
///
/// Introduces a fixed delay before processing each message.
/// Useful for rate limiting, testing, or allowing time for dependent systems to become ready.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DelayMiddleware {
    /// Delay duration in milliseconds.
    pub delay_ms: u64,
}

/// Weak Join middleware configuration.
///
/// Groups and correlates messages based on a metadata key, waiting for a specified number
/// of messages within a timeout window before processing them as a batch.
/// Messages that exceed the timeout are processed individually.
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

/// Fault injection modes for testing error handling and recovery mechanisms.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FaultMode {
    /// Trigger a thread panic.
    #[default]
    Panic,
    /// Simulate a connection/network error (retryable).
    Disconnect,
    /// Simulate a timeout error (retryable).
    Timeout,
    /// Simulate a JSON format error (non-retryable).
    JsonFormatError,
    /// Return a negative acknowledgement (for handlers).
    Nack,
}

impl std::fmt::Display for FaultMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultMode::Panic => write!(f, "panic"),
            FaultMode::Disconnect => write!(f, "disconnect"),
            FaultMode::Timeout => write!(f, "timeout"),
            FaultMode::JsonFormatError => write!(f, "json_format_error"),
            FaultMode::Nack => write!(f, "nack"),
        }
    }
}

/// Middleware for fault injection testing.
///
/// Allows testing error handling and recovery mechanisms by injecting faults
/// at specific points in the message processing pipeline.
///
/// # Examples
///
/// ```yaml
/// random_panic:
///   mode: panic
///   trigger_on_message: 3  # Trigger on the 3rd message
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RandomPanicMiddleware {
    /// The type of fault to inject.
    #[serde(default)]
    pub mode: FaultMode,
    /// Trigger the fault on the Nth message (1-indexed). None = trigger on every message.
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    #[serde(default)]
    pub trigger_on_message: Option<usize>,
    /// Enable/disable the fault injection without removing the configuration.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(skip, default = "default_atomic_usize_arc")]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub message_count: Arc<AtomicUsize>,
}

fn default_true() -> bool {
    true
}

fn default_atomic_usize_arc() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn deserialize_null_as_false<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<bool>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(false))
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

impl AwsConfig {
    /// Creates a new AWS configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_queue_url(mut self, queue_url: impl Into<String>) -> Self {
        self.queue_url = Some(queue_url.into());
        self
    }

    pub fn with_topic_arn(mut self, topic_arn: impl Into<String>) -> Self {
        self.topic_arn = Some(topic_arn.into());
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url = Some(endpoint_url.into());
        self
    }

    pub fn with_credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        self.access_key = Some(access_key.into());
        self.secret_key = Some(secret_key.into());
        self
    }
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

impl KafkaConfig {
    /// Creates a new Kafka configuration with the specified broker URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_producer_option(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let options = self.producer_options.get_or_insert_with(Vec::new);
        options.push((key.into(), value.into()));
        self
    }

    pub fn with_consumer_option(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let options = self.consumer_options.get_or_insert_with(Vec::new);
        options.push((key.into(), value.into()));
        self
    }
}

// --- Sled Specific Configuration ---

/// General Sled database configuration
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SledConfig {
    /// Path to the Sled database directory.
    pub path: String,
    /// The tree name to use as a queue. Defaults to "default".
    pub tree: Option<String>,
    /// (Consumer only) If true, start reading from the beginning of the tree.
    #[serde(default)]
    pub read_from_start: bool,
    /// (Consumer only) If true, delete messages after processing (Queue mode).
    #[serde(default)]
    pub delete_after_read: bool,
}

impl SledConfig {
    /// Creates a new Sled configuration with the specified database path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn with_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = Some(tree.into());
        self
    }

    pub fn with_read_from_start(mut self, read_from_start: bool) -> Self {
        self.read_from_start = read_from_start;
        self
    }
}

/// Format for messages written to or read from a file.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    /// The full `CanonicalMessage` is serialized to JSON. Payload is a byte array.
    #[default]
    Normal,
    /// The full `CanonicalMessage` is serialized to JSON. Payload is rendered as a JSON value if possible.
    Json,
    /// The full `CanonicalMessage` is serialized to JSON. Payload is rendered as a string if possible.
    Text,
    /// The raw payload of the message is written. For consumers, the line is read as raw bytes.
    Raw,
}

// --- File Specific Configuration ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileConfig {
    /// Path to the file.
    pub path: String,
    /// Optional delimiter for messages. Defaults to newline ("\n").
    /// Can be a string or a hex sequence (e.g. "0x00").
    /// Currently only single-byte delimiters are supported.
    pub delimiter: Option<String>,
    /// The consumption mode. If not specified, defaults to `consume`.
    /// For publishers, this setting is ignored.
    #[serde(flatten, default)]
    pub mode: Option<FileConsumerMode>,
    /// The format for writing messages to the file (Publisher) or interpreting them (Consumer). Defaults to `normal`.
    #[serde(default)]
    pub format: FileFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FileConsumerMode {
    /// **Queue Mode**: Reads from the beginning of the file.
    Consume {
        #[serde(default)]
        delete: bool,
    },
    /// **Broadcast Mode**: Tails the file (starts at the end).
    Subscribe {
        #[serde(default)]
        delete: bool,
    },
    /// **Persistent Mode**: Reads the file with offset tracking.
    GroupSubscribe {
        /// The consumer group ID that is used for offset tracking. Should be unique.
        group_id: String,
        /// If true, starts reading from the end of the file if no offset is stored.
        /// If false, starts reading from the beginning.
        #[serde(default)]
        read_from_tail: bool,
    },
}

impl Default for FileConsumerMode {
    fn default() -> Self {
        Self::Consume { delete: false }
    }
}

impl FileConfig {
    /// Creates a new File configuration with the specified path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: Some(FileConsumerMode::default()),
            delimiter: None,
            format: FileFormat::default(),
        }
    }

    pub fn with_mode(mut self, mode: FileConsumerMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Returns the effective consumer mode, defaulting to `Consume` if not set.
    pub fn effective_mode(&self) -> FileConsumerMode {
        self.mode.clone().unwrap_or_default()
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

impl NatsConfig {
    /// Creates a new NATS configuration with the specified server URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn with_stream(mut self, stream: impl Into<String>) -> Self {
        self.stream = Some(stream.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
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

    pub fn with_request_reply(mut self, request_reply: bool) -> Self {
        self.request_reply = request_reply;
        self
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
    /// (Publisher only) If true, do not attempt to declare the queue. Assumes the queue already exists. Defaults to false.
    #[serde(default)]
    pub no_declare_queue: bool,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
}

impl AmqpConfig {
    /// Creates a new AMQP configuration with the specified connection URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    pub fn with_exchange(mut self, exchange: impl Into<String>) -> Self {
        self.exchange = Some(exchange.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// MongoDB message storage format.
///
/// Determines how messages are stored and retrieved from MongoDB collections.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum MongoDbFormat {
    #[default]
    Normal,
    Json,
    Text,
    Raw,
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
    /// Format for storing messages. Defaults to Normal.
    #[serde(default)]
    pub format: MongoDbFormat,
    /// The ID used for the cursor in sequenced mode. If not provided, consumption starts from the current sequence (ephemeral).
    pub cursor_id: Option<String>,
}

impl MongoDbConfig {
    /// Creates a new MongoDB configuration with the specified URL and database name.
    pub fn new(url: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            database: database.into(),
            ..Default::default()
        }
    }

    pub fn with_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_change_stream(mut self, change_stream: bool) -> Self {
        self.change_stream = change_stream;
        self
    }
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
    /// (Consumer only) If true, messages are acknowledged immediately upon receipt (auto-ack).
    /// If false (default), messages are acknowledged after processing (manual-ack).
    /// Note: This setting does not currently enable synchronous publishing (waiting for PubAck) for the MQTT publisher.
    #[serde(default)]
    pub delayed_ack: bool,
}

impl MqttConfig {
    /// Creates a new MQTT configuration with the specified broker URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// MQTT protocol version.
///
/// Specifies which version of the MQTT protocol to use for connections.
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

impl ZeroMqConfig {
    /// Creates a new ZeroMQ configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_socket_type(mut self, socket_type: ZeroMqSocketType) -> Self {
        self.socket_type = Some(socket_type);
        self
    }

    pub fn with_bind(mut self, bind: bool) -> Self {
        self.bind = bind;
        self
    }
}

/// ZeroMQ socket type.
///
/// Defines the messaging pattern for ZeroMQ connections.
/// Different patterns support different communication paradigms (request-reply, publish-subscribe, etc.).
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

// --- gRPC Specific Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GrpcConfig {
    /// The gRPC server URL (e.g., "http://localhost:50051").
    pub url: String,
    /// The topic to subscribe to.
    pub topic: Option<String>,
    /// Timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
}

impl GrpcConfig {
    /// Creates a new gRPC configuration with the specified server URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }
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
    /// Timeout for HTTP requests in milliseconds. For consumers, it's the request-reply timeout. For publishers, it's the timeout for each individual request. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Consumer only) Internal buffer size for the channel. Defaults to 100.
    pub internal_buffer_size: Option<usize>,
    /// (Consumer only) If true, respond immediately with 202 Accepted without waiting for downstream processing. Defaults to false.
    #[serde(default)]
    pub fire_and_forget: bool,
    /// (Publisher only) The number of concurrent HTTP requests to send in a batch. Defaults to 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_concurrency: Option<usize>,
    /// (Publisher only) TCP keepalive timeout for the underlying connection pool in milliseconds. Defaults to 60000ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_keepalive_ms: Option<u64>,
    /// (Publisher only) Timeout for idle connections in the connection pool in milliseconds. Defaults to 90000ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_idle_timeout_ms: Option<u64>,
    /// Enable gzip compression for request/response bodies exceeding the threshold. Defaults to false.
    #[serde(default)]
    pub compression_enabled: bool,
    /// Minimum message size in bytes to compress. Messages smaller than this are sent uncompressed. Defaults to 1024 bytes.
    #[serde(default)]
    pub compression_threshold_bytes: Option<usize>,
    /// HTTP Basic Authentication credentials (username, password). For consumers: validates incoming requests. For publishers: adds Authorization header.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_basic_auth"
    )]
    pub basic_auth: Option<(String, String)>,
    /// Custom headers as key-value pairs (e.g., {"X-API-Key": "token123"}). Added to outgoing HTTP headers for both consumers and publishers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom_headers: HashMap<String, String>,
}

fn deserialize_basic_auth<'de, D>(deserializer: D) -> Result<Option<(String, String)>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(arr) => {
            if arr.len() != 2 {
                return Err(serde::de::Error::custom("basic_auth must have 2 elements"));
            }
            let u = arr[0]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("basic_auth[0] must be string"))?
                .to_string();
            let p = arr[1]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("basic_auth[1] must be string"))?
                .to_string();
            Ok(Some((u, p)))
        }
        serde_json::Value::Object(map) => {
            let u = map
                .get("0")
                .and_then(|v| v.as_str())
                .ok_or_else(|| serde::de::Error::custom("basic_auth map missing '0'"))?
                .to_string();
            let p = map
                .get("1")
                .and_then(|v| v.as_str())
                .ok_or_else(|| serde::de::Error::custom("basic_auth map missing '1'"))?
                .to_string();
            Ok(Some((u, p)))
        }
        _ => Err(serde::de::Error::custom("invalid type for basic_auth")),
    }
}

impl HttpConfig {
    /// Creates a new HTTP configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = Some(workers);
        self
    }
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
    pub username: Option<String>,
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
    /// If false, attempt to open the queue with INQUIRE permissions to fetch queue depth for status checks. Defaults to false.
    #[serde(default)]
    pub disable_status_inq: bool,
}

impl IbmMqConfig {
    /// Creates a new IBM MQ configuration with the specified connection URL, queue manager, and channel.
    pub fn new(
        url: impl Into<String>,
        queue_manager: impl Into<String>,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            queue_manager: queue_manager.into(),
            channel: channel.into(),
            disable_status_inq: false,
            ..Default::default()
        }
    }

    pub fn with_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
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

/// Configuration for a Streaming Handler endpoint.
///
/// This endpoint allows a handler to yield multiple responses for a single input message.
/// The yielded messages are then sent to the `output` endpoint.
#[derive(Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StreamingHandlerConfig {
    /// The output endpoint where the yielded messages will be sent.
    /// This could be a `response` endpoint for HTTP streaming, or a `fanout` for multiple destinations.
    pub output: Endpoint,
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub handler: Option<Arc<dyn StreamingHandler>>,
}

impl std::fmt::Debug for StreamingHandlerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingHandlerConfig")
            .field("output", &self.output)
            .field(
                "handler",
                &if self.handler.is_some() {
                    "Some(<StreamingHandler>)"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

// --- Response Endpoint Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    // This struct is a marker and currently has no fields.
}

// --- SQLx Specific Configuration ---

/// General SQLx connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SqlxConfig {
    /// Database connection URL.
    pub url: String,
    /// Optional username. Takes precedence over any credentials embedded in the `url`.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional password. Takes precedence over any credentials embedded in the `url`.
    #[serde(default)]
    pub password: Option<String>,
    /// The table to interact with.
    pub table: String,
    /// (Publisher only) Optional. A custom SQL INSERT query. Use `?` as a placeholder for the payload.
    /// If not provided, a default `INSERT INTO {table} (payload) VALUES (?)` is used.
    pub insert_query: Option<String>,
    /// (Consumer only) Optional. A custom SQL SELECT query to fetch messages. This is only supported for PostgreSQL and Microsoft SQL Server.
    /// The query must include a placeholder for the batch size (`$1` for PostgreSQL, `@p1` for SQL Server).
    /// The bridge will bind the route's `batch_size` to this placeholder.
    pub select_query: Option<String>,
    /// (Consumer only) If true, delete messages after processing.
    #[serde(default)]
    pub delete_after_read: bool,
    /// (Publisher only) If true, automatically create the table and indexes if they don't exist. Defaults to false.
    #[serde(default)]
    pub auto_create_table: bool,
    /// (Consumer only) Polling interval in milliseconds. Defaults to 100ms.
    pub polling_interval_ms: Option<u64>,
    /// TLS configuration for the database connection.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Maximum number of connections in the pool. Defaults to 10.
    pub max_connections: Option<u32>,
    /// Minimum number of connections to keep in the pool. Defaults to 0.
    pub min_connections: Option<u32>,
    /// Timeout for acquiring a connection from the pool in milliseconds. Defaults to 30000ms.
    pub acquire_timeout_ms: Option<u64>,
    /// Maximum idle time for a connection in milliseconds. Defaults to 600000ms (10 minutes).
    pub idle_timeout_ms: Option<u64>,
    /// Maximum lifetime of a connection in milliseconds. Defaults to 1800000ms (30 minutes).
    pub max_lifetime_ms: Option<u64>,
}

// --- Common Configuration ---

/// TLS configuration for secure connections.
///
/// Configures Transport Layer Security (TLS/SSL) for encrypted communication.
/// Supports both client certificate (mutual TLS) and server certificate validation.
///
/// # Examples
///
/// ```
/// use mq_bridge::models::TlsConfig;
///
/// let tls = TlsConfig {
///     required: true,
///     ca_file: Some("/path/to/ca.pem".to_string()),
///     cert_file: Some("/path/to/cert.pem".to_string()),
///     key_file: Some("/path/to/key.pem".to_string()),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// If true, enable TLS/SSL.
    #[serde(default, deserialize_with = "deserialize_null_as_false")]
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
    /// Creates a new TLS configuration with default settings (TLS not required).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ca_file(mut self, ca_file: impl Into<String>) -> Self {
        self.ca_file = Some(ca_file.into());
        self.required = true;
        self
    }

    pub fn with_client_cert(
        mut self,
        cert_file: impl Into<String>,
        key_file: impl Into<String>,
    ) -> Self {
        self.cert_file = Some(cert_file.into());
        self.key_file = Some(key_file.into());
        self.required = true;
        self
    }

    pub fn with_insecure(mut self, accept_invalid_certs: bool) -> Self {
        self.accept_invalid_certs = accept_invalid_certs;
        self
    }

    /// Checks if mutual TLS (mTLS) client authentication is configured.
    pub fn is_mtls_client_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }

    /// Checks if TLS server certificate authentication is configured.
    pub fn is_tls_server_configured(&self) -> bool {
        self.cert_file.is_some() && self.key_file.is_some()
    }
}

/// Trait for extracting secrets from configuration structures.
pub trait SecretExtractor {
    /// Extracts secrets into the provided map using the given prefix, and clears them from self.
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>);
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
            EndpointType::IbmMq(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "IBMMQ"), secrets)
            }
            EndpointType::Sqlx(cfg) => {
                cfg.extract_secrets(&format!("{}__{}", prefix, "SQLX"), secrets)
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
                        &format!("{}__{}__{}", prefix, "SWITCH__CASES", key.to_uppercase()),
                        secrets,
                    );
                }
                if let Some(default) = &mut cfg.default {
                    default.extract_secrets(&format!("{}__{}", prefix, "SWITCH__DEFAULT"), secrets);
                }
            }
            EndpointType::StreamingHandler(cfg) => {
                cfg.output.extract_secrets(
                    &format!("{}__{}", prefix, "STREAMING_HANDLER__OUTPUT"),
                    secrets,
                );
            }
            EndpointType::Reader(ep) => {
                ep.extract_secrets(&format!("{}__{}", prefix, "READER"), secrets)
            }
            _ => {}
        }
    }
}

impl SecretExtractor for Middleware {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
        if let Middleware::Dlq(cfg) = self {
            cfg.endpoint
                .extract_secrets(&format!("{}__{}__{}", prefix, "DLQ", "ENDPOINT"), secrets);
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
    }
}

impl SecretExtractor for KafkaConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
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

impl SecretExtractor for MqttConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
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
        if let Some((u, p)) = self.basic_auth.take() {
            secrets.insert(format!("{}__{}__{}", prefix, "BASIC_AUTH", 0), u);
            secrets.insert(format!("{}__{}__{}", prefix, "BASIC_AUTH", 1), p);
        }
        self.tls
            .extract_secrets(&format!("{}__{}", prefix, "TLS"), secrets);
    }
}

impl SecretExtractor for IbmMqConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
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

impl SecretExtractor for GrpcConfig {
    fn extract_secrets(&mut self, prefix: &str, secrets: &mut HashMap<String, String>) {
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

/// Extracts sensitive values (passwords, keys, tokens) from the configuration
/// and returns them as a map of environment variables (key-value pairs).
/// The extracted fields in the configuration are set to `None`.
///
/// The keys in the returned map follow the `MQB__{ROUTE}__{ENDPOINT}__{FIELD}` pattern
/// compatible with the `config` crate's environment variable override mechanism.
pub fn extract_config_secrets(config: &mut Config) -> HashMap<String, String> {
    let mut secrets = HashMap::new();
    for (route_name, route) in config.iter_mut() {
        let prefix = format!("MQB__{}", route_name.to_uppercase());
        route.extract_secrets(&prefix, &mut secrets);
    }
    secrets
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
          mode: nack
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
      - dlq:
          endpoint:
            file:
              path: "error.out"
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
                    assert!(rp.mode == FaultMode::Nack);
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
        assert_eq!(output.middlewares.len(), 2);
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

    #[test]
    fn test_extract_secrets() {
        let mut config = Config::new();
        let mut route = Route::default();

        // Setup Kafka with secrets
        let mut kafka_config = KafkaConfig::new("localhost:9092");
        kafka_config.username = Some("user".to_string());
        kafka_config.password = Some("pass".to_string());
        kafka_config.tls.cert_password = Some("certpass".to_string());

        route.input = Endpoint {
            endpoint_type: EndpointType::Kafka(kafka_config),
            middlewares: vec![],
            handler: None,
        };

        // Setup HTTP with basic auth
        let mut http_config = HttpConfig::new("http://localhost");
        http_config.basic_auth = Some(("httpuser".to_string(), "httppass".to_string()));

        route.output = Endpoint {
            endpoint_type: EndpointType::Http(http_config),
            middlewares: vec![],
            handler: None,
        };

        config.insert("test_route".to_string(), route);

        let secrets = extract_config_secrets(&mut config);

        // Verify secrets extracted
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__USERNAME")
                .map(|s| s.as_str()),
            Some("user")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__PASSWORD")
                .map(|s| s.as_str()),
            Some("pass")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__TLS__CERT_PASSWORD")
                .map(|s| s.as_str()),
            Some("certpass")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__BASIC_AUTH__0")
                .map(|s| s.as_str()),
            Some("httpuser")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__BASIC_AUTH__1")
                .map(|s| s.as_str()),
            Some("httppass")
        );

        // Verify config cleared
        let route = config.get("test_route").unwrap();
        if let EndpointType::Kafka(k) = &route.input.endpoint_type {
            assert!(k.username.is_none());
            assert!(k.password.is_none());
            assert!(k.tls.cert_password.is_none());
        }
        if let EndpointType::Http(h) = &route.output.endpoint_type {
            assert!(h.basic_auth.is_none());
        }
    }

    #[test]
    fn test_file_config_inference() {
        let yaml = r#"
mode: group_subscribe
path: "/tmp/test"
group_id: "my_group"
"#;
        let config: FileConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.mode {
            Some(FileConsumerMode::GroupSubscribe { group_id, .. }) => {
                assert_eq!(group_id, "my_group")
            }
            _ => panic!("Expected GroupSubscribe"),
        }

        let yaml_queue = r#"
mode: consume
path: "/tmp/test"
"#;
        let config_queue: FileConfig = serde_yaml_ng::from_str(yaml_queue).unwrap();
        match config_queue.mode {
            Some(FileConsumerMode::Consume { delete }) => assert!(!delete),
            _ => panic!("Expected Consume"),
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
