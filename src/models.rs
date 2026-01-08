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
    traits::{CustomEndpointFactory, CustomMiddlewareFactory, Handler},
};

/// The top-level configuration is a map of named routes.
/// The key is the route name (e.g., "kafka_to_nats").
pub type Config = HashMap<String, Route>;

/// A configuration map for named publishers (endpoints).
/// The key is the publisher name.
pub type PublisherConfig = HashMap<String, Endpoint>;

/// Defines a single message processing route from an input to an output.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// (Optional) Number of concurrent processing tasks for this route. Defaults to 1.
    #[serde(default = "default_concurrency")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub concurrency: usize,
    /// (Optional) Number of messages to process in a single batch. Defaults to 1.
    #[serde(default = "default_batch_size")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
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
    1
}

fn default_output_endpoint() -> Endpoint {
    Endpoint::new(EndpointType::Null)
}

fn default_dlq_retry_attempts() -> usize {
    3
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

/// Represents a connection point for messages, which can be a source (input) or a sink (output).
#[derive(Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// (Optional) A list of middlewares to apply to the endpoint.
    #[serde(default)]
    pub middlewares: Vec<Middleware>,

    /// (input only) The processing mode for the endpoint.
    #[serde(default)]
    pub mode: ConsumerMode,

    /// The specific endpoint implementation, determined by the configuration key (e.g., "kafka", "nats").
    #[serde(flatten)]
    pub endpoint_type: EndpointType,

    #[serde(skip_serializing)]
    #[cfg_attr(feature = "schema", schemars(skip))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum EndpointType {
    #[cfg(feature = "aws")]
    Aws(AwsEndpoint),
    Kafka(KafkaEndpoint),
    Nats(NatsEndpoint),
    File(String),
    Static(String),
    Memory(MemoryConfig),
    Amqp(AmqpEndpoint),
    MongoDb(MongoDbEndpoint),
    Mqtt(MqttEndpoint),
    IbmMq(IbmMqEndpoint),
    Http(HttpEndpoint),
    ZeroMq(ZeroMqEndpoint),
    Fanout(Vec<Endpoint>),
    Switch(SwitchConfig),
    Response(ResponseConfig),
    #[serde(skip)]
    Custom(Arc<dyn CustomEndpointFactory>),
    Null,
}

/// An enumeration of all supported middleware types.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Middleware {
    Deduplication(DeduplicationMiddleware),
    Metrics(MetricsMiddleware),
    Dlq(Box<DeadLetterQueueMiddleware>),
    CommitConcurrency(CommitConcurrencyMiddleware),
    Retry(RetryMiddleware),
    RandomPanic(RandomPanicMiddleware),
    Delay(DelayMiddleware),
    #[serde(skip)]
    Custom(Arc<dyn CustomMiddlewareFactory>),
}

/// Deduplication middleware configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    pub sled_path: String,
    pub ttl_seconds: u64,
}

/// Configuration for limiting the number of parallel commit tasks of publishers.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CommitConcurrencyMiddleware {
    pub limit: usize,
}

/// Metrics middleware configuration. It's currently a struct without fields
/// but can be extended later. Its presence in the config enables the middleware.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MetricsMiddleware {}

/// Dead-Letter Queue (DLQ) middleware configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DelayMiddleware {
    pub delay_ms: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

// --- AWS Specific Configuration ---

#[cfg(feature = "aws")]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AwsEndpoint {
    pub queue_url: Option<String>,
    pub topic_arn: Option<String>,
    #[serde(flatten)]
    pub config: AwsConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    pub max_messages: Option<i32>,
    pub wait_time_seconds: Option<i32>,
}

// --- Kafka Specific Configuration ---

/// Kafka endpoint configuration, combining connection and topic details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct KafkaEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: KafkaConfig,
}

/// General Kafka connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct KafkaConfig {
    // pub url: String // use "pub brokers: String" here.
    /// Comma-separated list of Kafka broker URLs. Can also be specified using the alias 'url'.
    #[serde(alias = "url")]
    pub brokers: String,
    /// Optional username for SASL authentication.
    pub username: Option<String>,
    /// Optional password for SASL authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Consumer group ID. Required for consumers.
    pub group_id: Option<String>,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// Additional librdkafka producer configuration options (key-value pairs).
    #[serde(default)]
    pub producer_options: Option<Vec<(String, String)>>,
    /// Additional librdkafka consumer configuration options (key-value pairs).
    #[serde(default)]
    pub consumer_options: Option<Vec<(String, String)>>,
}

// --- NATS Specific Configuration ---

/// NATS endpoint configuration, combining connection and subject details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NatsEndpoint {
    pub subject: Option<String>,
    pub stream: Option<String>,
    #[serde(flatten)]
    pub config: NatsConfig,
}

/// General NATS connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NatsConfig {
    /// Comma-separated list of NATS server URLs (e.g., "nats://localhost:4222,nats://localhost:4223").
    pub url: String,
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
    /// It sends a request and waits for a response (using `core_client.request_with_headers()`)
    /// with timeout handling. Defaults to false.
    #[serde(default)]
    pub request_reply: bool,
    /// Timeout for request-reply operations in milliseconds. Defaults to 2000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// If no_jetstream: true, use Core NATS (fire-and-forget) instead of JetStream. Defaults to false.
    #[serde(default)]
    pub no_jetstream: bool,
    /// The default stream name to use if not specified in the endpoint configuration.
    pub default_stream: Option<String>,
    /// Maximum number of messages in the stream (if created by the bridge). Defaults to 1,000,000.
    pub stream_max_messages: Option<i64>,
    /// Maximum total bytes in the stream (if created by the bridge). Defaults to 1GB.
    pub stream_max_bytes: Option<i64>,
    /// Number of messages to prefetch from the consumer. Defaults to 10000.
    pub prefetch_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ConsumerMode {
    #[default]
    Consume,
    Subscribe,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub topic: String,
    pub capacity: Option<usize>,
}

// --- AMQP Specific Configuration ---

/// AMQP endpoint configuration, combining connection and queue details.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmqpEndpoint {
    pub queue: Option<String>,
    #[serde(flatten)]
    pub config: AmqpConfig,
}

/// General AMQP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmqpConfig {
    /// AMQP connection URI. The `lapin` client connects to a single host specified in the URI.
    /// For high availability, provide the address of a load balancer or use DNS resolution
    /// that points to multiple brokers. Example: "amqp://localhost:5672/vhost".
    pub url: String,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// The exchange to publish to or bind the queue to.
    pub exchange: Option<String>,
    /// Number of messages to prefetch. Defaults to 100.
    pub prefetch_count: Option<u16>,
    /// If true, declare queues as non-durable (transient). Defaults to false.
    #[serde(default)]
    pub no_persistence: bool,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
}

// --- MongoDB Specific Configuration ---

/// MongoDB endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MongoDbEndpoint {
    pub collection: Option<String>,
    #[serde(flatten)]
    pub config: MongoDbConfig,
}

/// General MongoDB connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MongoDbConfig {
    /// MongoDB connection string URI. Can contain a comma-separated list of hosts for a replica set.
    /// Credentials provided via the separate `username` and `password` fields take precedence over any credentials embedded in the URL.
    pub url: String,
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
    /// Polling interval in milliseconds for the consumer (when not using Change Streams). Defaults to 100ms.
    pub polling_interval_ms: Option<u64>,
    /// TTL in seconds for documents created by the publisher. If set, a TTL index is created.
    pub ttl_seconds: Option<u64>,
}

// --- MQTT Specific Configuration ---

/// MQTT endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MqttEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: MqttConfig,
}

/// General MQTT connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    /// MQTT broker URL (e.g., "tcp://localhost:1883"). Does not support multiple hosts.
    pub url: String,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Capacity of the internal channel for incoming messages. Defaults to 100.
    pub queue_capacity: Option<usize>,
    /// Maximum number of inflight messages.
    pub max_inflight: Option<u16>,
    /// Quality of Service level (0, 1, or 2). Defaults to 1.
    pub qos: Option<u8>,
    /// If true, start with a clean session. Defaults to false (persistent session).
    #[serde(default = "default_clean_session")]
    pub clean_session: bool,
    /// Keep-alive interval in seconds. Defaults to 20.
    pub keep_alive_seconds: Option<u64>,
    /// MQTT protocol version (V3 or V5). Defaults to V5.
    #[serde(default)]
    pub protocol: MqttProtocol,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum MqttProtocol {
    #[default]
    V5,
    V3,
}

// --- IBM MQ Specific Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IbmMqEndpoint {
    pub queue: Option<String>,
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: IbmMqConfig,
}

/// General IBM MQ connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IbmMqConfig {
    /// Comma-separated list of IBM MQ connection names (e.g., "localhost(1414),otherhost(1414)").
    pub connection_name: String,
    /// The queue manager name.
    pub queue_manager: String,
    /// The channel name.
    pub channel: String,
    /// Optional username for authentication.
    pub user: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// Cipher spec for TLS connection.
    pub cipher_spec: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
}

// --- ZeroMQ Specific Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ZeroMqEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: ZeroMqConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ZeroMqConfig {
    /// The ZeroMQ URL (e.g., "tcp://127.0.0.1:5555").
    pub url: String,
    /// The socket type (PUSH, PULL, PUB, SUB, REQ, REP).
    #[serde(default)]
    pub socket_type: Option<ZeroMqSocketType>,
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

/// HTTP endpoint configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct HttpEndpoint {
    #[serde(flatten)]
    pub config: HttpConfig,
}

/// General HTTP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// For consumers, the listen address (e.g., "0.0.0.0:8080"). For publishers, the target URL.
    pub url: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// (Consumer only) Optional endpoint to send the response to.
    pub response_out: Option<Box<Endpoint>>,
}

// --- Switch/Router Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    pub metadata_key: String,
    pub cases: HashMap<String, Endpoint>,
    pub default: Option<Box<Endpoint>>,
}

// --- Response Endpoint Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {}

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
                Middleware::Custom(_) => {}
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
                Middleware::CommitConcurrency(_) => {}
                Middleware::Delay(_) => {}
            }
        }

        if let EndpointType::Kafka(kafka) = &input.endpoint_type {
            assert_eq!(kafka.topic, Some("input-topic".to_string()));
            assert_eq!(kafka.config.brokers, "localhost:9092");
            assert_eq!(kafka.config.group_id, Some("my-consumer-group".to_string()));
            let tls = &kafka.config.tls;
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
