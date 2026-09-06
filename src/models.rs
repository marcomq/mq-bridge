//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Configuration types: every endpoint config, middleware and option struct the
//! YAML/JSON/env layers deserialize into. Everything that is not a definition lives in a
//! submodule — defaults, hand-written serde impls, builder methods and secret extraction —
//! and is glob-imported back here, so the function names in `serde(default = "...")` and
//! `schemars(transform = ...)` attributes keep resolving in this module.

mod builders;
pub(crate) mod defaults;
mod secrets;
mod serde_support;
#[cfg(test)]
mod tests;

use defaults::*;
use serde_support::*;

pub use defaults::DEFAULT_KAFKA_PARTITIONS;
#[cfg(feature = "grpc")]
pub(crate) use secrets::decode_secret_map_key;
pub use secrets::{extract_config_secrets, SecretExtractor};

use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicUsize, Arc},
};

use crate::traits::Handler;
use tracing::trace;

#[cfg(feature = "filter")]
fn deserialize_filter_expression<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let expression = String::deserialize(deserializer)?;
    crate::middleware::filter::CompiledFilter::new(&expression)
        .map_err(|error| serde::de::Error::custom(format!("invalid filter expression: {error}")))?;
    Ok(expression)
}

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
///       - retry:
///           max_attempts: 5
///           initial_interval_ms: 200
///       - random_panic:
///           mode: nack
///       - dlq:
///           endpoint:
///             nats:
///               subject: "dlq-subject"
///               url: "nats://localhost:4222"
///     kafka:
///       topic: "input-topic"
///       url: "localhost:9092"
///       group_id: "my-consumer-group"
///       tls:
///         required: true
///         ca_file: "/path_to_ca"
///         cert_file: "/path_to_cert"
///         key_file: "/path_to_key"
///         cert_password: "password"
///         accept_invalid_certs: true
///   output:
///     middlewares:
///       - metrics: {}
///       - dlq:
///           endpoint:
///             file:
///               path: "error.out"
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
#[cfg_attr(feature = "schema", schemars(transform = route_schema_transform))]
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
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteOptions {
    /// A human-readable description of the route's purpose. Defaults to an empty string.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// (Optional) Number of concurrent processing tasks for this route. While it improves throughput for high-latency
    /// handlers, it adds synchronization overhead for ordered commits and may lead to out-of-order processing
    /// in the handler. Above 1, whole batches may also reach the sink out of source order (rows keep their order
    /// within a batch) unless the sink declares itself order-sensitive, as `file` does.
    /// Defaults to 1.
    #[serde(default = "default_concurrency")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub concurrency: usize,
    /// (Optional) Maximum number of messages to process in a single batch. The consumer waits for at least one message
    /// and then attempts to fetch more if available. Increasing this improves throughput but also increases
    /// the potential impact of a single batch processing failure. Defaults to 512.
    #[serde(default = "default_batch_size")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub batch_size: usize,
    /// (Optional) The maximum number of in-flight commit requests queued for ordered sequencing.
    /// Lower values apply backpressure earlier; higher values allow larger commit backlogs.
    /// Defaults to 4096.
    #[serde(default = "default_commit_concurrency_limit")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub commit_concurrency_limit: usize,
    /// Time to wait for a route to establish connections before startup fails. Defaults to 5000ms.
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    /// Time to wait before reconnecting after a transient route failure. Defaults to 5000ms.
    #[serde(default = "default_reconnect_interval_ms")]
    pub reconnect_interval_ms: u64,
    /// Delay after an empty receive batch to avoid hot polling. Set to 0 to only yield. Defaults to 10ms.
    #[serde(default = "default_empty_batch_delay_ms")]
    pub empty_batch_delay_ms: u64,
    /// Allows fault-injection middleware such as random_panic. Disabled by default.
    #[serde(default = "default_false", skip_serializing_if = "is_false")]
    #[cfg_attr(feature = "schema", schemars(default = "default_false"))]
    pub allow_fault_injection: bool,
    /// If true, the route exits gracefully once the source yields an empty batch
    /// (drain-then-exit). Off by default — routes normally poll indefinitely.
    /// A drain that keeps failing to reconnect gives up and fails rather than retrying forever.
    #[serde(default = "default_false", skip_serializing_if = "is_false")]
    #[cfg_attr(feature = "schema", schemars(default = "default_false"))]
    pub exit_on_empty: bool,
}

/// Represents a connection point for messages, which can be a source (input) or a sink (output).
#[derive(Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transform = endpoint_schema_transform))]
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

/// Configuration for the `static` endpoint.
///
/// Accepts either a bare string (the response body, JSON-encoded for backward
/// compatibility) or a map for full control:
///
/// ```yaml
/// # bare string  -> body is JSON-encoded ("Hello" comes back quoted)
/// static: "Hello, World!"
///
/// # map form -> raw body + custom metadata (HTTP maps metadata to headers)
/// static:
///   body: "Hello, World!"
///   raw: true
///   metadata:
///     content-type: "text/plain"
///     server: "mq-bridge"
/// ```
///
/// When `raw` is true the body is sent verbatim; otherwise it is JSON-encoded as
/// a string. Every entry in `metadata` is attached to the produced message; when
/// this endpoint feeds an HTTP response, those entries become response headers
/// (e.g. `content-type`), otherwise they are ordinary message metadata.
///
/// The `body` supports `${…}` placeholders (compiled once at startup): request
/// fields `${payload:a.b}` / `${metadata:key}` / `${message:id}`, generators
/// `${gen:uuid|now|timestamp|counter|random(1,100)}`, and `${env:VAR}`. When the
/// `content-type` metadata is a JSON type, interpolated request values are
/// JSON-escaped by default; append `| raw` to splice verbatim, and write `$${…}`
/// to emit a literal `${…}`. See [`crate::support::interpolation`] for the full reference.
#[derive(Debug, Clone, Default)]
pub struct StaticConfig {
    /// The static response body.
    pub body: String,
    /// Send the body verbatim instead of JSON-encoding it as a string.
    pub raw: bool,
    /// Extra metadata entries attached to the produced message.
    pub metadata: std::collections::HashMap<String, String>,
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
// Endpoint configs are intentionally stored inline to preserve the public construction API.
#[allow(clippy::large_enum_variant)]
pub enum EndpointType {
    Aws(AwsConfig),
    Kafka(KafkaConfig),
    Nats(NatsConfig),
    File(FileConfig),
    #[serde(rename = "dir_spool", alias = "spool", alias = "dirspool")]
    DirSpool(DirSpoolConfig),
    #[serde(rename = "object_store", alias = "objectstore", alias = "s3")]
    ObjectStore(ObjectStoreConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Static(StaticConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Ref(String),
    Memory(MemoryConfig),
    Sled(SledConfig),
    Amqp(AmqpConfig),
    MongoDb(MongoDbConfig),
    Mqtt(MqttConfig),
    Http(HttpConfig),
    WebSocket(WebSocketConfig),
    IbmMq(IbmMqConfig),
    ZeroMq(ZeroMqConfig),
    #[serde(rename = "redis_streams", alias = "redis")]
    RedisStreams(RedisStreamsConfig),
    Grpc(GrpcConfig),
    Sqlx(SqlxConfig),
    #[serde(rename = "clickhouse", alias = "click_house")]
    ClickHouse(ClickHouseConfig),
    #[serde(rename = "postgres_cdc", alias = "postgres-cdc")]
    PostgresCdc(PostgresCdcConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Fanout(Vec<Endpoint>),
    #[serde(rename = "stream_buffer")]
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    StreamBuffer(StreamBufferConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Switch(SwitchConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Response(ResponseConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Reader(Box<Endpoint>),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Request(RequestForwardConfig),
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Custom {
        name: String,
        config: serde_json::Value,
    },
    #[default]
    #[cfg_attr(feature = "schema", schemars(extend("format" = "structural_endpoint")))]
    Null,
}

/// AEAD cipher selection for [`EncryptionConfig`].
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CipherKind {
    /// XChaCha20-Poly1305 (default): 192-bit random nonce, safe at high message rates.
    #[default]
    Xchacha20poly1305,
    /// AES-256-GCM: 96-bit counter nonce, for interoperability with other systems.
    Aes256gcm,
}

/// AEAD encryption settings, shared by the `encryption` middleware (per-message
/// payload encryption) and the at-rest `encryption` field of the file and
/// object_store endpoints. Requires the `encryption` feature.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EncryptionConfig {
    /// AEAD cipher. Defaults to `xchacha20poly1305`.
    #[serde(default)]
    pub cipher: CipherKind,
    /// Key identifier written into each envelope; selects the key when decrypting. Defaults to `default`.
    #[serde(default = "default_encryption_key_id")]
    pub key_id: String,
    /// Base64-encoded 32-byte key. Supports `${env:VAR}` to read it from the environment.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub key: String,
    /// Extra `key_id -> base64 key` entries accepted when decrypting (key rotation).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[cfg_attr(feature = "schema", schemars(extend("format" = "password")))]
    pub decrypt_keys: HashMap<String, String>,
    /// Metadata keys bound into the AEAD tag; changing one then fails decryption. Middleware only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authenticate_metadata: Vec<String>,
}

/// An enumeration of all supported middleware types.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Middleware {
    /// Renders a template into the `mqb.id` metadata key, e.g. `id: "${payload:order_id}"`.
    /// Consumer-only; list it *after* anything that reads `mqb.id`, since the last consumer
    /// middleware in the list runs first.
    Id(String),
    Deduplication(DeduplicationMiddleware),
    Metrics(MetricsMiddleware),
    Dlq(Box<DeadLetterQueueMiddleware>),
    Retry(RetryMiddleware),
    RandomPanic(RandomPanicMiddleware),
    Delay(DelayMiddleware),
    WeakJoin(WeakJoinMiddleware),
    Limiter(LimiterMiddleware),
    Buffer(BufferMiddleware),
    CookieJar(CookieJarMiddleware),
    Transform(TransformMiddleware),
    Encryption(EncryptionConfig),
    Compression(CompressionMiddleware),
    /// Keeps only messages matching an expression, e.g. `filter: "amount > 100"`.
    /// Reads payload fields by name and metadata as `meta.<key>`. Input and output.
    Filter(
        #[cfg_attr(
            feature = "filter",
            serde(deserialize_with = "deserialize_filter_expression")
        )]
        String,
    ),
    Custom {
        name: String,
        config: serde_json::Value,
    },
}

/// Deduplication middleware configuration.
///
/// Prevents duplicate messages from being processed using a sled, MongoDB, or SQL backend.
/// Messages are identified by their deduplication key and removed after the TTL expires.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    /// Store URL: `sled:///path` (local), `mongodb://host/db[/collection]`, or `postgres|mysql|mariadb|sqlite://…[/table]` (shared).
    #[serde(default)]
    pub store: Option<String>,
    /// Local Sled directory (legacy). Prefer `store`.
    #[serde(default)]
    pub sled_path: Option<String>,
    /// Time-to-live for deduplication entries in seconds.
    pub ttl_seconds: u64,
    /// Dedup key template, e.g. `${payload:order_id}`. Defaults to `message_id`.
    #[serde(default)]
    pub key: Option<String>,
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

/// Throughput limiter middleware configuration.
///
/// Applies a best-effort pacing delay so an endpoint does not exceed the configured
/// message rate. For batch operations the limiter accounts for the number of messages
/// in the batch, not just the batch count.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LimiterMiddleware {
    /// Target throughput in messages per second. Must be greater than zero.
    pub messages_per_second: f64,
}

/// Publisher-side buffer middleware configuration.
///
/// Buffers outbound messages briefly so multiple single-message sends can be
/// forwarded as one `send_batch` call to the wrapped publisher.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BufferMiddleware {
    /// Maximum number of messages to accumulate before flushing immediately.
    pub max_messages: usize,
    /// Maximum time to wait before flushing a non-full buffer.
    pub max_delay_ms: u64,
}

/// Cookie/session jar middleware configuration.
///
/// Optimized for HTTP by default: it can read `cookie` and `set-cookie` metadata,
/// persist session cookies, and inject them into later outgoing requests.
///
/// The middleware can also capture arbitrary metadata values into the same session store
/// and optionally expose stored values back into message metadata.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CookieJarMiddleware {
    /// Optional shared scope name. When set, middleware instances using the same scope
    /// share one session store across endpoints/routes in the process.
    #[serde(default)]
    pub shared_scope: Option<String>,
    /// Metadata key used to read/write HTTP Cookie headers. Defaults to `cookie`.
    #[serde(default = "default_cookie_metadata_key")]
    pub cookie_metadata_key: String,
    /// Metadata key used to read HTTP Set-Cookie responses. Defaults to `set-cookie`.
    #[serde(default = "default_set_cookie_metadata_key")]
    pub set_cookie_metadata_key: String,
    /// Additional metadata keys to persist into the session value store.
    #[serde(default)]
    pub capture_metadata_keys: Vec<String>,
    /// Optional metadata prefix used to export stored values back onto each message.
    ///
    /// Exported keys use `PREFIXcookie.<name>` for cookies and `PREFIXvalue.<name>` for
    /// captured generic values.
    #[serde(default)]
    pub export_metadata_prefix: Option<String>,
    /// Optional mapping of outgoing metadata keys to stored session value names.
    ///
    /// Example: `{ "authorization": "access_token" }` copies the stored value
    /// `access_token` into outgoing metadata key `authorization` when not already present.
    #[serde(default)]
    pub inject_metadata: HashMap<String, String>,
    /// Maximum cookies kept per session; the least recently set are dropped. Defaults to 256.
    #[serde(default = "default_max_cookies")]
    pub max_cookies: usize,
}

/// Weak Join middleware configuration.
///
/// Correlates messages by a metadata key and joins them within a timeout window.
/// Count mode (default) waits for `expected_count` messages and emits a JSON array.
/// Branch mode (set `branch_by`) waits for named branches and emits a branch-keyed object.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WeakJoinMiddleware {
    /// The metadata key to group messages by (e.g., "correlation_id").
    pub group_by: String,
    /// The number of messages (count mode) or distinct branches (branch mode) to wait for.
    pub expected_count: usize,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
    /// Metadata key naming each message's branch; enables branch mode when set.
    #[serde(default)]
    pub branch_by: Option<String>,
    /// Branch names that must all arrive before firing (branch mode; overrides expected_count).
    #[serde(default)]
    pub required: Vec<String>,
    /// What to do with an incomplete group when the timeout expires.
    #[serde(default)]
    pub on_timeout: WeakJoinTimeout,
}

/// Action taken on an incomplete weak-join group when its timeout expires.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WeakJoinTimeout {
    /// Emit the partial join (current behavior).
    #[default]
    Fire,
    /// Drop the incomplete group without emitting.
    Discard,
}

/// JSON transform middleware configuration.
///
/// Applies `mapping`, a Zen `expression`, then an optional `schema`.
///
/// On an output endpoint a rejected message becomes a non-retryable failure, so a `dlq`
/// middleware listed *after* this one captures it (publisher middlewares are wrapped in
/// list order, so the last entry is the outermost layer). On an input endpoint a rejected
/// message is dropped from the batch and acknowledged, which is how invalid input is kept
/// out of the route.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transform = transform_middleware_schema_transform))]
#[serde(deny_unknown_fields)]
pub struct TransformMiddleware {
    /// Output field name -> source path (e.g. `firstName: "$.first_name"`). Dots nest the output.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mapping: HashMap<String, MappingRule>,
    /// Zen Expression whose result replaces the mapped payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Inline JSON Schema subset (type, properties, required, default, items, nullable, enum).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Path to a JSON Schema file. Read once at startup; never re-read per message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_file: Option<String>,
    /// Coerce safely convertible types (e.g. `"42"` -> `42`) instead of rejecting. Defaults to true.
    #[serde(default = "default_true")]
    pub coerce: bool,
    /// Insert `default` values from the schema for missing fields. Defaults to true.
    #[serde(default = "default_true")]
    pub apply_defaults: bool,
    /// Read an empty string as `null`, so a nullable field or its default wins. Defaults to false.
    #[serde(default)]
    pub coerce_empty_as_null: bool,
    /// What to do with a message that fails to transform. Defaults to `reject`.
    #[serde(default)]
    pub on_error: TransformErrorPolicy,
}

/// How one output field is produced from the input document.
///
/// Either a bare path string (`"$.first_name"`) or an object with a `path` plus an
/// optional `default` and `required` flag.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum MappingRule {
    /// Shorthand: just the source path.
    Path(String),
    /// Full form with a fallback value and/or a presence requirement.
    Detailed(DetailedMappingRule),
}

/// Full mapping form with a fallback value and/or a presence requirement.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DetailedMappingRule {
    /// Source path in the input document (e.g. `$.user.id`, `user.id`, `$.items[0]`).
    pub path: String,
    /// Value used when the source path is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Reject the message when the source path is absent and no `default` is set.
    #[serde(default)]
    pub required: bool,
}

/// Action taken on a message that fails to transform.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TransformErrorPolicy {
    /// Reject the message: non-retryable failure on output, dropped from the batch on input.
    #[default]
    Reject,
    /// Forward the original payload unchanged with the error recorded in metadata.
    PassThrough,
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

// --- AWS Specific Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    /// The SQS queue URL. Required for Consumer. Optional for Publisher if `topic_arn` is set. If it contains userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub queue_url: Option<String>,
    /// (Publisher only) The SNS topic ARN.
    pub topic_arn: Option<String>,
    /// AWS Region (e.g., "us-east-1").
    pub region: Option<String>,
    /// Custom endpoint URL (e.g., for LocalStack).
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub endpoint_url: Option<String>,
    /// AWS Access Key ID.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub access_key: Option<String>,
    /// AWS Secret Access Key.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub secret_key: Option<String>,
    /// AWS Session Token.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub session_token: Option<String>,
    /// (Consumer only) Maximum number of messages to receive in a batch (1-10).
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 10)))]
    pub max_messages: Option<i32>,
    /// (Consumer only) Wait time for long polling in seconds (0-20).
    #[cfg_attr(feature = "schema", schemars(range(min = 0, max = 20)))]
    pub wait_time_seconds: Option<i32>,
    /// Use binary payloads in SQS/SNS messages.
    #[serde(default)]
    pub binary_payload_mode: bool,
}

// --- Kafka Specific Configuration ---

/// General Kafka connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct KafkaConfig {
    /// Comma-separated list of Kafka broker URLs. If it contains userinfo, it will be treated as a secret.
    #[serde(alias = "brokers")]
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The Kafka topic to produce to or consume from.
    pub topic: Option<String>,
    /// Optional username for SASL authentication.
    pub username: Option<String>,
    /// Optional password for SASL authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// (Consumer only) Consumer group ID.
    /// If not provided, the consumer acts in **Subscriber mode**: it generates a unique, ephemeral group ID and starts consuming from the latest offset.
    pub group_id: Option<String>,
    /// (Consumer only) Include authoritative `mqb.src.kafka_*` source positions. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// (Publisher only) If true, do not wait for an acknowledgement when sending to broker. Defaults to false.
    #[serde(default)]
    pub delayed_ack: bool,
    /// (Publisher only) Additional librdkafka producer configuration options (key-value pairs).
    #[serde(default)]
    pub producer_options: Option<Vec<(String, String)>>,
    /// (Consumer only) Additional librdkafka consumer configuration options (key-value pairs).
    #[serde(default)]
    pub consumer_options: Option<Vec<(String, String)>>,
    /// (Publisher only) Share one producer per connection (default: true); false gives a dedicated producer.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
    /// (Publisher only) Partition count used when auto-creating the topic (default: 6).
    /// Higher values raise write/consume parallelism; ordering is only guaranteed per
    /// partition key (message_id), not across the whole topic. Ignored if the topic exists.
    #[serde(default)]
    #[cfg_attr(
        feature = "schema",
        schemars(default = "default_kafka_partitions_schema", range(min = 1))
    )]
    pub partitions: Option<i32>,
    /// (Publisher only) Name of a metadata field whose value is used as the Kafka record
    /// key (drives partitioning/ordering). Unset, or absent on a given message, falls back
    /// to the message id. Default unset.
    #[serde(default)]
    pub partition_key: Option<String>,
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

/// Format for messages written to or read from a file.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    /// The full `CanonicalMessage` is serialized to JSON. Payload is either base64 or utf8 text.
    #[default]
    Normal,
    /// The full `CanonicalMessage` is serialized to JSON. Payload is rendered as a JSON value if possible.
    Json,
    /// The full `CanonicalMessage` is serialized to JSON. Payload is rendered as a string if possible.
    Text,
    /// The raw payload of the message is written. For consumers, the line is read as raw bytes.
    Raw,
    /// CSV rows mapped to/from JSON objects (string values only). The first row is the header/schema.
    Csv,
}

/// Compression algorithm. Used for at-rest batches (file, object_store) and for HTTP
/// body compression (http, clickhouse). Orthogonal to `format`.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    /// No compression (default).
    #[default]
    None,
    /// gzip; each batch is a self-contained member, so files stay readable with `zcat`.
    Gzip,
    /// lz4 frame format; each batch is a self-contained frame (`lz4 -d` compatible).
    Lz4,
    /// zstd; each batch is a self-contained frame, concatenated frames decode as one
    /// stream (`zstd -d` compatible). Better ratio than lz4, still fast.
    Zstd,
}

/// Payload-compression middleware configuration.
///
/// Compresses each message payload on the output side and decompresses it on the input
/// side; metadata and routing keys are left untouched. Requires the `compression` feature.
/// Distinct from the `file`/`object_store` batch `compression` field, which stays
/// CLI-decodable at rest — this operates per message on any transport.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CompressionMiddleware {
    /// Algorithm: `none`, `gzip`, `lz4`, or `zstd`. Defaults to `zstd`.
    #[serde(default = "default_compression_algorithm")]
    pub algorithm: Compression,
    /// Reject a decompressed payload larger than this many bytes (decompression-bomb guard).
    /// Consumer side only; unset means no limit.
    #[serde(default)]
    pub max_decompressed_bytes: Option<u64>,
}

// --- Sink object / part naming ---

/// How a `file` or `object_store` sink names what it writes.
///
/// The name decides everything downstream: a name derived from the source position sorts by
/// source order and repeats exactly on a replay (so a re-write is a no-op), while a name
/// derived from the write time sorts by write order and is unique per write.
///
/// `auto` resolves against the input for `object_store` only. There the two schemes write the
/// same thing under a different name, so the choice is free; on `file` they are different sink
/// structures — one appended file versus a directory of part files — and deriving that from the
/// input would change what `path` means without the operator asking for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum NameBy {
    /// object_store: `source_position` when the input carries one, else `write_time`. file: always `write_time`.
    #[default]
    Auto,
    /// `part-<source>-<start>-<end>.<ext>`. Needs a replayable input; on `file` this turns `path` into a directory.
    SourcePosition,
    /// object_store: `<uuidv7>.<ext>` under an optional `YYYY/MM/DD/` prefix. file: appends to `path`.
    WriteTime,
}

impl NameBy {
    /// Resolves `Auto` against the input's ability to stamp a replay position. Explicit
    /// values pass through, so an unsupported source still fails where it is configured.
    pub fn resolve(self, source_has_position: bool) -> NameBy {
        match self {
            NameBy::Auto if source_has_position => NameBy::SourcePosition,
            NameBy::Auto => NameBy::WriteTime,
            explicit => explicit,
        }
    }

    /// Folds the deprecated `idempotency` alias in. An explicit `name_by` wins; the alias
    /// only decides while `name_by` is still `auto`.
    pub(crate) fn or_idempotency(self, idempotency: Option<bool>) -> NameBy {
        match (self, idempotency) {
            (NameBy::Auto, Some(true)) => NameBy::SourcePosition,
            (NameBy::Auto, Some(false)) => NameBy::WriteTime,
            (configured, _) => configured,
        }
    }
}

// --- File Specific Configuration ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileConfig {
    /// Path to the file, or to the directory holding the part files under `source_position` naming.
    pub path: String,
    /// (Sink only) `write_time` (default here, appends to `path`) or `source_position` (replay-safe part files, `path` is their directory).
    #[serde(default)]
    pub name_by: NameBy,
    /// Deprecated: use `name_by`. true = `source_position`, false = `write_time`; ignored when `name_by` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<bool>,
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
    /// Per-batch compression (`none`, `gzip`, `lz4`, `zstd`). Requires the `compression` feature. Publishers: always. Consumers: must match, and only the default `consume` mode reads it.
    #[serde(default)]
    pub compression: Compression,
    /// At-rest AEAD encryption applied after compression. Requires the `encryption` feature. Publishers: always. Consumers: must match, and only the default `consume` mode reads it.
    #[serde(default)]
    pub encryption: Option<EncryptionConfig>,
    /// (Consumer only) Include authoritative `mqb.src.file_*` source positions; only `consume` mode reproduces them across restarts. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FileConsumerMode {
    /// **Queue Mode**: Standard point-to-point consumption. Reads from the start
    /// of the file. If `delete` is true, processed lines are physically removed
    /// from the file once they are successfully acknowledged.
    Consume {
        /// If true, processed lines are physically removed from the file once
        /// they are successfully acknowledged.
        #[serde(default)]
        delete: bool,
    },
    /// **Broadcast Mode**: Pub-sub style consumption. Tails the file by starting
    /// at the current end. If `delete` is true, lines are removed only after
    /// all local application subscribers for this specific file have acknowledged them.
    Subscribe {
        /// If true, lines are removed only after all local application
        /// subscribers for this file have acknowledged them.
        #[serde(default)]
        delete: bool,
    },
    /// **Persistent Mode**: Consumption with external offset tracking.
    /// Saves the last read byte position to a `.offset` file identified by the `group_id`.
    /// This allows the consumer to resume exactly where it left off after a restart
    /// without deleting data or requiring the bridge to stay running.
    GroupSubscribe {
        /// The consumer group ID that is used for offset tracking. Should be unique.
        group_id: String,
        /// If true, starts reading from the end of the file if no offset is stored.
        /// If false, starts reading from the beginning.
        #[serde(default)]
        read_from_tail: bool,
    },
}

// --- Directory Spool Specific Configuration ---

/// Configuration for a `dir_spool` endpoint: a crash-safe FIFO queue backed by a directory.
///
/// As a **sink** each message becomes a pair of files under `path` — a payload file holding
/// the raw `CanonicalMessage.payload` bytes, and an optional JSON sidecar holding its
/// metadata. Both are written to a `.tmp` name, fsynced, and renamed into place, so a reader
/// never observes a partial write. As a **source** the directory is listed in lexical order,
/// each finalized pair is emitted as one message, and (with `drain_on_read`) the pair is
/// deleted once the message is acknowledged.
///
/// The names are what makes the queue ordered, so `naming_pattern` must keep the sequence
/// number zero-padded and leading. The publisher resumes the sequence from the highest
/// number already present in the directory, so a restart appends rather than overwrites.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DirSpoolConfig {
    /// Directory holding the spool. Created if missing.
    pub path: String,
    /// (Sink only) Name template for each chunk, without extension. Supports `{seq}`,
    /// `{seq:06}` / `{seq:06d}` (zero-padded), `{timestamp}` (unix millis) and
    /// `{message_id}`. Defaults to `{seq:09}`. Lexical order must match sequence order,
    /// so keep a zero-padded `{seq}` first.
    #[serde(default = "default_spool_naming_pattern")]
    pub naming_pattern: String,
    /// How many levels of shard subdirectory to spread chunks over. Defaults to 0, which
    /// writes every chunk straight into `path`.
    ///
    /// A flat spool is one directory per stream, and most filesystems degrade long before
    /// mq-bridge does: 30fps with sidecars is over 200,000 files an hour. Sharding takes the
    /// leading digits of the sequence number as directory names, so `{seq:09}` with a depth
    /// of 2 and a width of 3 writes chunk 1 as `000/000/001.bin` and gives every directory
    /// at most 1000 entries.
    ///
    /// Both ends must agree: a consumer only descends as far as its own `shard_depth`. It
    /// warns when a scan finds subdirectories it is not configured to enter, since the
    /// alternative is a spool that reads as permanently empty.
    #[serde(default)]
    pub shard_depth: usize,
    /// How many characters of the sequence number each shard level consumes. Defaults to 3,
    /// giving 1000 entries per level. Ignored when `shard_depth` is 0.
    ///
    /// With sharding on, `naming_pattern` must start with a zero-padded `{seq:0N}` wider
    /// than `shard_depth * shard_width`, so that every chunk shards identically and one
    /// character is left for the file itself.
    #[serde(default = "default_spool_shard_width")]
    pub shard_width: usize,
    /// Extension of the payload file, with or without the leading dot. Defaults to `bin`.
    #[serde(default = "default_spool_payload_extension")]
    pub payload_extension: String,
    /// Extension of the JSON metadata sidecar, with or without the leading dot. Defaults to
    /// `json`. Set to an empty string to write and expect payload files only.
    #[serde(default = "default_spool_metadata_extension")]
    pub metadata_extension: String,
    /// (Sink only) Write to a `.tmp` name and rename into place, so a reader never sees a
    /// partial chunk. Defaults to true; turning it off trades that guarantee for one less
    /// rename per chunk.
    #[serde(default = "default_true")]
    pub atomic: bool,
    /// How hard to work at making a chunk survive a power loss. Defaults to `chunk`, which
    /// is two fsyncs per message with a sidecar. See [`SpoolFsync`].
    #[serde(default)]
    pub fsync: SpoolFsync,
    /// Name of the producer-completion sentinel file. Defaults to `DONE`.
    ///
    /// Production can span several producers — they run one at a time, which
    /// `producer_file` enforces — so "a producer closed" is not "production finished".
    /// This file is what says the latter, and only the last producer should write it.
    #[serde(default = "default_spool_done_file")]
    pub done_file: String,
    /// (Sink only) When to create `done_file`, marking production finished for a
    /// `stop_on_done` consumer. Defaults to `never`. See [`SpoolDone`].
    ///
    /// Set it on the *last* producer only. A publisher opening the spool deletes an
    /// existing sentinel, since it is producing again.
    #[serde(default)]
    pub emit_done: SpoolDone,
    /// Name of the file holding the producer lock, which keeps a second producer out.
    /// Defaults to `PRODUCER`.
    ///
    /// Every instance sharing the directory must agree on this name — that is what makes
    /// them exclude each other — and it must not collide with the other control files or
    /// with a chunk name. See [`SpoolClaim`].
    #[serde(default = "default_spool_producer_file")]
    pub producer_file: String,
    /// Name of the file holding the consumer lock, which keeps a second *draining*
    /// consumer out. Defaults to `CONSUMER`. Same rules as `producer_file`.
    #[serde(default = "default_spool_consumer_file")]
    pub consumer_file: String,
    /// (Source only) Delete each chunk's files once its message is acknowledged. Defaults to
    /// true — this is what makes the directory a queue rather than a growing archive. With
    /// it off, chunks are left in place. Acknowledged chunks are emitted at most once per
    /// consumer run; nacked chunks are redelivered.
    #[serde(default = "default_true")]
    pub drain_on_read: bool,
    /// (Source only) End the stream once the directory holds no unread chunks *and*
    /// `done_file` is present. Defaults to false, which tails the directory indefinitely.
    ///
    /// Both halves matter: a producer that finished long ago still has its backlog drained
    /// first, and a spool that is merely empty keeps the stream open, because another
    /// producer may still be coming.
    #[serde(default)]
    pub stop_on_done: bool,
    /// (Source only) Idle poll interval in milliseconds when the directory holds no new
    /// chunks. Defaults to 100.
    #[serde(default = "default_spool_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// (Source only) Include `mqb.src.spool_*` source positions in each message's metadata.
    /// Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// How this endpoint claims its side of the spool against a second instance in the
    /// same role. Defaults to `exclusive`. See [`SpoolClaim`].
    #[serde(default)]
    pub claim: SpoolClaim,
}

/// How hard a `dir_spool` endpoint works to make its writes survive a power loss.
///
/// This is the endpoint's dominant cost. `chunk`, the default, is two fsyncs per message —
/// the payload and its sidecar — plus one directory fsync per batch, and on a spinning disk
/// or a conservative SSD that sets the throughput ceiling long before anything in
/// mq-bridge does. Dropping the sidecar (`metadata_extension: ""`) halves it; `off` removes
/// it.
///
/// Independent of `atomic`, which decides whether a reader can see a half-written chunk,
/// not whether a crash can lose one. The once-per-run fsyncs — the lock, the sentinel — are
/// not affected by either.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpoolFsync {
    /// fsync every chunk file before it is renamed into place, and the directory once per
    /// batch (default). An acknowledged batch has reached the disk.
    #[default]
    Chunk,
    /// Do not fsync chunks at all, and let the operating system flush when it chooses.
    ///
    /// A crash or a power loss can then lose recent chunks *or* leave one present but
    /// truncated, which a consumer would deliver as a short message — so this trades the
    /// endpoint's crash-safety claim, not merely its tail. Reasonable when the spool is a
    /// buffer between two processes on one machine and a reboot means starting over anyway.
    Off,
}

/// When a `dir_spool` publisher writes its `done_file`, marking production finished.
///
/// The distinction is *how* the route ended, which the publisher learns from
/// [`DisconnectOutcome`](crate::traits::DisconnectOutcome): every teardown path closes the
/// publisher, so being closed says nothing on its own.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpoolDone {
    /// Never write it (default). Something else decides when production is finished.
    #[default]
    Never,
    /// Write it only when the route reached the natural end of its input and every chunk
    /// this producer accepted was written.
    ///
    /// A route that is shut down, that fails, or that reconnects does *not* write it —
    /// production did not finish, so a `stop_on_done` consumer keeps waiting. Note that a
    /// continuously running producer never reaches a natural end, so `success` on one only
    /// ever means "not yet": use `end` there.
    Success,
    /// Write it whenever the producer closes, however the route ended — a clean finish, a
    /// shutdown, or a failure.
    ///
    /// This says "nothing more is coming from here", not "everything worked". Use it for a
    /// consumer that must not wait forever on a producer that may have died, at the price
    /// of it treating a truncated stream as complete.
    End,
}

/// What a `dir_spool` endpoint does when a second instance opens the same directory in the
/// same role — a second producer writing it, or a second draining consumer emptying it.
///
/// Neither is supported by the endpoint's design: two producers seeded from the same
/// highest sequence number overwrite each other's chunks, and two draining consumers each
/// deliver every chunk they win the race to read. So each role takes a pid lock on its own
/// file — `producer_file` or `consumer_file` — created when the endpoint opens and removed
/// when it closes. A producer and a consumer sharing a directory do not conflict:
/// that is the whole point of the endpoint.
///
/// The locks say "someone is running", not "production is finished" — several producers
/// may fill one spool in turn, and `done_file` is what marks the end of that.
///
/// A lock whose owner is no longer running is broken and retaken, so a crash does not
/// wedge a restart. That check is by process id, which is only meaningful on the machine
/// and in the pid namespace that wrote it: a spool shared between hosts or containers can
/// have a live holder's lock broken by a second starter that cannot see its process.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpoolClaim {
    /// Refuse to start when the role is already claimed, naming the holder (default).
    #[default]
    Exclusive,
    /// Log a warning and run anyway. For a spool deliberately shared by several
    /// producers, which needs `{message_id}` in `naming_pattern` to keep names unique.
    Warn,
    /// Take no claim and check for none.
    Off,
}

// --- Object Store (local/S3/GCS/Azure) Specific Configuration ---

/// Configuration for a local or cloud object-store endpoint.
///
/// As a **sink**, each flushed batch is written as one immutable object under `url`,
/// named `<prefix>/[YYYY/MM/DD/]<uuidv7>.<ext>`. As a **source**, objects under `url`
/// are listed in key order, fetched, split by `delimiter`, and emitted as messages;
/// progress is persisted to `checkpoint_store` (the last processed object key) so a
/// restart resumes without re-emitting. Objects are never mutated or deleted in place.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObjectStoreConfig {
    /// Object-store URL, e.g. `file:///var/lib/mqb/incoming`, `s3://bucket/prefix`,
    /// `gs://bucket/prefix`, or `az://account/container/prefix`. Credentials are resolved
    /// from the environment by the `object_store` crate (same mechanism as the checkpoint
    /// backend); R2 uses `s3://` plus a custom `AWS_ENDPOINT_URL`.
    pub url: String,
    /// (Sink only) `auto`, `write_time` (uuidv7 name) or `source_position` (name carries the source range).
    #[serde(default)]
    pub name_by: NameBy,
    /// Deprecated: use `name_by`. true = `source_position`, false = `write_time`; ignored when `name_by` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<bool>,
    /// Record encoding within an object, shared with the file endpoint. Defaults to
    /// `normal` (one JSON `CanonicalMessage` per line). CSV is supported for sources only.
    #[serde(default)]
    pub format: FileFormat,
    /// Record delimiter within an object. Defaults to newline ("\n"). Can be a string or a
    /// hex sequence (e.g. "0x00").
    pub delimiter: Option<String>,
    /// (Source only) Durable resume store URL recording the last processed object key, e.g.
    /// `file:///var/lib/mqb/obj.json`, `s3://bucket/cursors`, or `postgres://…`. Without it
    /// every restart re-lists and re-emits all objects.
    pub checkpoint_store: Option<String>,
    /// (Source only) Cursor id namespacing the checkpoint key; enables durable resume.
    pub cursor_id: Option<String>,
    /// (Source only) Idle poll interval in milliseconds when no new objects are found.
    /// Defaults to 1000.
    pub polling_interval_ms: Option<u64>,
    /// (Source only) Maximum size in bytes of a single object to fetch into memory. An object
    /// larger than this fails the read (surfaced as a consumer error) instead of being
    /// buffered whole. Unset means no limit (the whole object is materialized).
    pub max_object_bytes: Option<u64>,
    /// (Sink only) Prepend a `YYYY/MM/DD/` path (write time, UTC) to each object key. Applies to
    /// `write_time` naming only; defaults to on. Purely for readability / lifecycle rules.
    #[serde(default)]
    pub date_partition: Option<bool>,
    /// (Sink only) Extension for written objects, without the dot. Defaults to a value derived
    /// from `format`, `compression` and `encryption` (e.g. `jsonl`, `csv`, `bin`, `jsonl.gz`,
    /// `jsonl.lz4`, `jsonl.gz.enc`); encrypted objects get a trailing `.enc` since they are
    /// ciphertext, not a directly decompressible `.gz`.
    pub extension: Option<String>,
    /// Whole-object compression (`none`, `gzip`, `lz4`, `zstd`). Requires the `compression` feature.
    #[serde(default)]
    pub compression: Compression,
    /// At-rest AEAD encryption applied after compression. Requires the `encryption` feature.
    #[serde(default)]
    pub encryption: Option<EncryptionConfig>,
}

// --- NATS Specific Configuration ---

/// General NATS connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NatsConfig {
    /// Comma-separated list of NATS server URLs (e.g., "nats://localhost:4222,nats://localhost:4223"). If it contains userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The NATS subject to publish to or subscribe to. If a stream is
    /// auto-created, it's scoped to `{stream}.>`, so prefix accordingly.
    pub subject: Option<String>,
    /// The JetStream stream name. Required for Consumers, even with
    /// `no_jetstream: true` (unused there, but still validated).
    pub stream: Option<String>,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub password: Option<String>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Optional token for authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
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
    /// (Publisher only, JetStream) If true, publish a `Nats-Msg-Id` header (from the message id) so
    /// JetStream deduplicates redeliveries within the stream's duplicate window. Defaults to false.
    #[serde(default)]
    pub deduplicate: bool,
    /// If no_jetstream: true, use Core NATS (fire-and-forget) instead of JetStream. Defaults to false.
    #[serde(default)]
    pub no_jetstream: bool,
    /// (Consumer only) If true, use ephemeral **Subscriber mode**. Defaults to false (durable consumer).
    #[serde(default)]
    pub subscriber_mode: bool,
    /// (Consumer only) Include authoritative `mqb.src.nats_*` source positions. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// (Publisher only) Maximum number of messages in the stream (if created by the bridge). Defaults to 1,000,000.
    pub stream_max_messages: Option<i64>,
    /// (Consumer only) The delivery policy for the consumer. Defaults to "all".
    pub deliver_policy: Option<NatsDeliverPolicy>,
    /// (Publisher only) Maximum total bytes in the stream (if created by the bridge). Defaults to 1GB.
    pub stream_max_bytes: Option<i64>,
    /// (Consumer only) Number of messages to prefetch from the consumer. Defaults to 10000.
    pub prefetch_count: Option<usize>,
    /// Share one NATS client per connection (default: true); false forces a dedicated connection.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NatsDeliverPolicy {
    #[default]
    All,
    Last,
    New,
    LastPerSubject,
}

#[derive(Debug, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transform = memory_config_schema_transform))]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// The topic name or transport URL. Can be:
    /// - Simple name: "my-topic" (defaults to memory://my-topic)
    /// - Memory URL: "memory://my-topic"
    /// - IPC URL: "ipc://my-queue" or "ipc:///path/to/socket"
    /// - Unix socket: "unix:///path/to/socket" (Unix only)
    /// - Named pipe: "pipe://my-pipe" (Windows only)
    ///
    /// Either `topic` or `url` can be specified (they are serde aliases).
    #[serde(default, skip_serializing_if = "String::is_empty", alias = "url")]
    pub topic: String,
    /// Transport URL (serde alias for `topic`). Use either `topic` or `url`.
    #[serde(skip)]
    pub url: Option<String>,
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
    /// (Consumer only) If true, enables NACK support (re-queuing), which requires cloning messages.
    /// Defaults to false for memory:// transports, automatically true for IPC transports (ipc://, unix://, pipe://).
    #[serde(default)]
    pub enable_nack: bool,
    #[serde(skip)]
    pub enable_nack_overridden: bool,
}

/// Configuration for the correlated in-process stream response buffer.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StreamBufferConfig {
    /// Shared buffer topic used by both the publisher and correlated consumers.
    pub topic: String,
    /// Consumer-only correlation id partition to read from.
    ///
    /// Leave this unset for the publisher endpoint configured in
    /// `HttpConfig::stream_response_to`. Set it on consumers so a reader only
    /// receives messages belonging to one request or response stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Capacity of each correlation partition. Defaults to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<usize>,
    /// Seconds before a partition with no attached consumer is discarded. 0 disables. Defaults to 3600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ttl_secs: Option<u64>,
}

// --- AMQP Specific Configuration ---

/// General AMQP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmqpConfig {
    /// AMQP connection URI. The `lapin` client connects to a single host specified in the URI. If it contains userinfo, it will be treated as a secret.
    /// For high availability, provide the address of a load balancer or use DNS resolution
    /// that points to multiple brokers. Example: "amqp://localhost:5672/vhost".
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The AMQP queue name.
    pub queue: Option<String>,
    /// (Consumer only) If true, act as a **Subscriber** (fan-out). Defaults to false.
    #[serde(default)]
    pub subscribe_mode: bool,
    /// (Consumer only) Include authoritative `mqb.src.amqp_*` source positions. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
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

/// How a MongoDB endpoint consumes a collection. One intent-named selector — the bridge picks the
/// underlying mechanism (change stream vs. polling) automatically. Defaults to `capture_all`.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MongoConsume {
    /// **Queue** — competing consumers: claim, process, delete, so each document goes to exactly one
    /// reader. Destructive and intended for jobs, not bulk reads.
    Consumer,
    /// **One-shot read** — page documents by `_id`, then end the route. Non-destructive and
    /// non-resumable; needs no replica set and reads arbitrary collections. This is not a
    /// point-in-time snapshot: separate page queries can observe concurrent inserts and deletes,
    /// while inserts below the current `_id` high-water mark can be missed.
    Snapshot,
    /// **Watch existing collection** — capture changes from now on (insert/update/delete), resuming
    /// under `cursor_id`. Reads an existing collection non-destructively; never ends on drain.
    CaptureNew,
    /// **Watch existing collection** — read the existing documents first, then capture changes.
    /// Non-destructive and the fastest read mode; use this for bulk reads and ETL. Default.
    /// Needs a replica set; on a standalone `mongod` use `snapshot` or `consumer`.
    #[default]
    CaptureAll,
}

// --- MongoDB Specific Configuration ---

/// General MongoDB connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MongoDbConfig {
    /// MongoDB connection string URI. Can contain a comma-separated list of hosts for a replica set. If it contains userinfo, it will be treated as a secret.
    /// Credentials provided via the separate `username` and `password` fields take precedence over any credentials embedded in the URL.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The MongoDB collection name.
    pub collection: Option<String>,
    /// Optional username. Takes precedence over any credentials embedded in the `url`.
    /// Use embedded URL credentials for simple one-off connections but prefer explicit username/password fields (or environment-sourced secrets) for clarity and secret management in production.
    pub username: Option<String>,
    /// Optional password. Takes precedence over any credentials embedded in the `url`.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
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
    /// (Consumer only) How to consume the collection: `capture_all` (default, read existing
    /// documents first, then watch for changes — non-destructive, for bulk reads and ETL),
    /// `capture_new` (watch an existing collection for changes only), `snapshot` (one-shot
    /// non-destructive read that ends on drain, the option without a replica set), or
    /// `consumer` (competing-consumers work queue — destructive and intended for jobs).
    /// The bridge selects the underlying mechanism automatically. If unset, the deprecated
    /// `change_stream` boolean is honored for backward compatibility.
    pub consume: Option<MongoConsume>,
    /// (Consumer only) Optional custom MongoDB query to filter messages. Provided as a JSON string (e.g., '{"type": "notification"}').
    pub receive_query: Option<String>,
    /// (Consumer only, `capture_new`/`capture_all`) Include authoritative `mqb.src.mongodb_*`
    /// source positions. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// (Consumer only) **Deprecated** — use `consume: capture_new`. Kept for compatibility.
    #[serde(default)]
    pub change_stream: bool,
    /// (Consumer only) Where to persist the resume cursor in `capture_new`/`capture_all` mode. A URL
    /// selects the backend; a bare name (or `/name`) reuses the **source** database with that name:
    /// - absent → source database, collection `mqb_cursors_<source_collection>` (auto-unique)
    /// - `/my_cursors` → source database, collection `my_cursors`
    /// - `file:///var/lib/mqb/cursors.json` → local JSON file (read-only / write-restricted sources)
    /// - `mongodb://host/db/collection` → external MongoDB collection (collection optional)
    /// - `postgres://user@host/db/table` or `mysql://host/db/table` → external SQL table (table optional)
    /// - `s3://bucket/prefix` (also `gs://`, `az://`, `abfs://`) → cloud object store; creds via env
    ///
    /// When no collection/table is named, it defaults to `mqb_cursors_<source_collection>`.
    /// May embed connection credentials, so it is treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub checkpoint_store: Option<String>,
    /// (Publisher only) Timeout for request-reply operations in milliseconds. Defaults to 30000ms.
    pub request_timeout_ms: Option<u64>,
    /// (Publisher only) TTL in seconds for documents created by the publisher. If set, a TTL index is created.
    pub ttl_seconds: Option<u64>,
    /// (Publisher only) If set, creates a capped collection with this size in bytes.
    pub capped_size_bytes: Option<i64>,
    /// Format for storing messages. Defaults to Normal.
    #[serde(default)]
    pub format: MongoDbFormat,
    /// (Publisher only) Top-level payload field whose value becomes the document `_id`, or a
    /// replay-stable `${...}` template such as `${metadata:mqb.id}`. Enables idempotent inserts
    /// through MongoDB's unique `_id` index. Sink collections only.
    pub id_field: Option<String>,
    /// (Publisher only) Return the message with metadata `mongodb.outcome` = `inserted`/`existed`
    /// (dup-key) so a `request`+`switch` can branch. Sink collections only; pair with `id_field`.
    #[serde(default)]
    pub report_outcome: bool,
    /// The ID used for the cursor in sequenced mode. If not provided, consumption starts from the current sequence (ephemeral).
    pub cursor_id: Option<String>,
    /// (Optional) Collection to store sequence counters and cursor positions. Defaults to the message collection if not set.
    pub meta_collection: Option<String>,
    /// Share one MongoDB client per connection (default: true); false forces a dedicated client.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
}

// --- MQTT Specific Configuration ---

/// General MQTT connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    /// MQTT broker URL (e.g., "tcp://localhost:1883"). Does not support multiple hosts. If it contains userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The MQTT topic.
    pub topic: Option<String>,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
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
    /// Note: For QoS 1/2 the publisher always waits for end-to-end broker
    /// confirmation (PUBACK/PUBCOMP) before reporting success, independent of
    /// this setting; QoS 0 remains fire-and-forget.
    #[serde(default)]
    pub delayed_ack: bool,
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
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
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
    /// Internal buffer size for the channel. Defaults to 128. `zmq` backend only — `omq`
    /// applies HWM backpressure on the socket itself and ignores this.
    #[serde(default)]
    pub internal_buffer_size: Option<usize>,
    /// Wire format: `json` wraps the CanonicalMessage; `raw` sends payload bytes per frame; `raw_framed` adds a JSON metadata frame. Default `raw_framed`.
    /// REQ/REP replies are the exception: a REP peer always answers with a JSON array of
    /// canonical messages and a REQ publisher always decodes one, whatever `format` is set to.
    #[serde(default)]
    pub format: ZeroMqFormat,
    /// Backend: `try_omq` (default, prefer `omq` and fall back to `zmq`), `zmq` (the `zeromq` crate) or `omq` (the `omq-tokio` backend). `omq` needs the `zeromq-omq` build feature.
    #[serde(default)]
    pub backend: ZeroMqBackend,
    /// (REQ publisher only) Timeout in ms for one request/reply exchange before it is reported as failed. Defaults to 30000.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
}

/// ZeroMQ wire format.
///
/// `json` wraps each message as a JSON CanonicalMessage (batched into one frame);
/// `raw` sends/receives the payload bytes directly, one frame per message (metadata
/// is not transmitted); `raw_framed` (default) sends a two-frame message — a JSON metadata
/// frame followed by the raw payload frame — keeping the payload binary-safe while still
/// carrying headers. Use `raw`/`raw_framed` for binary feeds such as JPEG, Avro or Protobuf.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ZeroMqFormat {
    Json,
    Raw,
    #[default]
    RawFramed,
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

/// ZeroMQ backend implementation.
///
/// `omq` uses `omq-tokio` (omq.rs) — much faster on the per-message `raw`/`raw_framed`
/// path and adds CURVE/PLAIN security — and requires the `zeromq-omq` build feature
/// (MSRV 1.93). `zmq` uses the `zeromq` crate (pure-Rust zmq.rs) and covers every
/// socket type.
///
/// `try_omq` is the default: it picks `omq` when that feature is compiled in and the
/// configured socket type is supported there, and falls back to `zmq` otherwise, so a
/// build without `zeromq-omq` still runs. Name a backend explicitly to make the choice
/// a hard requirement instead — a missing feature is then a startup error.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ZeroMqBackend {
    /// Prefer `omq`, fall back to `zmq` when it is unavailable for this build or socket.
    #[default]
    #[serde(rename = "try_omq", alias = "try-omq")]
    TryOmq,
    Zmq,
    Omq,
}

// --- Redis Streams Specific Configuration ---

/// Configuration for a Redis Streams endpoint.
///
/// Publishers `XADD` to the stream; consumers read via a consumer group
/// (`XREADGROUP` + `XACK`) by default, or ephemerally via `XREAD` from new
/// messages when `subscriber_mode` is set.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RedisStreamsConfig {
    /// Redis URL, `redis://` or `rediss://` for TLS. Userinfo is treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// The stream key to publish to or read from. Defaults to the route name.
    pub stream: Option<String>,
    /// (Consumer) Group name. Defaults to `{APP_NAME}-{stream}`; ignored in `subscriber_mode`.
    pub group: Option<String>,
    /// (Consumer) Consumer name within the group. Defaults to a unique per-instance id.
    pub consumer_name: Option<String>,
    /// (Consumer) Read ephemerally via `XREAD` from new messages (no group/acks). Default false.
    #[serde(default)]
    pub subscriber_mode: bool,
    /// (Consumer) Block timeout in milliseconds for each read. Defaults to 5000ms.
    pub block_ms: Option<u64>,
    /// (Consumer) On group creation, start from the stream beginning ("0") not "$". Default false.
    #[serde(default)]
    pub read_from_start: bool,
    /// (Consumer) Redeliver entries pending ≥ this long via `XAUTOCLAIM`; 0 disables. Default 60000ms.
    pub redelivery_timeout_ms: Option<u64>,
    /// (Publisher) If set, cap the stream length with `XADD MAXLEN`.
    pub maxlen: Option<usize>,
    /// (Publisher) Use approximate (`~`) trimming when `maxlen` is set. Defaults to true.
    pub approx_trim: Option<bool>,
    /// Optional username for authentication (Redis ACL).
    pub username: Option<String>,
    /// Optional password for authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub password: Option<String>,
    /// Internal buffer size for the consumer channel. Defaults to 128.
    pub internal_buffer_size: Option<usize>,
    /// (Consumer) Parallel `XREADGROUP` reader connections fanned out across the group. Default 1.
    /// Ignored in `subscriber_mode`.
    pub reader_connections: Option<usize>,
}

// --- gRPC Specific Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GrpcConfig {
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    /// The gRPC server URL (e.g., "http://localhost:50051" for client or "0.0.0.0:50051" for server mode).
    pub url: String,
    /// Topic / subject used for both subscribe and publish paths.
    pub topic: Option<String>,
    /// Stable subscription identity used for ACK tracking and redelivery. Defaults to a
    /// fresh id per consumer; set it to be redelivered unacknowledged messages on reconnect.
    #[serde(default)]
    pub consumer_id: Option<String>,
    /// Deprecated compatibility timeout in milliseconds. Used as the fallback for
    /// connection and initial-request deadlines. Prefer the dedicated settings.
    pub timeout_ms: Option<u64>,
    /// Maximum time to establish a client connection.
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    /// Maximum time to establish an RPC and receive its initial response.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    /// Maximum time a dynamic response stream may remain idle between messages.
    #[serde(default)]
    pub idle_stream_timeout_ms: Option<u64>,
    /// Maximum lifetime of a dynamic RPC; exceeding it stops the route instead of reconnecting.
    #[serde(default)]
    pub overall_timeout_ms: Option<u64>,
    /// TLS configuration.
    #[serde(default)]
    pub tls: TlsConfig,
    /// If `true`, start an embedded tonic gRPC server that accepts incoming `Publish` /
    /// `PublishBatch` RPCs. If `false` (the default), connect to a remote server as a client.
    #[serde(default)]
    pub server_mode: bool,
    /// HTTP/2 stream-level initial window size in bytes. Applies in both modes.
    #[serde(default)]
    pub initial_stream_window_size: Option<u32>,
    /// HTTP/2 connection-level initial window size in bytes. Applies in both modes.
    #[serde(default)]
    pub initial_connection_window_size: Option<u32>,
    /// Maximum number of concurrent requests handled per connection. **Server-mode only.**
    #[serde(default)]
    pub concurrency_limit_per_connection: Option<usize>,
    /// HTTP/2 keepalive ping interval in milliseconds. Applies in both modes. Default disabled
    #[serde(default)]
    pub http2_keepalive_interval_ms: Option<u64>,
    /// Timeout for a keepalive ping acknowledgement in milliseconds. Applies in both modes.
    #[serde(default)]
    pub http2_keepalive_timeout_ms: Option<u64>,
    /// Maximum size of a decoded incoming message in bytes. Applies in both modes. Default 4 MiB.
    #[serde(default)]
    pub max_decoding_message_size: Option<usize>,
    /// Maximum size of an encoded outgoing message in bytes. Default unlimited.
    #[serde(default)]
    pub max_encoding_message_size: Option<usize>,
    /// Compiled protobuf FileDescriptorSet for dynamic client mode.
    #[serde(default)]
    pub descriptor_set_path: Option<String>,
    /// Compiled protobuf FileDescriptorSet bytes for embedded callers. Takes precedence
    /// over `descriptor_set_path` and avoids writing a temporary descriptor file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_set_bytes: Option<Vec<u8>>,
    /// Discover descriptors from the remote gRPC server reflection v1 service.
    #[serde(default)]
    pub reflection: bool,
    /// Fully-qualified protobuf service name for dynamic client mode.
    #[serde(default)]
    pub service_name: Option<String>,
    /// RPC method name for dynamic client mode.
    #[serde(default)]
    pub method_name: Option<String>,
    /// JSON request mapped to the dynamic protobuf input message.
    #[serde(default)]
    pub request: Option<serde_json::Value>,
    /// Deprecated compatibility hint. Dynamic RPC shape is always derived from the descriptor.
    #[serde(default)]
    pub server_streaming: bool,
    /// Static ASCII metadata attached to dynamic RPCs and to the reflection RPC that
    /// fetches their descriptors. Values for keys that look sensitive are extracted by
    /// mq-bridge's normal secret handling.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    /// Static binary metadata for dynamic calls and the reflection RPC. Keys must end in
    /// `-bin`; each value is raw bytes, written as a JSON array of byte values 0-255 (the
    /// only accepted form — a base64 or text string is rejected). As a URL parameter:
    /// `?binary_metadata=%7B%22x-trace-bin%22%3A%5B1%2C2%2C3%5D%7D`, which is
    /// `{"x-trace-bin": [1, 2, 3]}` percent-encoded.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub binary_metadata: HashMap<String, Vec<u8>>,
    /// Bearer token sent as `authorization` on dynamic calls and the reflection RPC;
    /// rejected in Bridge/server mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub bearer_token: Option<String>,
    /// API key sent as `api_key_name` (default `x-api-key`) on dynamic calls and the
    /// reflection RPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub api_key: Option<String>,
    /// Metadata key used for `api_key`. Defaults to `x-api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_name: Option<String>,
    /// (Publisher only) Share one gRPC channel per connection (default: true); false forces a dedicated channel.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
}

// --- HTTP Specific Configuration ---

/// Supported inbound HTTP protocols for server listeners.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HttpServerProtocol {
    /// Accept both HTTP/1.1 and HTTP/2, matching the current default behavior.
    #[default]
    Auto,
    /// Accept only HTTP/1.x connections.
    Http1Only,
    /// Accept only HTTP/2 connections.
    Http2Only,
}

/// WebSocket route execution strategy.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WebSocketExecutionMode {
    /// Use direct per-connection handling for simple `websocket -> response` routes and fall back
    /// to the routed adapter with a warning when route semantics need the normal pipeline.
    #[default]
    Auto,
    /// Require direct per-connection handling. Startup fails if the route cannot run directly.
    DirectOnly,
    /// Always use the normal routed consumer/worker/disposition pipeline.
    Routed,
}

/// General HTTP connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// For consumers, the listen address (e.g., "0.0.0.0:8080"). For publishers, the target URL.
    pub url: String,
    /// (Consumer only) Optional request path filter. If set, only requests whose URI path matches exactly are delivered to this consumer.
    pub path: Option<String>,
    /// (Optional) HTTP method. For publishers: the method to use (defaults to POST). For consumers: restrict to this method (others return 405).
    pub method: Option<String>,
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
    /// (Publisher) Treat every HTTP response status as response data instead of classifying
    /// non-2xx statuses as publisher errors. Transport and response-read failures remain errors.
    /// (Consumer) When every output sink opts in, transient sink failures return 502 without
    /// stopping a non-streaming request/reply route. Defaults to false.
    #[serde(default)]
    pub pass_through_status: bool,
    /// (Consumer only) If true, read request bodies as a stream and emit each received stream item as a separate message.
    #[serde(default)]
    pub receive_streamable: bool,
    /// (Consumer only) If true, compatible `http -> response` routes may bypass the normal route consumer/worker/disposition pipeline
    /// and reply inline for lower latency. Defaults to true. Set to false to force the normal route path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(default = "default_inline_response_fast_path_schema")
    )]
    pub inline_response_fast_path: Option<bool>,
    /// (Consumer only) Restrict which HTTP protocol versions a server listener accepts.
    /// Defaults to `auto` (HTTP/1.1 + HTTP/2). On cleartext listeners, `http2_only`
    /// means HTTP/2 prior-knowledge (h2c) only.
    #[serde(default)]
    pub server_protocol: HttpServerProtocol,
    /// (Publisher only) Optional endpoint that receives streamed HTTP response items as correlated messages.
    ///
    /// Use a `stream_buffer` endpoint here when callers need to read streamed
    /// response items later through a normal mq-bridge consumer. Each streamed
    /// item is published with `correlation_id`, `http_stream_id`,
    /// `http_stream_index`, `http_stream_format`, and `http_stream_end`
    /// metadata. If the request message has no `correlation_id`, the HTTP
    /// publisher uses `format!("{:032x}", request.message_id)` so callers can
    /// derive the consumer correlation id before calling `send`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_response_to: Option<Box<Endpoint>>,
    /// (Publisher only) The number of concurrent HTTP requests to send in a batch. Defaults to 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_concurrency: Option<usize>,
    /// (Publisher only) TCP keepalive timeout for the underlying connection pool in milliseconds. Defaults to 60000ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_keepalive_ms: Option<u64>,
    /// (Publisher only) Timeout for idle connections in the connection pool in milliseconds. Defaults to 90000ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_idle_timeout_ms: Option<u64>,
    /// (Publisher only) Codec for the request body (`none`, `gzip`, `lz4`, `zstd`); overrides
    /// `compression_enabled`. `lz4` is non-standard (mq-bridge peers only). Ignored on a consumer —
    /// enable response compression with `compression_enabled`. Defaults to `none`.
    #[serde(default)]
    pub compression: Compression,
    /// Turns compression on. Publisher: compress the request body with gzip (unless `compression`
    /// sets another codec). Consumer: compress responses, negotiating the best codec the client's
    /// `Accept-Encoding` accepts. Defaults to off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_enabled: Option<bool>,
    /// Minimum message size in bytes to compress. Messages smaller than this are sent uncompressed. Defaults to 1024 bytes.
    #[serde(default)]
    pub compression_threshold_bytes: Option<usize>,
    /// (Consumer only) Maximum number of concurrent requests to handle. Defaults to 100.
    pub concurrency_limit: Option<usize>,
    /// HTTP Basic Authentication credentials (username, password). For consumers: validates incoming requests. For publishers: adds Authorization header.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_basic_auth"
    )]
    pub basic_auth: Option<(String, String)>,
    /// Custom headers as key-value pairs (e.g., {"X-API-Key": "token123"}). Added to outgoing HTTP headers for both consumers and publishers.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom_headers: HashMap<String, String>,
    /// (Publisher only) Share one HTTP client per connection (default: true); false forces a dedicated client.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
}

/// WebSocket connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WebSocketConfig {
    /// For consumers, the listen address (e.g. "0.0.0.0:9000"). For publishers, the target URL.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// (Consumer only) Optional request path filter. If set, only upgrade requests whose URI path matches exactly are delivered to this consumer.
    pub path: Option<String>,
    /// (Consumer only) Header key to extract the message ID from the WebSocket handshake. Defaults to "message-id".
    pub message_id_header: Option<String>,
    /// (Consumer only) Queue capacity for the routed adapter. Direct response routes do not use this queue. Defaults to 100.
    pub routed_queue_capacity: Option<usize>,
    /// (Consumer only) TCP listen backlog (pending-connection queue depth) for the accept socket.
    /// Raise this if high-concurrency handshake bursts are being dropped/reset before `accept()`
    /// can keep up. Defaults to 4096, which is higher than the OS/tokio default of 1024.
    pub backlog: Option<u32>,
    /// (Consumer only) Selects whether WebSocket routes run directly or through the routed pipeline.
    #[serde(default)]
    pub execution_mode: WebSocketExecutionMode,
}

// --- IBM MQ Specific Configuration ---

/// TLS configuration for the IBM MQ native client.
///
/// The IBM MQ client doesn't consume PEM files, so this uses MQ-native field
/// names rather than the generic [`TlsConfig`] used by the other endpoints.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transform = ibm_tls_config_schema_transform))]
#[serde(deny_unknown_fields)]
pub struct IbmTlsConfig {
    /// If true, enable TLS/SSL.
    #[serde(default, deserialize_with = "deserialize_null_as_false")]
    pub required: bool,
    /// TLS CipherSpec (e.g., `ANY_TLS12`). Required for encrypted connections. IBM MQ-specific.
    pub cipher_spec: Option<String>,
    /// For IBM MQ this is the CMS key repository stem (e.g. `/path/to/tls` for `tls.kdb`/`tls.sth`),
    /// not a PEM file. Exposed as `cert_file` for config parity with the generic `TlsConfig`;
    /// the MQ-native name `key_repository` is still accepted.
    #[serde(rename = "cert_file", alias = "key_repository")]
    pub key_repository: Option<String>,
    /// Password unlocking the key repository. Requires an IBM MQ client/server at 9.3.0.0+.
    /// Exposed as `cert_password` for parity with `TlsConfig`; alias `key_repository_password`.
    #[serde(rename = "cert_password", alias = "key_repository_password")]
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub key_repository_password: Option<String>,
    /// If true, disable server certificate verification (insecure).
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

/// Connection settings for the IBM MQ Queue Manager.
// Default is implemented manually (not derived): the numeric fields must match
// the serde defaults, otherwise `IbmMqConfig::new()` / `..Default::default()`
// would yield max_message_size=0 (zero-length receive buffer) and wait_timeout=0.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IbmMqConfig {
    /// Required. Connection URL in `host(port)` format. Supports comma-separated list for failover (e.g., `host1(1414),host2(1414)`). If it contains userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// Target Queue name for point-to-point messaging. Optional if `topic` is set; defaults to route name if omitted.
    pub queue: Option<String>,
    /// Target Topic string for Publish/Subscribe. If set, enables **Subscriber mode** (Consumer) or publishes to a topic (Publisher). Optional if `queue` is set.
    pub topic: Option<String>,
    /// Required. Name of the Queue Manager to connect to (e.g., `QM1`).
    pub queue_manager: String,
    /// Required. Server Connection (SVRCONN) Channel name defined on the QM.
    pub channel: String,
    /// Username for authentication. Optional; required if the channel enforces authentication
    pub username: Option<String>,
    /// Password for authentication. Optional; required if the channel enforces authentication.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub password: Option<String>,
    /// TLS configuration settings (e.g., keystore paths). Optional.
    #[serde(default)]
    pub tls: IbmTlsConfig,
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

// --- Switch/Router Configuration ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    /// Value-lookup mode: the metadata key whose value picks the case.
    #[serde(default)]
    pub metadata_key: String,
    /// Value-lookup mode: a map of metadata values to endpoints.
    #[serde(default)]
    pub cases: HashMap<String, Endpoint>,
    /// Predicate mode: ordered cases, first match wins. Needs the `filter` feature.
    #[serde(default)]
    pub when: Vec<SwitchCase>,
    /// The default endpoint if no case matches.
    pub default: Option<Box<Endpoint>>,
}

/// One predicate case of a `switch` in `when` mode.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SwitchCase {
    /// Expression over payload fields and `meta.<key>`, e.g. `amount > 100`.
    #[serde(rename = "if")]
    #[cfg_attr(
        feature = "filter",
        serde(deserialize_with = "deserialize_filter_expression")
    )]
    pub condition: String,
    /// Where a matching message goes.
    pub to: Endpoint,
}

impl SwitchConfig {
    /// Rejects a `switch` that names neither mode or both.
    ///
    /// The two modes differ in cost, not just spelling: value lookup is a
    /// HashMap get on metadata, while a predicate may parse the payload. Mixing
    /// them in one endpoint would hide which one a message actually took.
    pub fn validate(&self) -> anyhow::Result<()> {
        let lookup = !self.metadata_key.is_empty() || !self.cases.is_empty();
        let predicate = !self.when.is_empty();
        match (lookup, predicate) {
            (true, true) => Err(anyhow::anyhow!(
                "switch takes either `metadata_key` + `cases` or `when`, not both"
            )),
            (false, false) => Err(anyhow::anyhow!(
                "switch needs either `metadata_key` + `cases` (value lookup) or `when` (predicates)"
            )),
            (true, false) if self.metadata_key.is_empty() => Err(anyhow::anyhow!(
                "switch `cases` needs a `metadata_key` to look up"
            )),
            _ => Ok(()),
        }
    }
}

// --- Response Endpoint Configuration ---
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    // This struct is a marker and currently has no fields.
}

// --- Request/Forward Endpoint Configuration ---

/// Sends each message to a request-capable endpoint and forwards its response elsewhere.
///
/// Turns a request/reply exchange (HTTP, or a request_reply NATS/Mongo/Memory endpoint) into
/// a one-way flow whose response lands on `forward_to` — e.g. IBM MQ → HTTP → IBM MQ. On
/// request error/timeout the original message is forwarded instead (unchanged). Successful
/// responses carry the transport-native status (e.g. `http_status_code`), so a `switch` on
/// `forward_to` can route them by status; a failed request forwards the original message with
/// no status key, so catch failures on the switch's default branch.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RequestForwardConfig {
    /// The request-capable endpoint to send each message to (e.g. an `http` client).
    pub to: Box<Endpoint>,
    /// Where the response (or, on error, the original message) is forwarded.
    pub forward_to: Box<Endpoint>,
}

// --- Postgres CDC (logical replication) Configuration ---

/// Postgres logical-replication CDC source (pgoutput). Source-only.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PostgresCdcConfig {
    /// Connection URL, e.g. `postgres://user:pass@host:5432/dbname`.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// Publication name (must already exist; defines which tables are captured).
    pub publication: String,
    /// Include authoritative `mqb.src.postgres_*` source positions. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
    /// Replication slot name; created if missing when `create_slot` is true.
    #[serde(default = "default_pg_cdc_slot")]
    pub slot_name: String,
    /// Create the replication slot if it does not exist.
    #[serde(default = "default_true")]
    pub create_slot: bool,
    /// Create the `publication` if missing (default false; leave off if it pre-exists).
    /// Needs table ownership for `publication_tables`, or superuser when none are set (`FOR ALL TABLES`).
    #[serde(default)]
    pub create_publication: bool,
    /// Tables to include when managing the publication (`create_publication`); may be `schema.table`.
    /// Missing ones are added to an existing publication (never removed). Empty = `FOR ALL TABLES` (needs superuser).
    #[serde(default)]
    pub publication_tables: Vec<String>,
    /// Ephemeral run: drop the slot when the route stops. Not restart-safe; a hard crash leaks it.
    #[serde(default)]
    pub temporary_slot: bool,
    /// Checkpoint key for persisting the confirmed LSN across restarts (optional; the slot is authoritative).
    pub cursor_id: Option<String>,
    /// Checkpoint store spec (e.g. `file:///path`, `s3://bucket/prefix`); defaults to the source database.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub checkpoint_store: Option<String>,
    /// Standby-status-update interval in ms; must be shorter than the server's `wal_sender_timeout`.
    #[serde(default = "default_pg_cdc_status_interval_ms")]
    pub status_interval_ms: u64,
    /// TLS configuration for the replication connection.
    #[serde(default)]
    pub tls: TlsConfig,
}

// --- SQLx Specific Configuration ---

/// General SQLx connection configuration.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SqlxConfig {
    /// Database connection URL. If it contains userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// Optional username. Takes precedence over any credentials embedded in the `url`.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional password. Takes precedence over any credentials embedded in the `url`.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    #[serde(default)]
    pub password: Option<String>,
    /// The table to interact with.
    pub table: String,
    /// (Publisher only) Optional. A custom SQL INSERT query. Use `?` as a placeholder for the payload.
    /// If not provided, a default `INSERT INTO {table} (payload) VALUES (?)` is used.
    ///
    /// For multi-column inserts, embed explicit source tokens directly in the query:
    /// `${metadata:<key>}` binds `message.metadata["<key>"]`, and `${payload:<field>}`
    /// binds the top-level JSON field `<field>` of the payload (types preserved:
    /// numbers/bools stay numeric/bool). There is no fallback between the two: an
    /// absent metadata key, non-JSON payload, or missing/non-scalar field binds SQL NULL.
    /// Example: `INSERT INTO orders (customer_id, sku, qty) VALUES (${metadata:customer_id}, ${payload:sku}, ${payload:qty})`.
    /// A query with no `${...}` tokens behaves exactly as before (whole payload bound once).
    /// `auto_create_table` is not supported together with a token-based query.
    ///
    /// Tokens bind as text/number/bool; Postgres won't implicitly cast text into a
    /// `numeric`/`timestamptz` column (these arrive as JSON strings from a sql source).
    /// Add an explicit cast next to the token — it is preserved verbatim in the SQL:
    /// `VALUES (${payload:amount}::numeric, ${payload:created_at}::timestamptz)`.
    pub insert_query: Option<String>,
    /// (Consumer only) Optional. A custom SQL SELECT query to fetch messages. This is only supported for PostgreSQL and Microsoft SQL Server.
    /// The query must include a placeholder for the batch size (`$1` for PostgreSQL, `@p1` for SQL Server).
    /// The bridge will bind the route's `batch_size` to this placeholder.
    pub select_query: Option<String>,
    /// (Consumer only) If true, delete messages after processing.
    #[serde(default)]
    pub delete_after_read: bool,
    /// (Consumer only) Read an existing table **non-destructively** and resumably, paging by this
    /// monotonic column (`SELECT * FROM {table} WHERE {cursor_column} > $last ORDER BY {cursor_column} ASC LIMIT n`)
    /// and persisting the last read value under `cursor_id`. Does not delete/lock source rows.
    /// Mutually exclusive with `delete_after_read`.
    pub cursor_column: Option<String>,
    /// (Consumer only) Cursor id used to key the persisted resume position. Recommended when
    /// `cursor_column` is set: without it, progress is not persisted and every restart re-copies
    /// from the beginning.
    pub cursor_id: Option<String>,
    /// (Consumer only) Where to persist the resume cursor in `cursor_column` mode. A URL selects the
    /// backend; a bare name (or `/name`) reuses the **source** datastore with that table name:
    /// - absent → source datastore, table `mqb_cursors_<source_table>` (auto-unique)
    /// - `/my_cursors` → source datastore, table `my_cursors`
    /// - `file:///var/lib/mqb/cursors.json` → local JSON file (read-only / write-restricted sources)
    /// - `postgres://user@host/db/table` or `mysql://host/db/table` → external SQL table (table optional)
    /// - `mongodb://host/db/collection` → external MongoDB collection (collection optional)
    /// - `s3://bucket/prefix` (also `gs://`, `az://`, `abfs://`) → cloud object store; creds via env
    ///
    /// When no table/collection is named, it defaults to `mqb_cursors_<source_table>`.
    /// May embed connection credentials, so it is treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub checkpoint_store: Option<String>,
    /// (Publisher only) If true, automatically create the table and indexes if they don't exist. Defaults to false.
    #[serde(default)]
    pub auto_create_table: bool,
    /// (Publisher only) PostgreSQL only. Bulk-load batches via `COPY FROM STDIN` (much faster than multi-row INSERT). Requires a token-based `insert_query`; no `ON CONFLICT`/`RETURNING`.
    #[serde(default)]
    pub bulk_copy: bool,
    /// (Consumer only) Polling interval in milliseconds. Defaults to 100ms.
    pub polling_interval_ms: Option<u64>,
    /// (Consumer only) If set, the poll interval backs off exponentially from `polling_interval_ms`
    /// up to this value while drained, resetting on new rows. Unset = constant interval.
    pub max_polling_interval_ms: Option<u64>,
    /// (Consumer only, PostgreSQL) If set, consume via logical-replication CDC instead of cursor
    /// polling: streams inserts/updates/deletes from this publication. Requires the `postgres-cdc`
    /// feature and a Postgres URL. For full control use the dedicated `postgres_cdc` endpoint.
    pub publication: Option<String>,
    /// (Consumer only, CDC) Replication slot name; created if missing. Defaults to `mq_bridge_slot`.
    pub slot_name: Option<String>,
    /// (Consumer only, CDC) When `publication` is set, create it if missing (default false).
    /// Needs table-owner privilege: it is auto-published `FOR TABLE {table}`.
    #[serde(default)]
    pub create_publication: bool,
    /// (Consumer only) Include authoritative `mqb.src.sqlx_*` source positions; `cursor_column` must then be a unique integer. Defaults to false.
    #[serde(default)]
    pub source_metadata: bool,
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
    /// Ping each pooled connection before handing it out (default false). Costs a round-trip per acquire.
    pub test_before_acquire: Option<bool>,
    /// Share one connection pool per connection (default: true); false forces a dedicated pool.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(default = "default_shared_schema"))]
    pub shared: Option<bool>,
}

// --- ClickHouse Specific Configuration ---

/// ClickHouse endpoint configuration (talks the ClickHouse HTTP interface).
///
/// As a **publisher** it batch-inserts messages using `FORMAT JSONEachRow` — by default the whole
/// message payload (which must be a JSON object) becomes one row; set `columns` to build each row
/// from explicit `${payload:<field>}` / `${metadata:<key>}` tokens instead. As a **consumer** it
/// reads an existing table **non-destructively** by paging over a monotonic `cursor_column`
/// (ClickHouse has no native queue/pub-sub), serializing each row to a JSON payload.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ClickHouseConfig {
    /// ClickHouse HTTP endpoint URL, e.g. `http://localhost:8123` (or `https://…`). If it contains
    /// userinfo, it will be treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub url: String,
    /// Optional username. Takes precedence over any credentials embedded in the `url`. Defaults to `default`.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional password. Takes precedence over any credentials embedded in the `url`.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    #[serde(default)]
    pub password: Option<String>,
    /// Database name. Defaults to `default`.
    pub database: Option<String>,
    /// The table to read from / write to. May be schema-qualified (`db.table`).
    pub table: String,
    /// (Publisher only) Optional per-column mapping. Each entry maps a target column name to a value
    /// token: `${payload:<field>}` takes the top-level JSON field `<field>` of the payload (JSON type
    /// preserved), `${metadata:<key>}` takes `message.metadata["<key>"]` (as a string), and any other
    /// value is inserted literally. When omitted, the whole payload JSON object is inserted as one row.
    pub columns: Option<std::collections::BTreeMap<String, String>>,
    /// (Publisher only) If true, set the ClickHouse `async_insert=1` server setting so inserts are
    /// buffered server-side. Defaults to false.
    #[serde(default)]
    pub async_insert: bool,
    /// (Publisher only) With `async_insert`, wait for the server to flush before acking. Defaults to
    /// true (durable). False = fire-and-forget: faster, but a crash before flush can drop the batch.
    #[serde(default)]
    pub wait_for_async_insert: Option<bool>,
    /// (Consumer only) Read an existing table **non-destructively** and resumably, paging by this
    /// monotonic column (`SELECT … WHERE {cursor_column} > {last} ORDER BY {cursor_column} ASC LIMIT n`)
    /// and persisting the last read value under `cursor_id`.
    pub cursor_column: Option<String>,
    /// (Consumer only) Cursor id used to key the persisted resume position. Without it, progress is not
    /// persisted and every restart re-copies from the beginning.
    pub cursor_id: Option<String>,
    /// (Consumer only) Where to persist the resume cursor. Because ClickHouse is unsuited to per-row
    /// cursor upserts, a durable checkpoint requires an **external** store URL:
    /// - `file:///var/lib/mqb/cursors.json` → local JSON file
    /// - `postgres://user@host/db/table` / `mysql://host/db/table` → external SQL table (table optional)
    /// - `mongodb://host/db/collection` → external MongoDB collection (collection optional)
    /// - `s3://bucket/prefix` (also `gs://`, `az://`, `abfs://`) → cloud object store; creds via env
    ///
    /// May embed connection credentials, so it is treated as a secret.
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub checkpoint_store: Option<String>,
    /// (Consumer only) Columns to select in `cursor_column` mode. Defaults to `*`.
    pub select_columns: Option<String>,
    /// (Consumer only) Polling interval in milliseconds when the table is drained. Defaults to 100ms.
    pub polling_interval_ms: Option<u64>,
    /// (Consumer only) If set, the poll interval backs off exponentially from `polling_interval_ms`
    /// up to this value while drained, resetting on new rows. Unset = constant interval.
    pub max_polling_interval_ms: Option<u64>,
    /// Request timeout in milliseconds for ClickHouse HTTP calls (inserts, cursor reads, status).
    /// Unset = no timeout (wait indefinitely), which suits very large batch inserts.
    pub request_timeout_ms: Option<u64>,
    /// Connection (TCP + TLS handshake) timeout in milliseconds. Defaults to 10000ms.
    pub connect_timeout_ms: Option<u64>,
    /// TLS configuration for `https://` connections.
    #[serde(default)]
    pub tls: TlsConfig,
    /// HTTP body compression for inserts and cursor reads (`none`, `gzip`, `lz4`, `zstd`). Applied
    /// as `Content-Encoding` on the request body and negotiated on the response via `Accept-Encoding`.
    /// `lz4`/`zstd` are faster than `gzip`; all are understood natively by ClickHouse. Defaults to `gzip`.
    #[serde(default = "default_gzip_compression")]
    pub compression: Compression,
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
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq, Hash)]
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
    #[cfg_attr(feature = "schema", schemars(extend("format"="password")))]
    pub cert_password: Option<String>,
    /// If true, disable server certificate verification (insecure).
    #[serde(default)]
    pub accept_invalid_certs: bool,
}
