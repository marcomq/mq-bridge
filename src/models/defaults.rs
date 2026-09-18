//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Default values for the configuration structs in the parent module.
//!
//! Most are named in `serde(default = "...")` / `schemars(default = "...")` attributes,
//! which resolve the path in `models`, so `models.rs` glob-imports this module.

use super::*;

impl Default for Route {
    fn default() -> Self {
        Self {
            input: Endpoint::null(),
            output: Endpoint::null(),
            options: RouteOptions::default(),
        }
    }
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            description: String::new(),
            concurrency: default_concurrency(),
            batch_size: default_batch_size(),
            commit_concurrency_limit: default_commit_concurrency_limit(),
            startup_timeout_ms: default_startup_timeout_ms(),
            reconnect_interval_ms: default_reconnect_interval_ms(),
            empty_batch_delay_ms: default_empty_batch_delay_ms(),
            allow_fault_injection: false,
            exit_on_empty: false,
        }
    }
}

pub(crate) fn default_concurrency() -> usize {
    1
}

pub(crate) fn default_batch_size() -> usize {
    512
}

pub(crate) fn default_commit_concurrency_limit() -> usize {
    4096
}

pub(crate) fn default_startup_timeout_ms() -> u64 {
    5000
}

pub(crate) fn default_reconnect_interval_ms() -> u64 {
    5000
}

pub(crate) fn default_empty_batch_delay_ms() -> u64 {
    10
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn default_false() -> bool {
    false
}

#[cfg(feature = "schema")]
pub(crate) fn default_inline_response_fast_path_schema() -> Option<bool> {
    Some(true)
}

/// Schema default for `shared` fields, whose runtime default is `true`.
#[cfg(feature = "schema")]
pub(crate) fn default_shared_schema() -> Option<bool> {
    Some(true)
}

/// Schema default for Kafka `partitions`, whose runtime default is 6.
#[cfg(feature = "schema")]
pub(crate) fn default_kafka_partitions_schema() -> Option<i32> {
    Some(DEFAULT_KAFKA_PARTITIONS)
}

/// Partition count used when auto-creating a Kafka topic if none is configured.
/// ordering is per-key (we key by message_id), not global across the topic if > 1
pub const DEFAULT_KAFKA_PARTITIONS: i32 = 6;

pub(crate) fn default_output_endpoint() -> Endpoint {
    Endpoint::new(EndpointType::Null)
}

pub(crate) fn default_retry_attempts() -> usize {
    3
}
pub(crate) fn default_initial_interval_ms() -> u64 {
    100
}
pub(crate) fn default_max_interval_ms() -> u64 {
    5000
}
pub(crate) fn default_multiplier() -> f64 {
    2.0
}
pub(crate) fn default_clean_session() -> bool {
    false
}
pub(crate) fn default_cookie_metadata_key() -> String {
    "cookie".to_string()
}
pub(crate) fn default_set_cookie_metadata_key() -> String {
    "set-cookie".to_string()
}

pub(crate) fn default_encryption_key_id() -> String {
    "default".to_string()
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            cipher: CipherKind::default(),
            key_id: default_encryption_key_id(),
            key: String::new(),
            decrypt_keys: HashMap::new(),
            authenticate_metadata: Vec::new(),
        }
    }
}

pub(crate) fn default_max_cookies() -> usize {
    256
}

impl Default for CookieJarMiddleware {
    fn default() -> Self {
        Self {
            shared_scope: None,
            cookie_metadata_key: default_cookie_metadata_key(),
            set_cookie_metadata_key: default_set_cookie_metadata_key(),
            capture_metadata_keys: Vec::new(),
            export_metadata_prefix: None,
            inject_metadata: HashMap::new(),
            max_cookies: default_max_cookies(),
        }
    }
}

// Hand-written rather than derived: `coerce` and `apply_defaults` default to *true*, which
// a derived `Default` would silently turn into `false`. That would make
// `TransformMiddleware { ..Default::default() }` in Rust behave differently from the same
// config parsed from YAML.
impl Default for TransformMiddleware {
    fn default() -> Self {
        Self {
            mapping: HashMap::new(),
            expression: None,
            schema: None,
            schema_file: None,
            coerce: default_true(),
            apply_defaults: default_true(),
            coerce_empty_as_null: false,
            on_error: TransformErrorPolicy::default(),
        }
    }
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_spool_naming_pattern() -> String {
    "{seq:09}".to_string()
}

pub(crate) fn default_spool_payload_extension() -> String {
    "bin".to_string()
}

pub(crate) fn default_spool_metadata_extension() -> String {
    "json".to_string()
}

pub(crate) fn default_spool_shard_width() -> usize {
    3
}

pub(crate) fn default_spool_done_file() -> String {
    "DONE".to_string()
}

pub(crate) fn default_spool_producer_file() -> String {
    "PRODUCER".to_string()
}

pub(crate) fn default_spool_consumer_file() -> String {
    "CONSUMER".to_string()
}

pub(crate) fn default_spool_poll_interval_ms() -> u64 {
    100
}

impl Default for DirSpoolConfig {
    fn default() -> Self {
        Self::new(String::new())
    }
}

pub(crate) fn default_atomic_usize_arc() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

pub(crate) fn default_compression_algorithm() -> Compression {
    Compression::Zstd
}

impl Default for CompressionMiddleware {
    fn default() -> Self {
        Self {
            algorithm: default_compression_algorithm(),
            max_decompressed_bytes: None,
        }
    }
}

impl Default for FileConsumerMode {
    fn default() -> Self {
        Self::Consume { delete: false }
    }
}

impl Default for IbmMqConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            queue: None,
            topic: None,
            queue_manager: String::new(),
            channel: String::new(),
            username: None,
            password: None,
            tls: IbmTlsConfig::default(),
            max_message_size: default_max_message_size(),
            wait_timeout_ms: default_wait_timeout_ms(),
            internal_buffer_size: None,
            disable_status_inq: false,
        }
    }
}

pub(crate) fn default_max_message_size() -> usize {
    4 * 1024 * 1024 // 4MB default
}

pub(crate) fn default_wait_timeout_ms() -> i32 {
    1000 // 1 second default
}

pub(crate) fn default_pg_cdc_slot() -> String {
    "mq_bridge_slot".to_string()
}

pub(crate) fn default_pg_cdc_status_interval_ms() -> u64 {
    10_000
}

pub(crate) fn default_gzip_compression() -> Compression {
    Compression::Gzip
}
