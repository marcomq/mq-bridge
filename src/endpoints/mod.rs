//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

#[cfg(feature = "amqp")]
pub mod amqp;
#[cfg(feature = "aws")]
pub mod aws;
#[cfg(feature = "clickhouse")]
pub mod clickhouse;
pub mod dir_spool;
pub mod file;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;
#[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
pub mod ibm_mq;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod memory;
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "object-store")]
pub mod object_store;
#[cfg(any(feature = "sqlx", feature = "clickhouse"))]
mod poll;
#[cfg(feature = "postgres-cdc")]
pub mod postgres;
#[cfg(feature = "redis-streams")]
pub mod redis_streams;
#[cfg(feature = "sled")]
pub mod sled;
#[cfg(feature = "sqlx")]
pub mod sqlx;
/// Structural endpoints (`fanout`, `switch`, `request`, `response`, `reader`,
/// `static`, `stream_buffer`, `null`) that route or terminate a flow instead of
/// talking to an external system.
pub mod structural;
#[cfg(feature = "websocket")]
pub mod websocket;
#[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
pub mod zeromq;
use crate::endpoints::memory::{get_or_create_channel, MemoryChannel};
/// Backwards-compatible aliases for the structural endpoints, which used to live
/// directly under `endpoints`. Prefer `endpoints::structural::*`.
#[doc(hidden)]
pub use crate::endpoints::structural::{
    fanout, null, reader, request, response, static_endpoint, stream_buffer, switch,
};
use crate::middleware::apply_middlewares_to_consumer;
use crate::models::{
    Endpoint, EndpointType, MemoryConfig, Middleware, NameBy, ResponseConfig, SpoolDone,
    StreamBufferConfig, TransformErrorPolicy,
};
use crate::route::{get_endpoint, get_endpoint_factory};
use crate::traits::{BoxFuture, MessageConsumer, MessagePublisher};
use anyhow::{anyhow, Result};
use std::sync::Arc;

impl Endpoint {
    pub fn new(endpoint_type: EndpointType) -> Self {
        Self {
            middlewares: Vec::new(),
            endpoint_type,
            handler: None,
        }
    }
    /// Creates a new in-memory endpoint with the specified topic and capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use mq_bridge::models::Endpoint;
    /// let endpoint = Endpoint::new_memory("my_topic", 100);
    /// ```
    pub fn new_memory(topic: &str, capacity: usize) -> Self {
        Self::new(EndpointType::Memory(MemoryConfig::new(
            topic,
            Some(capacity),
        )))
    }
    pub fn new_response() -> Self {
        Self::new(EndpointType::Response(ResponseConfig::default()))
    }
    pub fn new_stream_buffer(topic: &str, correlation_id: Option<&str>, capacity: usize) -> Self {
        Self::new(EndpointType::StreamBuffer(StreamBufferConfig {
            topic: topic.to_string(),
            correlation_id: correlation_id.map(str::to_string),
            capacity: Some(capacity),
            idle_ttl_secs: None,
        }))
    }
    pub fn has_retry_middleware(&self) -> bool {
        self.middlewares
            .iter()
            .any(|m| matches!(m, Middleware::Retry(_)))
    }

    pub fn has_dlq_middleware(&self) -> bool {
        self.middlewares
            .iter()
            .any(|m| matches!(m, Middleware::Dlq(_)))
    }
    pub fn add_middleware(mut self, middleware: Middleware) -> Self {
        self.middlewares.push(middleware);
        self
    }
    pub fn add_middlewares(mut self, mut middlewares: Vec<Middleware>) -> Self {
        self.middlewares.append(&mut middlewares);
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
    pub fn null() -> Self {
        Self::new(EndpointType::Null)
    }

    pub fn with_retry(mut self, retry: crate::models::RetryMiddleware) -> Self {
        // Retry should be inner to DLQ and Metrics.
        // We insert it before any existing DLQ or Metrics middleware.
        let mut insert_idx = self.middlewares.len();
        for (i, m) in self.middlewares.iter().enumerate() {
            if matches!(m, Middleware::Dlq(_) | Middleware::Metrics(_)) {
                insert_idx = i;
                break;
            }
        }
        self.middlewares
            .insert(insert_idx, Middleware::Retry(retry));
        self
    }

    pub fn with_dlq(mut self, dlq: crate::models::DeadLetterQueueMiddleware) -> Self {
        // DLQ should be outer to Retry, but inner to Metrics.
        let mut insert_idx = self.middlewares.len();
        for (i, m) in self.middlewares.iter().enumerate() {
            if matches!(m, Middleware::Metrics(_)) {
                insert_idx = i;
                break;
            }
        }
        self.middlewares
            .insert(insert_idx, Middleware::Dlq(Box::new(dlq)));
        self
    }

    pub fn with_deduplication(mut self, dedup: crate::models::DeduplicationMiddleware) -> Self {
        // Deduplication is consumer-only.
        // We insert it at the beginning so it is applied last (innermost) for consumers,
        // or second to last if metrics are at 0.
        // List: [Dedup, ...] -> Consumer: ... ( Dedup ( base ) )
        self.middlewares.insert(0, Middleware::Deduplication(dedup));
        self
    }

    pub fn with_consumer_metrics(mut self) -> Self {
        // For consumers, the first middleware in the list is the outermost (applied last).
        // Inserting at 0 ensures it wraps everything else (Ingestion Metrics).
        // List: [Metrics, Dedup] -> Consumer: Metrics ( Dedup ( base ) )
        if !self
            .middlewares
            .iter()
            .any(|m| matches!(m, Middleware::Metrics(_)))
        {
            self.middlewares
                .insert(0, Middleware::Metrics(crate::models::MetricsMiddleware {}));
        }
        self
    }

    pub fn with_metrics(mut self) -> Self {
        // Metrics should be outer to everything (last in the list for publishers).
        if !self
            .middlewares
            .iter()
            .any(|m| matches!(m, Middleware::Metrics(_)))
        {
            self.middlewares
                .push(Middleware::Metrics(crate::models::MetricsMiddleware {}));
        }
        self
    }

    pub async fn create_consumer(
        &self,
        route_name: &str,
    ) -> anyhow::Result<Box<dyn crate::traits::MessageConsumer>> {
        crate::endpoints::create_consumer_from_route(route_name, self).await
    }

    pub async fn create_publisher(&self, _route_name: &str) -> anyhow::Result<crate::Publisher> {
        crate::Publisher::new(self.clone()).await
    }

    pub fn check_consumer(
        &self,
        route_name: &str,
        allowed_endpoints: Option<&[&str]>,
    ) -> anyhow::Result<Vec<String>> {
        crate::endpoints::check_consumer(route_name, self, allowed_endpoints)
    }

    pub fn check_publisher(
        &self,
        route_name: &str,
        allowed_endpoints: Option<&[&str]>,
    ) -> anyhow::Result<Vec<String>> {
        crate::endpoints::check_publisher(route_name, self, allowed_endpoints)
    }
}

/// Validates the consumer configuration for a route.
pub fn check_consumer(
    route_name: &str,
    endpoint: &Endpoint,
    allowed_types: Option<&[&str]>,
) -> Result<Vec<String>> {
    check_consumer_recursive(route_name, endpoint, 0, allowed_types)
}

fn check_consumer_recursive(
    route_name: &str,
    endpoint: &Endpoint,
    depth: usize,
    allowed_types: Option<&[&str]>,
) -> Result<Vec<String>> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(anyhow!(
            "Ref recursion depth exceeded limit of {}",
            MAX_DEPTH
        ));
    }
    let mut warnings = Vec::new();
    if endpoint.handler.is_some() {
        warnings.push(
            "Endpoint 'handler' is set on an input endpoint. Handlers are currently only supported on output endpoints (publishers) and will be ignored here."
            .to_string()
        );
    }

    if let Some(allowed) = allowed_types {
        if !endpoint.endpoint_type.is_core() {
            let name = endpoint.endpoint_type.name();
            if !allowed.contains(&name) {
                return Err(anyhow!(
                    "[route:{}] Endpoint type '{}' is not allowed by policy",
                    route_name,
                    name
                ));
            }
        }
    }
    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            let referenced = crate::route::get_endpoint(name).ok_or_else(|| {
                anyhow!(
                    "[route:{}] Referenced endpoint '{}' not found",
                    route_name,
                    name
                )
            })?;
            // We need to check the referenced endpoint, but we don't need to merge middlewares
            // for the check itself, as we just want to validate the core type.
            // However, to be thorough, we recurse on the referenced endpoint.
            // Note: This check ignores the middlewares on the 'ref' itself, which is acceptable for type checking.
            warnings.extend(check_consumer_recursive(
                route_name,
                &referenced,
                depth + 1,
                allowed_types,
            )?);
            Ok(warnings)
        }
        #[cfg(feature = "aws")]
        EndpointType::Aws(cfg) => {
            if cfg.topic_arn.is_some() {
                warnings.push(
                    "Endpoint 'aws' is used as a consumer, but 'topic_arn' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            if cfg.delayed_ack {
                warnings.push(
                    "Endpoint 'kafka' is used as a consumer, but 'delayed_ack' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.producer_options.is_some() {
                warnings.push(
                    "Endpoint 'kafka' is used as a consumer, but 'producer_options' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            if cfg.stream.is_none() {
                return Err(anyhow!(
                    "[route:{}] NATS consumer must specify a 'stream'",
                    route_name
                ));
            }
            if cfg.request_reply {
                warnings.push(
                    "Endpoint 'nats' is used as a consumer, but 'request_reply' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.request_timeout_ms.is_some() {
                warnings.push(
                    "Endpoint 'nats' is used as a consumer, but 'request_timeout_ms' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.delayed_ack {
                warnings.push(
                    "Endpoint 'nats' is used as a consumer, but 'delayed_ack' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.stream_max_messages.is_some() {
                warnings.push(
                    "Endpoint 'nats' is used as a consumer, but 'stream_max_messages' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.stream_max_bytes.is_some() {
                warnings.push(
                    "Endpoint 'nats' is used as a consumer, but 'stream_max_bytes' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            if cfg.delayed_ack {
                warnings.push(
                    "Endpoint 'amqp' is used as a consumer, but 'delayed_ack' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            if cfg.delayed_ack {
                warnings.push(
                    "Endpoint 'mqtt' is used as a consumer, but 'delayed_ack' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
        EndpointType::ZeroMq(_) => Ok(warnings),
        #[cfg(feature = "redis-streams")]
        EndpointType::RedisStreams(cfg) => {
            if cfg.maxlen.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a consumer, but 'maxlen' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.approx_trim.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a consumer, but 'approx_trim' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
        EndpointType::IbmMq(_) => Ok(warnings),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            use crate::models::MongoConsume;
            let mode = cfg.resolved_consume();
            if mode == MongoConsume::Snapshot && cfg.cursor_id.is_some() {
                return Err(anyhow!(
                    "[route:{}] MongoDB 'snapshot' does not support 'cursor_id'. Resuming above a stored \
                     `_id` skips anything a concurrent writer commits below it, which is silent data loss. \
                     Use 'capture_all' on a replica set to read incrementally.",
                    route_name
                ));
            }
            if cfg.consume.is_some() && cfg.change_stream {
                warnings.push(
                    "Endpoint 'mongodb' sets 'consume'; the deprecated 'change_stream' boolean is ignored and should be removed."
                    .to_string(),
                );
            } else if cfg.change_stream {
                warnings.push(
                    "Endpoint 'mongodb' option 'change_stream' is deprecated; use 'consume: capture_new'."
                    .to_string(),
                );
            }
            if cfg.reply_polling_ms.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a consumer, but 'reply_polling_ms' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.request_reply {
                warnings.push(
                    "Endpoint 'mongodb' is used as a consumer, but 'request_reply' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.request_timeout_ms.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a consumer, but 'request_timeout_ms' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.ttl_seconds.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a consumer, but 'ttl_seconds' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.capped_size_bytes.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a consumer, but 'capped_size_bytes' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(_) => Ok(warnings),
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => {
            if cfg.batch_concurrency.is_some() {
                warnings.push("Endpoint 'http' is used as a consumer, but 'batch_concurrency' is a publisher-only option and will be ignored.".to_string());
            }
            if cfg.tcp_keepalive_ms.is_some() {
                warnings.push("Endpoint 'http' is used as a consumer, but 'tcp_keepalive_ms' is a publisher-only option and will be ignored.".to_string());
            }
            if cfg.pool_idle_timeout_ms.is_some() {
                warnings.push(
                        "Endpoint 'http' is used as a consumer, but 'pool_idle_timeout_ms' is a publisher-only option and will be ignored."
                        .to_string(),
                    );
            }
            if cfg.stream_response_to.is_some() {
                warnings.push("Endpoint 'http' is used as a consumer, but 'stream_response_to' is a publisher-only option and will be ignored.".to_string());
            }
            Ok(warnings)
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => {
            if cfg.cursor_column.is_none() {
                return Err(anyhow!(
                    "ClickHouse endpoint used as a consumer requires 'cursor_column' (ClickHouse has no native queue; only non-destructive cursor reads are supported)."
                ));
            }
            if cfg.columns.is_some() {
                warnings.push("Endpoint 'clickhouse' is used as a consumer, but 'columns' is a publisher-only option and will be ignored.".to_string());
            }
            if cfg.async_insert {
                warnings.push("Endpoint 'clickhouse' is used as a consumer, but 'async_insert' is a publisher-only option and will be ignored.".to_string());
            }
            Ok(warnings)
        }
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => {
            if cfg.insert_query.is_some() {
                warnings.push(
                    "Endpoint 'sqlx' is used as a consumer, but 'insert_query' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "postgres-cdc")]
        EndpointType::PostgresCdc(cfg) => {
            if cfg.publication.trim().is_empty() {
                return Err(anyhow!(
                    "postgres_cdc consumer requires a 'publication' (defines which tables are captured)."
                ));
            }
            Ok(warnings)
        }
        #[cfg(feature = "sled")]
        EndpointType::Sled(_) => Ok(warnings),
        EndpointType::Static(_) => Ok(warnings),
        EndpointType::Memory(cfg) => {
            if cfg.request_reply {
                warnings.push(
                    "Endpoint 'memory' is used as a consumer, but 'request_reply' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.request_timeout_ms.is_some() {
                warnings.push(
                    "Endpoint 'memory' is used as a consumer, but 'request_timeout_ms' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        EndpointType::StreamBuffer(cfg) => {
            if cfg.correlation_id.is_none() {
                return Err(anyhow!(
                    "[route:{}] stream_buffer consumer must specify 'correlation_id'",
                    route_name
                ));
            }
            Ok(warnings)
        }
        EndpointType::File(_) => Ok(warnings),
        EndpointType::DirSpool(cfg) => {
            dir_spool::validate_spool_layout(cfg)
                .map_err(|error| anyhow!("[route:{route_name}] {error}"))?;
            if !matches!(cfg.emit_done, SpoolDone::Never) {
                warnings.push(
                    "Endpoint 'dir_spool' is used as a consumer, but 'emit_done' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.naming_pattern != crate::models::defaults::default_spool_naming_pattern() {
                warnings.push(
                    "Endpoint 'dir_spool' is used as a consumer, but 'naming_pattern' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            if !cfg.atomic {
                warnings.push(
                    "Endpoint 'dir_spool' is used as a consumer, but 'atomic' is a publisher-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => {
            if cfg.extension.is_some() {
                warnings.push("Endpoint 'object_store' is used as a consumer, but 'extension' is a publisher-only option and will be ignored.".to_string());
            }
            if cfg.date_partition.is_some() {
                warnings.push("Endpoint 'object_store' is used as a consumer, but 'date_partition' is a publisher-only option and will be ignored.".to_string());
            }
            Ok(warnings)
        }
        #[cfg(feature = "websocket")]
        EndpointType::WebSocket(_) => Ok(warnings),
        EndpointType::Custom { .. } => Ok(warnings),
        EndpointType::Switch(_) => Err(anyhow!(
            "[route:{}] Switch endpoint is only supported as an output",
            route_name
        )),
        EndpointType::Reader(_) => Err(anyhow!(
            "[route:{}] Reader endpoint is only supported as an output",
            route_name
        )),
        #[allow(unreachable_patterns)]
        _ => {
            if let Some(allowed) = allowed_types {
                let name = endpoint.endpoint_type.name();
                if allowed.contains(&name) {
                    return Ok(warnings);
                }
            }
            Err(anyhow!(
                "[route:{}] Unsupported consumer endpoint type '{:?}'",
                route_name,
                endpoint.endpoint_type
            ))
        }
    }
}

fn resolve_endpoint(endpoint: &Endpoint, route_name: &str) -> Result<Endpoint> {
    let mut visited = std::collections::HashSet::new();
    resolve_endpoint_recursive(endpoint, route_name, &mut visited)
}

fn resolve_endpoint_recursive(
    endpoint: &Endpoint,
    route_name: &str,
    visited: &mut std::collections::HashSet<String>,
) -> Result<Endpoint> {
    const MAX_DEPTH: usize = 16;
    if visited.len() > MAX_DEPTH {
        return Err(anyhow!(
            "Reference recursion depth exceeded limit of {}",
            MAX_DEPTH
        ));
    }

    if let EndpointType::Ref(name) = &endpoint.endpoint_type {
        if !visited.insert(name.clone()) {
            return Err(anyhow!(
                "[route:{}] Circular reference detected for endpoint '{}'",
                route_name,
                name
            ));
        }

        let referenced_endpoint = crate::route::get_endpoint(name).ok_or_else(|| {
            anyhow!(
                "[route:{}] Referenced endpoint '{}' not found",
                route_name,
                name
            )
        })?;

        let mut resolved = resolve_endpoint_recursive(&referenced_endpoint, route_name, visited)?;
        // Merge middlewares: The ref's middlewares should be outer (applied last in the rev() loop).
        // Since apply_middlewares_to_consumer iterates in reverse, we prepend the ref's middlewares.
        let mut new_middlewares = endpoint.middlewares.clone();
        new_middlewares.extend(resolved.middlewares);
        resolved.middlewares = new_middlewares;
        Ok(resolved)
    } else {
        Ok(endpoint.clone())
    }
}

/// Map a `sqlx` consumer config that requested CDC (via `publication`) onto a
/// `PostgresCdcConfig`, so a Postgres `sqlx` endpoint can transparently fall back
/// to logical-replication CDC. Postgres-only; other drivers error.
#[cfg(all(feature = "sqlx", feature = "postgres-cdc"))]
fn sqlx_cfg_to_cdc(
    cfg: &crate::models::SqlxConfig,
) -> anyhow::Result<crate::models::PostgresCdcConfig> {
    let url = cfg.url.trim();
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(anyhow!(
            "sqlx `publication` (CDC) is only supported for PostgreSQL URLs; got '{}'. \
             Use a postgres:// URL or the dedicated `postgres_cdc` endpoint.",
            cfg.url
        ));
    }
    if cfg.cursor_column.is_some() {
        return Err(anyhow!(
            "sqlx endpoint sets both `publication` (CDC) and `cursor_column` (polling); pick one."
        ));
    }
    Ok(crate::models::PostgresCdcConfig {
        url: cfg.url.clone(),
        publication: cfg.publication.clone().unwrap_or_default(),
        source_metadata: false,
        slot_name: cfg
            .slot_name
            .clone()
            .unwrap_or_else(|| "mq_bridge_slot".to_string()),
        create_slot: true,
        create_publication: cfg.create_publication,
        publication_tables: if cfg.create_publication {
            vec![cfg.table.clone()]
        } else {
            Vec::new()
        },
        temporary_slot: false,
        cursor_id: cfg.cursor_id.clone(),
        checkpoint_store: cfg.checkpoint_store.clone(),
        status_interval_ms: 10_000,
        tls: cfg.tls.clone(),
    })
}

/// Creates a `MessageConsumer` based on the route's "in" configuration.
pub async fn create_consumer_from_route(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<Box<dyn MessageConsumer>> {
    create_consumer_from_route_with_source_metadata(route_name, endpoint, false).await
}

/// Create a route consumer with source positions required by an idempotent output.
pub async fn create_consumer_from_route_with_source_metadata(
    route_name: &str,
    endpoint: &Endpoint,
    source_metadata_required: bool,
) -> Result<Box<dyn MessageConsumer>> {
    create_consumer_from_route_with_policy(route_name, endpoint, source_metadata_required, false)
        .await
}

/// Create a route consumer with the requested source metadata and resume policy.
pub(crate) async fn create_consumer_from_route_with_policy(
    route_name: &str,
    endpoint: &Endpoint,
    source_metadata_required: bool,
    no_resume: bool,
) -> Result<Box<dyn MessageConsumer>> {
    let resolved_endpoint = resolve_endpoint(endpoint, route_name)?;
    check_consumer(route_name, &resolved_endpoint, None)?;
    if source_metadata_required && !supports_source_metadata(&resolved_endpoint.endpoint_type) {
        return Err(source_position_unavailable(route_name));
    }
    let source_metadata =
        source_metadata_required || source_metadata_requested(&resolved_endpoint.endpoint_type);
    let consumer =
        create_base_consumer(route_name, &resolved_endpoint, source_metadata, no_resume).await?;
    apply_middlewares_to_consumer(consumer, &resolved_endpoint, route_name).await
}

fn source_position_unavailable(route_name: &str) -> anyhow::Error {
    anyhow!(
        "[route:{route_name}] 'name_by: source_position' on a file/object_store output requires an input that carries a replay position: kafka, postgres_cdc, sqlx, mongodb (change stream) or file"
    )
}

/// Rejects a `name_by: source_position` output whose input cannot stamp one. The same check
/// guards [`create_consumer_from_route_with_source_metadata`]; running it first keeps a route
/// that is about to be rejected from opening the sink, which for `file` would create the part
/// directory at `path`.
pub(crate) fn check_source_position_available(
    route_name: &str,
    input: &Endpoint,
    source_metadata_required: bool,
) -> Result<()> {
    if !source_metadata_required {
        return Ok(());
    }
    let resolved = resolve_endpoint(input, route_name)?;
    if !supports_source_metadata(&resolved.endpoint_type) {
        return Err(source_position_unavailable(route_name));
    }
    Ok(())
}

/// Sources whose `mqb.src.*` keys [`SourcePosition`](crate::support::source_ranges::SourcePosition)
/// can turn into a replay position. NATS and AMQP also emit provenance under `source_metadata`,
/// but a routing key is not a replayable offset, so they do not belong here — admitting them
/// would start the route and then fail on the first message.
pub(crate) fn supports_source_metadata(endpoint_type: &EndpointType) -> bool {
    matches!(
        endpoint_type,
        EndpointType::Kafka(_) | EndpointType::PostgresCdc(_)
    ) || matches!(endpoint_type, EndpointType::Sqlx(config) if config.publication.is_some())
        // A polling cursor is itself a replay position, provided it is a unique integer;
        // the reader rejects text and repeated values rather than naming records wrongly.
        || matches!(endpoint_type, EndpointType::Sqlx(config) if config.cursor_column.is_some())
        // The change-stream reader positions changes by cluster time and its initial
        // snapshot by `_id` scan index. The `consumer`/`snapshot` readers have neither.
        || matches!(
            endpoint_type,
            EndpointType::MongoDb(config) if matches!(
                config.consume,
                Some(crate::models::MongoConsume::CaptureNew)
                    | Some(crate::models::MongoConsume::CaptureAll)
                    | None
            )
        )
        // Every file mode positions records by index. Only `consume` reproduces that index
        // across runs; the others add a run epoch, which keeps them ordered and distinct
        // but not deduplicated. That is a weaker guarantee, not an invalid setup.
        || matches!(endpoint_type, EndpointType::File(_))
}

fn source_metadata_requested(endpoint_type: &EndpointType) -> bool {
    match endpoint_type {
        EndpointType::Kafka(config) => config.source_metadata,
        EndpointType::Nats(config) => config.source_metadata,
        EndpointType::Amqp(config) => config.source_metadata,
        EndpointType::PostgresCdc(config) => config.source_metadata,
        EndpointType::MongoDb(config) => config.source_metadata,
        EndpointType::File(config) => config.source_metadata,
        // Provenance only, not a replay position — so `dir_spool` is deliberately absent
        // from `supports_source_metadata` above. A chunk name orders the queue but says
        // nothing an idempotent sink could re-derive after the chunk has been drained.
        EndpointType::DirSpool(config) => config.source_metadata,
        EndpointType::Sqlx(config) => config.source_metadata,
        _ => false,
    }
}

/// Whether a route's resolved output contains a sink that names what it writes after the
/// source position, and therefore needs the input to stamp one.
pub fn output_requires_source_metadata(
    route_name: &str,
    endpoint: &Endpoint,
    source_has_position: bool,
) -> Result<bool> {
    output_has_sink(route_name, endpoint, &|endpoint_type| {
        let name_by = match endpoint_type {
            EndpointType::File(config) => config.resolved_name_by(source_has_position),
            EndpointType::ObjectStore(config) => config.resolved_name_by(source_has_position),
            _ => return false,
        };
        name_by == NameBy::SourcePosition
    })
}

/// Whether a route's resolved output contains an object-store sink that names objects by write
/// time. Key order is the only order a bucket has, and above `concurrency: 1` write order is
/// worker arrival order rather than source order.
pub fn output_has_write_time_named_object_store(
    route_name: &str,
    endpoint: &Endpoint,
    source_has_position: bool,
) -> Result<bool> {
    output_has_sink(
        route_name,
        endpoint,
        &|endpoint_type| matches!(endpoint_type, EndpointType::ObjectStore(config) if config.resolved_name_by(source_has_position) == NameBy::WriteTime),
    )
}

/// Names the middleware on `endpoint` that removes messages from a batch, if any.
///
/// Only a middleware that drops a message outright counts. One that delays, retries or
/// rewrites a message leaves the batch dense, which is what matters here.
fn row_dropping_middleware(endpoint: &Endpoint) -> Option<&'static str> {
    endpoint
        .middlewares
        .iter()
        .find_map(|middleware| match middleware {
            Middleware::Filter(_) => Some("the `filter` middleware"),
            Middleware::Deduplication(_) => Some("the `deduplication` middleware"),
            // One message per group carrying the first member's position, so a group of
            // four leaves three holes behind every survivor.
            Middleware::WeakJoin(_) => Some("the `weak_join` middleware"),
            // A transform with no configured stage never rejects anything.
            Middleware::Transform(config)
                if config.on_error == TransformErrorPolicy::Reject
                    && !(config.mapping.is_empty()
                        && config.expression.is_none()
                        && config.schema.is_none()
                        && config.schema_file.is_none()) =>
            {
                Some("`transform` with `on_error: reject`")
            }
            _ => None,
        })
}

/// Takes a row-dropping route's object-store sinks off `source_position` naming.
///
/// `name_by: auto` resolves to `source_position` whenever the input stamps one, naming each
/// object after a *contiguous* source range. A batch with holes in it is therefore written as
/// one object per surviving run: a filter keeping 80% of rows at random turns one PUT into
/// roughly a hundred, measured at a 220x slowdown. Renumbering positions to close the holes
/// would forge the replay identity the naming exists to provide, so the honest move is to stop
/// claiming it.
///
/// Only `auto` is relaxed — an explicit `source_position`, or the deprecated `idempotency`
/// alias, is a request for replay-safe names, holes and all. The `file` sink is untouched:
/// its `auto` never resolves to `source_position`. A sink behind a `ref` is also left alone,
/// since the referenced endpoint is resolved from the registry when the publisher is built
/// and never sees this copy.
///
/// Returns the rewritten output and the line to tell the operator, or `None` when nothing
/// needed relaxing.
pub fn relax_object_naming(
    route_name: &str,
    source_has_position: bool,
    input: &Endpoint,
    output: &Endpoint,
) -> Result<Option<(Endpoint, String)>> {
    if !source_has_position {
        return Ok(None);
    }

    fn visit(
        route_name: &str,
        endpoint: &mut Endpoint,
        dropped_by: Option<&'static str>,
        depth: usize,
        relaxed: &mut Option<String>,
    ) -> Result<()> {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return Err(anyhow!(
                "[route:{route_name}] output recursion depth exceeded limit of {MAX_DEPTH}"
            ));
        }
        // A dropper anywhere above a sink makes every batch beneath it sparse.
        let dropped_by = dropped_by.or_else(|| row_dropping_middleware(endpoint));
        match &mut endpoint.endpoint_type {
            EndpointType::ObjectStore(config) => {
                let Some(dropped_by) = dropped_by else {
                    return Ok(());
                };
                if config.name_by != NameBy::Auto || config.idempotency.is_some() {
                    return Ok(());
                }
                config.name_by = NameBy::WriteTime;
                relaxed.get_or_insert_with(|| {
                    format!(
                        "{dropped_by} removes messages from each batch, which would leave every \
                         object covering one contiguous source range and split each batch into \
                         many small objects. Naming objects by write time instead; set \
                         name_by: source_position to keep replay-safe names at that cost."
                    )
                });
            }
            EndpointType::Fanout(outputs) => {
                for output in outputs {
                    visit(route_name, output, dropped_by, depth + 1, relaxed)?;
                }
            }
            EndpointType::Switch(config) => {
                // Either switch mode without a `default` drops whatever matches nothing.
                let dropped_by = dropped_by.or_else(|| {
                    ((!config.when.is_empty() || !config.cases.is_empty())
                        && config.default.is_none())
                    .then_some("a `switch` with no `default`")
                });
                for output in config.cases.values_mut() {
                    visit(route_name, output, dropped_by, depth + 1, relaxed)?;
                }
                for case in &mut config.when {
                    visit(route_name, &mut case.to, dropped_by, depth + 1, relaxed)?;
                }
                if let Some(output) = config.default.as_deref_mut() {
                    visit(route_name, output, dropped_by, depth + 1, relaxed)?;
                }
            }
            EndpointType::Reader(output) => {
                visit(route_name, output, dropped_by, depth + 1, relaxed)?;
            }
            EndpointType::Request(config) => {
                visit(route_name, &mut config.to, dropped_by, depth + 1, relaxed)?;
                visit(
                    route_name,
                    &mut config.forward_to,
                    dropped_by,
                    depth + 1,
                    relaxed,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    let mut candidate = output.clone();
    let mut relaxed = None;
    let mut resolved_input = input.clone();
    for _ in 0..16 {
        let EndpointType::Ref(name) = &resolved_input.endpoint_type else {
            break;
        };
        let Some(referenced) = get_endpoint(name) else {
            break;
        };
        resolved_input = referenced;
    }
    visit(
        route_name,
        &mut candidate,
        row_dropping_middleware(input).or_else(|| row_dropping_middleware(&resolved_input)),
        0,
        &mut relaxed,
    )?;
    Ok(relaxed.map(|reason| (candidate, reason)))
}

/// Walk a resolved output tree — through `fanout`, `switch`, `reader`, `request` and `ref` —
/// and report whether any leaf sink matches.
fn output_has_sink(
    route_name: &str,
    endpoint: &Endpoint,
    matches_leaf: &dyn Fn(&EndpointType) -> bool,
) -> Result<bool> {
    fn visit(
        route_name: &str,
        endpoint: &Endpoint,
        depth: usize,
        visited_refs: &mut std::collections::HashSet<String>,
        matches_leaf: &dyn Fn(&EndpointType) -> bool,
    ) -> Result<bool> {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return Err(anyhow!(
                "[route:{route_name}] output recursion depth exceeded limit of {MAX_DEPTH}"
            ));
        }
        match &endpoint.endpoint_type {
            EndpointType::Fanout(outputs) => outputs
                .iter()
                .map(|output| visit(route_name, output, depth + 1, visited_refs, matches_leaf))
                .collect::<Result<Vec<_>>>()
                .map(|requirements| requirements.into_iter().any(|required| required)),
            EndpointType::Switch(config) => {
                let cases_required = config
                    .cases
                    .values()
                    .chain(config.when.iter().map(|case| &case.to))
                    .map(|output| visit(route_name, output, depth + 1, visited_refs, matches_leaf))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .any(|required| required);
                if cases_required {
                    Ok(true)
                } else {
                    config.default.as_deref().map_or(Ok(false), |output| {
                        visit(route_name, output, depth + 1, visited_refs, matches_leaf)
                    })
                }
            }
            EndpointType::Reader(output) => {
                visit(route_name, output, depth + 1, visited_refs, matches_leaf)
            }
            EndpointType::Request(config) => Ok(visit(
                route_name,
                &config.to,
                depth + 1,
                visited_refs,
                matches_leaf,
            )? || visit(
                route_name,
                &config.forward_to,
                depth + 1,
                visited_refs,
                matches_leaf,
            )?),
            EndpointType::Ref(name) => {
                if !visited_refs.insert(name.clone()) {
                    return Err(anyhow!(
                        "[route:{route_name}] circular output reference detected for endpoint '{name}'"
                    ));
                }
                let referenced = crate::route::get_endpoint(name).ok_or_else(|| {
                    anyhow!("[route:{route_name}] referenced output endpoint '{name}' not found")
                })?;
                let required = visit(
                    route_name,
                    &referenced,
                    depth + 1,
                    visited_refs,
                    matches_leaf,
                );
                visited_refs.remove(name);
                required
            }
            leaf => Ok(matches_leaf(leaf)),
        }
    }

    visit(
        route_name,
        endpoint,
        0,
        &mut std::collections::HashSet::new(),
        matches_leaf,
    )
}

pub(crate) fn output_passes_through_http_status(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<bool> {
    output_has_sink(
        route_name,
        endpoint,
        &|endpoint_type| {
            !matches!(endpoint_type, EndpointType::Http(cfg) if cfg.pass_through_status)
        },
    )
    .map(|has_non_opted_in_sink| !has_non_opted_in_sink)
}

pub(crate) async fn try_run_fast_path_route(
    route: &crate::models::Route,
    name: &str,
    shutdown_rx: async_channel::Receiver<()>,
    ready_tx: Option<async_channel::Sender<()>>,
) -> Option<anyhow::Result<bool>> {
    #[cfg(feature = "http")]
    {
        // The inline fast path applies to outputs that reply synchronously
        // without the route worker/disposition pipeline: `response` (handler- or
        // request-derived reply) and `static` (a fixed, pre-rendered reply).
        let output_is_inline = matches!(
            route.output.endpoint_type,
            EndpointType::Response(_) | EndpointType::Static(_)
        );
        if let EndpointType::Http(cfg) = &route.input.endpoint_type {
            if output_is_inline
                && cfg.inline_response_fast_path_enabled()
                && route.input.middlewares.is_empty()
                && output_middlewares_allow_http_inline_fast_path(&route.output.middlewares)
                && !cfg.fire_and_forget
            {
                return Some(
                    run_http_inline_response_fast_path(
                        route,
                        name,
                        shutdown_rx,
                        ready_tx,
                        cfg.clone(),
                    )
                    .await,
                );
            }
        }
    }

    #[cfg(feature = "websocket")]
    {
        if let EndpointType::WebSocket(cfg) = &route.input.endpoint_type {
            match websocket_direct_route_support(route) {
                WebSocketDirectRouteSupport::Supported => {
                    return Some(
                        websocket::run_direct_response_route(
                            name,
                            cfg.clone(),
                            route.output.handler.clone(),
                            shutdown_rx,
                            ready_tx,
                        )
                        .await,
                    );
                }
                WebSocketDirectRouteSupport::Unsupported(reason) => match cfg.execution_mode {
                    crate::models::WebSocketExecutionMode::Auto => {
                        tracing::warn!(
                            route = name,
                            reason = reason,
                            "WebSocket route cannot run in direct mode; falling back to routed mode"
                        );
                    }
                    crate::models::WebSocketExecutionMode::DirectOnly => {
                        return Some(Err(anyhow!(
                            "WebSocket route '{}' is configured for direct_only, but direct mode is unsupported: {}",
                            name,
                            reason
                        )));
                    }
                    crate::models::WebSocketExecutionMode::Routed => {}
                },
            }
        }
    }

    let _ = route;
    let _ = name;
    let _ = shutdown_rx;
    let _ = ready_tx;
    None
}

#[cfg(feature = "websocket")]
enum WebSocketDirectRouteSupport {
    Supported,
    Unsupported(&'static str),
}

#[cfg(feature = "websocket")]
fn websocket_direct_route_support(route: &crate::models::Route) -> WebSocketDirectRouteSupport {
    let EndpointType::WebSocket(cfg) = &route.input.endpoint_type else {
        return WebSocketDirectRouteSupport::Unsupported("input is not websocket");
    };

    if cfg.execution_mode == crate::models::WebSocketExecutionMode::Routed {
        return WebSocketDirectRouteSupport::Unsupported("execution_mode is routed");
    }
    if !matches!(route.output.endpoint_type, EndpointType::Response(_)) {
        return WebSocketDirectRouteSupport::Unsupported("output is not response");
    }
    if !websocket_direct_route_options_allowed(&route.options) {
        return WebSocketDirectRouteSupport::Unsupported(
            "custom route options require routed mode",
        );
    }
    if !route.input.middlewares.is_empty() || !route.output.middlewares.is_empty() {
        return WebSocketDirectRouteSupport::Unsupported("middleware requires routed mode");
    }

    WebSocketDirectRouteSupport::Supported
}

#[cfg(feature = "websocket")]
fn websocket_direct_route_options_allowed(options: &crate::models::RouteOptions) -> bool {
    let mut defaults = crate::models::RouteOptions::default();
    defaults.description.clone_from(&options.description);
    options == &defaults
}

#[cfg(feature = "http")]
fn output_middlewares_allow_http_inline_fast_path(middlewares: &[Middleware]) -> bool {
    middlewares.iter().all(|middleware| {
        matches!(
            middleware,
            Middleware::Buffer(_)
                | Middleware::Delay(_)
                | Middleware::Limiter(_)
                | Middleware::Metrics(_)
        )
    })
}

#[cfg(feature = "http")]
async fn run_http_inline_response_fast_path(
    route: &crate::models::Route,
    name: &str,
    shutdown_rx: async_channel::Receiver<()>,
    ready_tx: Option<async_channel::Sender<()>>,
    http_config: crate::models::HttpConfig,
) -> anyhow::Result<bool> {
    let publisher = create_publisher_from_route(name, &route.output).await?;
    let consumer =
        http::HttpConsumer::new_with_inline_publisher(&http_config, Some(publisher.clone()))
            .await?;

    if let Err(err) = crate::route::run_publisher_connect_hook(name, &publisher).await {
        crate::route::run_publisher_disconnect_hook(
            name,
            &publisher,
            crate::traits::DisconnectOutcome::Failed,
        )
        .await;
        return Err(err);
    }
    if let Err(err) = crate::route::run_consumer_connect_hook(name, &consumer).await {
        crate::route::run_consumer_disconnect_hook(name, &consumer).await;
        crate::route::run_publisher_disconnect_hook(
            name,
            &publisher,
            crate::traits::DisconnectOutcome::Failed,
        )
        .await;
        return Err(err);
    }

    tracing::info!(
        route = name,
        has_output_handler = route.output.handler.is_some(),
        output_middlewares = route.output.middlewares.len(),
        "Running HTTP inline response fast path; bypassing the normal route consumer/worker/disposition pipeline while keeping the output publisher chain active"
    );
    tracing::debug!(
        route = name,
        "HTTP inline response fast path differences: no input middlewares, no fire-and-forget, only buffer/metrics output middlewares allowed, and unchanged request metadata is not echoed back as response headers"
    );
    if let Some(tx) = ready_tx {
        let _ = tx.send(()).await;
    }

    let stopped = shutdown_rx.recv().await.is_ok();
    if stopped {
        tracing::info!(
            "Shutdown signal received in HTTP inline response runner for route '{}'.",
            name
        );
    }
    crate::route::run_consumer_disconnect_hook(name, &consumer).await;
    // This runner only ever ends on shutdown: an HTTP listener has no natural end.
    crate::route::run_publisher_disconnect_hook(
        name,
        &publisher,
        crate::traits::DisconnectOutcome::Stopped,
    )
    .await;
    Ok(true)
}

/// `_source_metadata` is honoured by the types [`supports_source_metadata`] admits and by the
/// NATS and AMQP consumers, which stamp provenance that is not a replay position. The other
/// branches ignore the flag. Startup only rejects an input for missing replay positions when
/// `source_metadata_required` is set.
async fn create_base_consumer(
    route_name: &str,
    endpoint: &Endpoint,
    _source_metadata: bool,
    _no_resume: bool,
) -> Result<Box<dyn MessageConsumer>> {
    // Helper to coerce concrete consumers to the trait object, fixing type inference issues in the match block.
    fn boxed<T: MessageConsumer + 'static>(c: T) -> Box<dyn MessageConsumer> {
        Box::new(c)
    }

    match &endpoint.endpoint_type {
        #[cfg(feature = "aws")]
        EndpointType::Aws(cfg) => Ok(boxed(aws::AwsConsumer::new(cfg).await?)),
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let mut config = cfg.clone();
            if config.topic.is_none() {
                config.topic = Some(route_name.to_string());
            }
            Ok(boxed(
                kafka::KafkaConsumer::new_with_source_metadata(&config, _source_metadata).await?,
            ))
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let mut config = cfg.clone();
            if config.subject.is_none() {
                config.subject = Some(route_name.to_string());
            }
            Ok(boxed(
                nats::NatsConsumer::new_with_source_metadata(&config, _source_metadata).await?,
            ))
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let mut config = cfg.clone();
            if config.queue.is_none() {
                config.queue = Some(route_name.to_string());
            }
            Ok(boxed(
                amqp::AmqpConsumer::new_with_source_metadata(&config, _source_metadata).await?,
            ))
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            let mut config = cfg.clone();
            if config.topic.is_none() {
                config.topic = Some(route_name.to_string());
            }
            if config.client_id.is_none() && !config.clean_session {
                // For persistent sessions, default client_id to route_name if not provided
                config.client_id = Some(format!("{}-{}", crate::APP_NAME, route_name));
            }
            Ok(boxed(mqtt::MqttConsumer::new(&config).await?))
        }
        #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
        EndpointType::IbmMq(cfg) => {
            let mut config = cfg.clone();
            if config.queue.is_none() && config.topic.is_none() {
                config.queue = Some(route_name.to_string());
            }
            Ok(boxed(ibm_mq::IbmMqConsumer::new(&config).await?))
        }
        #[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
        EndpointType::ZeroMq(cfg) => zeromq::create_consumer(cfg).await,
        #[cfg(feature = "redis-streams")]
        EndpointType::RedisStreams(cfg) => {
            let mut config = cfg.clone();
            if config.stream.is_none() {
                config.stream = Some(route_name.to_string());
            }
            Ok(boxed(
                redis_streams::RedisStreamsConsumer::new(&config).await?,
            ))
        }
        EndpointType::File(cfg) => Ok(boxed(
            file::FileConsumer::new_with_source_metadata(cfg, _source_metadata).await?,
        )),
        EndpointType::DirSpool(cfg) => Ok(boxed(
            dir_spool::DirSpoolConsumer::new_with_source_metadata(cfg, _source_metadata).await?,
        )),
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => Ok(boxed(
            object_store::ObjectStoreConsumer::new_with_no_resume(cfg, _no_resume).await?,
        )),
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(cfg) => {
            let mut config = cfg.clone();
            if config.topic.is_none() {
                config.topic = Some(route_name.to_string());
            }
            Ok(boxed(grpc::GrpcConsumer::new(&config).await?))
        }
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => {
            if cfg.publication.is_some() {
                // A Postgres publication means CDC (logical replication), not polling —
                // delegate to the postgres_cdc consumer. See `sqlx_cfg_to_cdc`.
                #[cfg(feature = "postgres-cdc")]
                {
                    Ok(boxed(
                        postgres::PostgresCdcConsumer::new_with_source_metadata(
                            &sqlx_cfg_to_cdc(cfg)?,
                            _source_metadata,
                        )
                        .await?,
                    ))
                }
                #[cfg(not(feature = "postgres-cdc"))]
                {
                    Err(anyhow!(
                        "sqlx endpoint with `publication` set uses Postgres CDC, which requires \
                         the `postgres-cdc` feature to be enabled."
                    ))
                }
            } else if cfg.cursor_column.is_some() {
                // Non-destructive, resumable cursor read of an arbitrary table.
                Ok(boxed(
                    sqlx::SqlxCursorReader::new_with_source_metadata_and_no_resume(
                        cfg,
                        _source_metadata,
                        _no_resume,
                    )
                    .await?,
                ))
            } else {
                Ok(boxed(sqlx::SqlxConsumer::new(cfg).await?))
            }
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => {
            if cfg.cursor_column.is_some() {
                // ClickHouse has no native queue; only non-destructive cursor reads are supported.
                Ok(boxed(
                    clickhouse::ClickHouseCursorReader::new_with_no_resume(cfg, _no_resume).await?,
                ))
            } else {
                Err(anyhow::anyhow!(
                    "ClickHouse endpoint used as a consumer requires 'cursor_column' (ClickHouse has no native queue; only non-destructive cursor reads are supported)."
                ))
            }
        }
        #[cfg(feature = "postgres-cdc")]
        EndpointType::PostgresCdc(cfg) => Ok(boxed(
            postgres::PostgresCdcConsumer::new_with_source_metadata(cfg, _source_metadata).await?,
        )),
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => Ok(boxed(http::HttpConsumer::new(cfg).await?)),
        #[cfg(feature = "websocket")]
        EndpointType::WebSocket(cfg) => Ok(boxed(websocket::WebSocketConsumer::new(cfg).await?)),
        EndpointType::Static(cfg) => Ok(boxed(static_endpoint::StaticRequestConsumer::new(cfg)?)),
        EndpointType::Memory(cfg) => Ok(boxed(memory::MemoryConsumer::new_async(cfg).await?)),
        EndpointType::StreamBuffer(cfg) => {
            Ok(boxed(stream_buffer::StreamBufferConsumer::new(cfg)?))
        }
        #[cfg(feature = "sled")]
        EndpointType::Sled(cfg) => Ok(boxed(sled::SledConsumer::new(cfg)?)),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            use crate::models::MongoConsume;
            let mut config = cfg.clone();
            if config.collection.is_none() {
                config.collection = Some(route_name.to_string());
            }
            match config.resolved_consume() {
                MongoConsume::Consumer => {
                    // Durable queue drain (auto-uses a change stream when available, else polls).
                    Ok(boxed(mongodb::MongoDbConsumer::new(&config).await?))
                }
                MongoConsume::Snapshot => {
                    // One-shot, non-destructive read of what is already there. Ends on drain, so it
                    // never becomes a tail — see the `_id` ordering note on `CaptureAll` below.
                    Ok(boxed(mongodb::MongoDbIdReader::new(&config).await?))
                }
                MongoConsume::CaptureNew => {
                    // Watch an existing collection for changes from now on (needs a replica set;
                    // otherwise the change-stream open returns a clear error).
                    Ok(boxed(
                        mongodb::MongoDbChangeStreamReader::new_with_source_metadata_and_no_resume(
                            &config,
                            false,
                            _source_metadata,
                            _no_resume,
                        )
                        .await?,
                    ))
                }
                MongoConsume::CaptureAll => {
                    // Read existing documents first, then capture changes via a change stream.
                    // There is deliberately no standalone fallback: an `_id`-ordered read can only
                    // return documents above its high-water mark, so anything a concurrent writer
                    // commits below it is skipped for good. That loss is silent and unrecoverable,
                    // so refuse to start instead and name the two sound alternatives.
                    mongodb::MongoDbChangeStreamReader::new_with_source_metadata_and_no_resume(
                        &config,
                        true,
                        _source_metadata,
                        _no_resume,
                    )
                    .await
                    .map(boxed)
                    .map_err(|e| {
                        if mongodb::is_change_stream_unsupported(&e) {
                            anyhow!(
                                "[route:{}] MongoDB 'capture_all' needs a replica set (a single-node one is enough). \
                                 On a standalone mongod use 'consume: snapshot' for a one-shot non-destructive read, \
                                 or 'consume: consumer' for a destructive work queue: {}",
                                route_name,
                                e
                            )
                        } else {
                            e
                        }
                    })
                }
            }
        }
        EndpointType::Custom { name, config } => {
            let factory = get_endpoint_factory(name)
                .ok_or_else(|| anyhow!("Custom endpoint factory '{}' not found", name))?;
            factory.create_consumer(route_name, config).await
        }
        EndpointType::Switch(_) => Err(anyhow!(
            "[route:{}] Switch endpoint is only supported as an output",
            route_name
        )),
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported consumer endpoint type '{:?}'",
            route_name,
            endpoint.endpoint_type
        )),
    }
}

/// Validates the publisher configuration for a route.
pub fn check_publisher(
    route_name: &str,
    endpoint: &Endpoint,
    allowed_types: Option<&[&str]>,
) -> Result<Vec<String>> {
    check_publisher_recursive(route_name, endpoint, 0, allowed_types)
}

fn check_publisher_recursive(
    route_name: &str,
    endpoint: &Endpoint,
    depth: usize,
    allowed_types: Option<&[&str]>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if let Some(allowed) = allowed_types {
        if !endpoint.endpoint_type.is_core() {
            let name = endpoint.endpoint_type.name();
            if !allowed.contains(&name) {
                return Err(anyhow!(
                    "[route:{}] Endpoint type '{}' is not allowed by policy",
                    route_name,
                    name
                ));
            }
        }
    }
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(anyhow!(
            "Fanout recursion depth exceeded limit of {}",
            MAX_DEPTH
        ));
    }
    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            let referenced = crate::route::get_endpoint(name).ok_or_else(|| {
                anyhow!(
                    "[route:{}] Referenced endpoint '{}' not found in endpoint registry",
                    route_name,
                    name
                )
            });
            if let Ok(referenced) = referenced {
                warnings.extend(check_publisher_recursive(
                    route_name,
                    &referenced,
                    depth + 1,
                    allowed_types,
                )?);
                return Ok(warnings);
            }
            if crate::publisher::get_publisher(name).is_some() {
                return Ok(warnings);
            }
            Err(anyhow!(
                "[route:{}] Referenced endpoint '{}' not found in any registry",
                route_name,
                name
            ))
        }
        #[cfg(feature = "aws")]
        EndpointType::Aws(cfg) => {
            if cfg.max_messages.is_some() {
                warnings.push(
                    "Endpoint 'aws' is used as a publisher, but 'max_messages' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.wait_time_seconds.is_some() {
                warnings.push(
                    "Endpoint 'aws' is used as a publisher, but 'wait_time_seconds' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            if cfg.group_id.is_some() {
                warnings.push(
                    "Endpoint 'kafka' is used as a publisher, but 'group_id' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.consumer_options.is_some() {
                warnings.push(
                    "Endpoint 'kafka' is used as a publisher, but 'consumer_options' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            if cfg.stream.is_some() {
                warnings.push(
                    "Endpoint 'nats' is used as a publisher, but 'stream' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.subscriber_mode {
                warnings.push(
                    "Endpoint 'nats' is used as a publisher, but 'subscriber_mode' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.prefetch_count.is_some() {
                warnings.push(
                    "Endpoint 'nats' is used as a publisher, but 'prefetch_count' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            if cfg.subscribe_mode {
                warnings.push(
                    "Endpoint 'amqp' is used as a publisher, but 'subscribe_mode' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.prefetch_count.is_some() {
                warnings.push(
                    "Endpoint 'amqp' is used as a publisher, but 'prefetch_count' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            if cfg.clean_session {
                warnings.push(
                    "Endpoint 'mqtt' is used as a publisher, but 'clean_session' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
        EndpointType::ZeroMq(cfg) => {
            if cfg.topic.is_some() {
                warnings.push(
                    "Endpoint 'zeromq' is used as a publisher, but 'topic' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }

        #[cfg(feature = "http")]
        EndpointType::Http(_cfg) => {
            if _cfg.path.is_some() {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'path' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if _cfg.workers.is_some() {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'workers' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if _cfg.message_id_header.is_some() {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'message_id_header' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if _cfg.internal_buffer_size.is_some() {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'internal_buffer_size' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if _cfg.fire_and_forget {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'fire_and_forget' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if _cfg.receive_streamable {
                warnings.push(
                    "Endpoint 'http' is used as a publisher, but 'receive_streamable' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "redis-streams")]
        EndpointType::RedisStreams(cfg) => {
            if cfg.group.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'group' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.consumer_name.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'consumer_name' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.subscriber_mode {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'subscriber_mode' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.block_ms.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'block_ms' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.read_from_start {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'read_from_start' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.redelivery_timeout_ms.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'redelivery_timeout_ms' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.internal_buffer_size.is_some() {
                warnings.push(
                    "Endpoint 'redis_streams' is used as a publisher, but 'internal_buffer_size' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(_) => Ok(warnings),
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => {
            if cfg.select_query.is_some() {
                warnings.push(
                    "Endpoint 'sqlx' is used as a publisher, but 'select_query' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.delete_after_read {
                warnings.push(
                    "Endpoint 'sqlx' is used as a publisher, but 'delete_after_read' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.polling_interval_ms.is_some() {
                warnings.push(
                    "Endpoint 'sqlx' is used as a publisher, but 'polling_interval_ms' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => {
            if cfg.cursor_column.is_some() {
                warnings.push("Endpoint 'clickhouse' is used as a publisher, but 'cursor_column' is a consumer-only option and will be ignored.".to_string());
            }
            if cfg.checkpoint_store.is_some() {
                warnings.push("Endpoint 'clickhouse' is used as a publisher, but 'checkpoint_store' is a consumer-only option and will be ignored.".to_string());
            }
            if cfg.polling_interval_ms.is_some() {
                warnings.push("Endpoint 'clickhouse' is used as a publisher, but 'polling_interval_ms' is a consumer-only option and will be ignored.".to_string());
            }
            Ok(warnings)
        }
        #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
        EndpointType::IbmMq(cfg) => {
            if cfg.wait_timeout_ms != 1000 {
                warnings.push(
                    "Endpoint 'ibmmq' is used as a publisher, but 'wait_timeout_ms' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            if cfg.polling_interval_ms.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a publisher, but 'polling_interval_ms' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.change_stream {
                warnings.push(
                    "Endpoint 'mongodb' is used as a publisher, but 'change_stream' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.cursor_id.is_some() {
                warnings.push(
                    "Endpoint 'mongodb' is used as a publisher, but 'cursor_id' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        EndpointType::File(_) => Ok(warnings),
        EndpointType::DirSpool(cfg) => {
            for (set, option) in [
                (!cfg.drain_on_read, "drain_on_read"),
                (cfg.stop_on_done, "stop_on_done"),
                (cfg.source_metadata, "source_metadata"),
            ] {
                if set {
                    warnings.push(format!(
                        "Endpoint 'dir_spool' is used as a publisher, but '{option}' is a consumer-only option and will be ignored."
                    ));
                }
            }
            dir_spool::validate_spool_layout(cfg)
                .map_err(|error| anyhow!("[route:{route_name}] {error}"))?;
            // Sink-only: the front of a chunk name has to be the sequence, or the queue
            // loses both its order and its resume point.
            dir_spool::validate_naming_pattern(cfg)
                .map_err(|error| anyhow!("[route:{route_name}] {error}"))?;
            warnings.extend(dir_spool::naming_pattern_warning(cfg));
            Ok(warnings)
        }
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => {
            if cfg.checkpoint_store.is_some() {
                warnings.push("Endpoint 'object_store' is used as a publisher, but 'checkpoint_store' is a consumer-only option and will be ignored.".to_string());
            }
            if cfg.cursor_id.is_some() {
                warnings.push("Endpoint 'object_store' is used as a publisher, but 'cursor_id' is a consumer-only option and will be ignored.".to_string());
            }
            if cfg.polling_interval_ms.is_some() {
                warnings.push("Endpoint 'object_store' is used as a publisher, but 'polling_interval_ms' is a consumer-only option and will be ignored.".to_string());
            }
            if cfg.max_object_bytes.is_some() {
                warnings.push("Endpoint 'object_store' is used as a publisher, but 'max_object_bytes' is a consumer-only option and will be ignored.".to_string());
            }
            Ok(warnings)
        }
        #[cfg(feature = "websocket")]
        EndpointType::WebSocket(_) => Ok(warnings),
        EndpointType::Static(_) => Ok(warnings),
        EndpointType::Memory(cfg) => {
            if cfg.subscribe_mode {
                warnings.push(
                    "Endpoint 'memory' is used as a publisher, but 'subscribe_mode' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.enable_nack {
                warnings.push(
                    "Endpoint 'memory' is used as a publisher, but 'enable_nack' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        EndpointType::StreamBuffer(cfg) => {
            if cfg.correlation_id.is_some() {
                warnings.push(
                    "Endpoint 'stream_buffer' is used as a publisher, but 'correlation_id' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        #[cfg(feature = "sled")]
        EndpointType::Sled(cfg) => {
            if cfg.read_from_start {
                warnings.push(
                    "Endpoint 'sled' is used as a publisher, but 'read_from_start' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            if cfg.delete_after_read {
                warnings.push(
                    "Endpoint 'sled' is used as a publisher, but 'delete_after_read' is a consumer-only option and will be ignored."
                    .to_string()
                );
            }
            Ok(warnings)
        }
        EndpointType::Null => Ok(warnings),
        EndpointType::Fanout(endpoints) => {
            for endpoint in endpoints {
                warnings.extend(check_publisher_recursive(
                    route_name,
                    endpoint,
                    depth + 1,
                    allowed_types,
                )?);
            }
            Ok(warnings)
        }
        EndpointType::Switch(cfg) => {
            cfg.validate()?;
            for endpoint in cfg
                .cases
                .values()
                .chain(cfg.when.iter().map(|case| &case.to))
            {
                warnings.extend(check_publisher_recursive(
                    route_name,
                    endpoint,
                    depth + 1,
                    allowed_types,
                )?);
            }
            if let Some(endpoint) = &cfg.default {
                warnings.extend(check_publisher_recursive(
                    route_name,
                    endpoint,
                    depth + 1,
                    allowed_types,
                )?);
            }
            Ok(warnings)
        }
        EndpointType::Response(_) => Ok(warnings),
        EndpointType::Custom { .. } => Ok(warnings),
        EndpointType::Reader(inner) => check_consumer(route_name, inner, allowed_types),
        EndpointType::Request(cfg) => {
            warnings.extend(check_publisher_recursive(
                route_name,
                &cfg.to,
                depth + 1,
                allowed_types,
            )?);
            warnings.extend(check_publisher_recursive(
                route_name,
                &cfg.forward_to,
                depth + 1,
                allowed_types,
            )?);
            Ok(warnings)
        }
        #[allow(unreachable_patterns)]
        _ => {
            if let Some(allowed) = allowed_types {
                let name = endpoint.endpoint_type.name();
                if allowed.contains(&name) {
                    return Ok(warnings);
                }
            }
            Err(anyhow!(
                "[route:{}] Unsupported publisher endpoint type '{:?}'",
                route_name,
                endpoint.endpoint_type
            ))
        }
    }
}

/// Creates a `MessagePublisher` based on the route's "out" configuration.
///
/// Sinks that name what they write after the source position need to know whether the input
/// can supply one; without a route there is no input, so `name_by: auto` resolves to
/// `write_time`. Use [`create_publisher_from_route_with_source_position`] inside a route.
pub async fn create_publisher_from_route(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<Arc<dyn MessagePublisher>> {
    create_publisher_from_route_with_source_position(route_name, endpoint, false).await
}

/// Creates a route publisher, telling it whether the route's input stamps a replay position.
pub async fn create_publisher_from_route_with_source_position(
    route_name: &str,
    endpoint: &Endpoint,
    source_has_position: bool,
) -> Result<Arc<dyn MessagePublisher>> {
    check_publisher(route_name, endpoint, None)?;
    create_publisher_with_depth(
        route_name.to_string(),
        endpoint.clone(),
        0,
        source_has_position,
    )
    .await
}

fn create_publisher_with_depth(
    route_name: String,
    endpoint: Endpoint,
    depth: usize,
    source_has_position: bool,
) -> BoxFuture<'static, Result<Arc<dyn MessagePublisher>>> {
    Box::pin(async move {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return Err(anyhow!(
                "Fanout/Ref recursion depth exceeded limit of {}",
                MAX_DEPTH
            ));
        }

        if let EndpointType::Ref(name) = &endpoint.endpoint_type {
            let referenced_opt = crate::route::get_endpoint(name);

            if referenced_opt.is_none() {
                if let Some(pub_instance) = crate::publisher::get_publisher(name) {
                    let inner = pub_instance.inner();
                    let publisher: Box<dyn MessagePublisher> = Box::new(inner);
                    let publisher = crate::middleware::apply_middlewares_to_publisher(
                        publisher,
                        &endpoint,
                        &route_name,
                    )
                    .await?;
                    return Ok(wrap_handler(publisher, &endpoint));
                }
            }

            let referenced = referenced_opt.ok_or_else(|| {
                anyhow!(
                    "[route:{}] Referenced endpoint '{}' not found",
                    route_name,
                    name
                )
            })?;

            let mut merged = referenced;
            // Merge middlewares: The ref's middlewares should be outer (applied last).
            // Since apply_middlewares_to_publisher iterates forward, we append the ref's middlewares to the referenced ones.
            merged.middlewares.extend(endpoint.middlewares);

            if endpoint.handler.is_some() {
                if merged.handler.is_some() {
                    return Err(anyhow!("[route:{}] Both ref endpoint and referenced endpoint '{}' have handlers defined. This is ambiguous.", route_name, name));
                }
                merged.handler = endpoint.handler;
            }

            return create_publisher_with_depth(route_name, merged, depth + 1, source_has_position)
                .await;
        }

        let publisher = create_base_publisher(
            &route_name,
            &endpoint.endpoint_type,
            depth,
            source_has_position,
        )
        .await?;
        let publisher =
            crate::middleware::apply_middlewares_to_publisher(publisher, &endpoint, &route_name)
                .await?;
        Ok(wrap_handler(publisher, &endpoint))
    })
}

/// Puts the handler outside every output middleware, so it runs **once** per message and
/// the middlewares act on what it produced.
///
/// The handler used to sit innermost, which meant `retry` wrapped it: a sink that retried
/// four times ran the handler four times, so any handler with a side effect (a counter, a
/// capture, an enrichment call) fired N times for one message. Retrying only the publish
/// is what comparable pipelines do. The trade, accepted deliberately: a `dlq` on the
/// output no longer captures handler failures, only send failures — a handler error now
/// propagates to the route.
fn wrap_handler(
    publisher: Arc<dyn MessagePublisher>,
    endpoint: &Endpoint,
) -> Arc<dyn MessagePublisher> {
    match &endpoint.handler {
        Some(handler) => Arc::new(crate::command_handler::CommandPublisher::new(
            publisher,
            handler.clone(),
        )),
        None => publisher,
    }
}

async fn create_base_publisher(
    route_name: &str,
    endpoint_type: &EndpointType,
    depth: usize,
    source_has_position: bool,
) -> Result<Box<dyn MessagePublisher>> {
    let publisher = match endpoint_type {
        #[cfg(feature = "aws")]
        EndpointType::Aws(cfg) => {
            Ok(Box::new(aws::AwsPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let mut config = cfg.clone();
            if config.topic.is_none() {
                config.topic = Some(route_name.to_string());
            }
            Ok(Box::new(kafka::KafkaPublisher::new(&config).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let mut config = cfg.clone();
            if config.subject.is_none() {
                config.subject = Some(route_name.to_string());
            }
            Ok(Box::new(nats::NatsPublisher::new(&config).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let mut config = cfg.clone();
            if config.queue.is_none() {
                config.queue = Some(route_name.to_string());
            }
            Ok(Box::new(amqp::AmqpPublisher::new(&config).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            let mut config = cfg.clone();
            if config.topic.is_none() {
                config.topic = Some(route_name.to_string());
            }
            if config.client_id.is_none() {
                config.client_id = Some(format!("{}-{}", crate::APP_NAME, route_name));
            }
            Ok(Box::new(mqtt::MqttPublisher::new(&config).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
        EndpointType::ZeroMq(cfg) => zeromq::create_publisher(cfg).await,
        #[cfg(feature = "redis-streams")]
        EndpointType::RedisStreams(cfg) => {
            let mut config = cfg.clone();
            if config.stream.is_none() {
                config.stream = Some(route_name.to_string());
            }
            Ok(
                Box::new(redis_streams::RedisStreamsPublisher::new(&config).await?)
                    as Box<dyn MessagePublisher>,
            )
        }
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(cfg) => grpc::create_grpc_publisher(cfg).await,
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => {
            Ok(Box::new(sqlx::SqlxPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => {
            Ok(Box::new(clickhouse::ClickHousePublisher::new(cfg).await?)
                as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => {
            let stream_response_sink =
                if let Some(stream_response_to) = cfg.stream_response_to.as_deref() {
                    Some(
                        create_publisher_with_depth(
                            route_name.to_string(),
                            stream_response_to.clone(),
                            depth + 1,
                            source_has_position,
                        )
                        .await?,
                    )
                } else {
                    None
                };
            let sink =
                http::HttpPublisher::new_with_stream_response_sink(cfg, stream_response_sink)
                    .await?;
            Ok(Box::new(sink) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "websocket")]
        EndpointType::WebSocket(cfg) => {
            let sink = websocket::WebSocketPublisher::new(cfg);
            Ok(Box::new(sink) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            let mut config = cfg.clone();
            if config.collection.is_none() {
                config.collection = Some(route_name.to_string());
            }
            Ok(Box::new(mongodb::MongoDbPublisher::new(&config).await?)
                as Box<dyn MessagePublisher>)
        }
        EndpointType::File(cfg) => Ok(Box::new(
            file::FilePublisher::new_with_name_by(cfg, cfg.resolved_name_by(source_has_position))
                .await?,
        ) as Box<dyn MessagePublisher>),
        EndpointType::DirSpool(cfg) => {
            Ok(Box::new(dir_spool::DirSpoolPublisher::new(cfg).await?)
                as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => Ok(Box::new(
            object_store::ObjectStorePublisher::new_with_name_by(
                cfg,
                cfg.resolved_name_by(source_has_position),
            )
            .await?,
        ) as Box<dyn MessagePublisher>),
        EndpointType::Static(cfg) => Ok(Box::new(static_endpoint::StaticEndpointPublisher::new(
            cfg,
        )?) as Box<dyn MessagePublisher>),
        EndpointType::Memory(cfg) => {
            Ok(Box::new(memory::MemoryPublisher::new_async(cfg).await?)
                as Box<dyn MessagePublisher>)
        }
        EndpointType::StreamBuffer(cfg) => {
            Ok(Box::new(stream_buffer::StreamBufferPublisher::new(cfg)?)
                as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "sled")]
        EndpointType::Sled(cfg) => {
            Ok(Box::new(sled::SledPublisher::new(cfg)?) as Box<dyn MessagePublisher>)
        }
        #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
        EndpointType::IbmMq(cfg) => {
            Ok(Box::new(ibm_mq::IbmMqPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        EndpointType::Null => Ok(Box::new(null::NullPublisher) as Box<dyn MessagePublisher>),
        EndpointType::Fanout(endpoints) => {
            let mut publishers = Vec::with_capacity(endpoints.len());
            for endpoint in endpoints {
                let p = create_publisher_with_depth(
                    route_name.to_string(),
                    endpoint.clone(),
                    depth + 1,
                    source_has_position,
                )
                .await?;
                publishers.push(p);
            }
            Ok(Box::new(fanout::FanoutPublisher::new(publishers)) as Box<dyn MessagePublisher>)
        }
        EndpointType::Switch(cfg) => {
            cfg.validate()?;
            let mut cases = std::collections::HashMap::new();
            for (key, endpoint) in &cfg.cases {
                let p = create_publisher_with_depth(
                    route_name.to_string(),
                    endpoint.clone(),
                    depth + 1,
                    source_has_position,
                )
                .await?;
                cases.insert(key.clone(), p);
            }
            let default = if let Some(endpoint) = &cfg.default {
                Some(
                    create_publisher_with_depth(
                        route_name.to_string(),
                        (**endpoint).clone(),
                        depth + 1,
                        source_has_position,
                    )
                    .await?,
                )
            } else {
                None
            };
            if cfg.when.is_empty() {
                return Ok(Box::new(switch::SwitchPublisher::new(
                    cfg.metadata_key.clone(),
                    cases,
                    default,
                )) as Box<dyn MessagePublisher>);
            }

            #[cfg(not(feature = "filter"))]
            {
                Err(anyhow!(
                    "[{route_name}] switch `when` needs the `filter` feature, which this build does not have. Rebuild with `--features filter`, or use `metadata_key` + `cases`."
                ))
            }
            #[cfg(feature = "filter")]
            {
                use anyhow::Context as _;
                let mut predicates = Vec::with_capacity(cfg.when.len());
                for case in &cfg.when {
                    let filter = crate::middleware::filter::CompiledFilter::new(&case.condition)
                        .with_context(|| {
                            format!("[{route_name}] invalid switch `when` expression")
                        })?;
                    let publisher = create_publisher_with_depth(
                        route_name.to_string(),
                        case.to.clone(),
                        depth + 1,
                        source_has_position,
                    )
                    .await?;
                    predicates.push((filter, publisher));
                }
                Ok(
                    Box::new(switch::SwitchPublisher::new_predicate(predicates, default))
                        as Box<dyn MessagePublisher>,
                )
            }
        }
        EndpointType::Response(_) => {
            Ok(Box::new(response::ResponsePublisher) as Box<dyn MessagePublisher>)
        }
        EndpointType::Reader(inner) => {
            let consumer = create_consumer_from_route(route_name, inner).await?;
            Ok(Box::new(reader::ReaderPublisher::new(consumer)) as Box<dyn MessagePublisher>)
        }
        EndpointType::Request(cfg) => {
            let request = create_publisher_with_depth(
                route_name.to_string(),
                (*cfg.to).clone(),
                depth + 1,
                source_has_position,
            )
            .await?;
            let forward = create_publisher_with_depth(
                route_name.to_string(),
                (*cfg.forward_to).clone(),
                depth + 1,
                source_has_position,
            )
            .await?;
            Ok(
                Box::new(request::RequestForwardPublisher::new(request, forward))
                    as Box<dyn MessagePublisher>,
            )
        }
        EndpointType::Custom { name, config } => {
            let factory = get_endpoint_factory(name)
                .ok_or_else(|| anyhow!("Custom endpoint factory '{}' not found", name))?;
            factory.create_publisher(route_name, config).await
        }
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported publisher endpoint type '{:?}'",
            route_name,
            endpoint_type
        )),
    }?;
    Ok(publisher)
}

/// Returns the active process-level rustls `CryptoProvider`, or a descriptive error if none
/// has been installed yet.
///
/// This is called by every endpoint that creates a rustls `ClientConfig` / `ServerConfig`.
/// As a library, mq-bridge never installs a provider itself; the choice belongs to the
/// application binary.  To resolve the error, either:
///
/// * Enable the **`rustls-ring`** or **`rustls-aws-lc`** feature of `mq-bridge`, or
/// * Call `rustls::crypto::CryptoProvider::install_default()` early in your `main()`.
#[cfg(feature = "rustls")]
#[allow(unused)]
pub(crate) fn get_crypto_provider() -> anyhow::Result<std::sync::Arc<rustls::crypto::CryptoProvider>>
{
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .ok_or_else(|| {
            anyhow!("No rustls CryptoProvider is installed.\n\
Fix: enable the `rustls-ring` or `rustls-aws-lc` feature of mq-bridge, or call `rustls::crypto::CryptoProvider::install_default()` in your application binary before creating any TLS endpoint.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, EndpointType};
    use crate::CanonicalMessage;

    fn file_named_by(name_by: NameBy) -> Endpoint {
        let mut config = crate::models::FileConfig::new("/tmp/parts");
        config.name_by = name_by;
        Endpoint::new(EndpointType::File(config))
    }

    #[test]
    fn source_position_output_requires_source_metadata_through_fanout_and_switch() {
        let ordinary = Endpoint::new_memory("ordinary", 1);
        let file = file_named_by(NameBy::SourcePosition);

        let fanout = Endpoint::new(EndpointType::Fanout(vec![ordinary.clone(), file.clone()]));
        assert!(output_requires_source_metadata("test", &fanout, false).unwrap());

        let mut cases = std::collections::HashMap::new();
        cases.insert("archive".to_string(), file);
        let switch = Endpoint::new(EndpointType::Switch(crate::models::SwitchConfig {
            metadata_key: "kind".to_string(),
            cases,
            when: Vec::new(),
            default: Some(Box::new(ordinary)),
        }));
        assert!(output_requires_source_metadata("test", &switch, false).unwrap());
    }

    /// `auto` is what almost every config carries, so the traversal has to resolve it rather
    /// than treat it as "not source-positioned".
    #[cfg(feature = "object-store")]
    #[test]
    fn auto_naming_follows_what_the_input_can_stamp() {
        let bucket = Endpoint::new(EndpointType::ObjectStore(
            crate::models::ObjectStoreConfig {
                url: "s3://bucket/data".to_string(),
                ..Default::default()
            },
        ));
        let auto = Endpoint::new(EndpointType::Fanout(vec![bucket]));
        assert!(output_requires_source_metadata("test", &auto, true).unwrap());
        assert!(!output_requires_source_metadata("test", &auto, false).unwrap());

        // An explicit choice is not second-guessed by what the input happens to offer.
        let explicit = Endpoint::new(EndpointType::Fanout(vec![file_named_by(
            NameBy::SourcePosition,
        )]));
        assert!(output_requires_source_metadata("test", &explicit, false).unwrap());
    }

    /// The file sink keeps `auto` on `write_time`: `source_position` turns `path` from a file
    /// into a directory of parts, which is not something to derive for the operator.
    #[test]
    fn auto_naming_never_turns_a_file_sink_into_part_files() {
        let auto = Endpoint::new(EndpointType::Fanout(vec![file_named_by(NameBy::Auto)]));
        assert!(!output_requires_source_metadata("test", &auto, true).unwrap());
    }

    /// The deprecated `idempotency` alias still decides while `name_by` is `auto`, and loses
    /// to an explicit `name_by`.
    #[test]
    fn idempotency_alias_is_still_read_but_never_overrides_name_by() {
        let mut config = crate::models::FileConfig::new("/tmp/parts");
        config.idempotency = Some(true);
        assert_eq!(config.resolved_name_by(false), NameBy::SourcePosition);

        config.name_by = NameBy::WriteTime;
        assert_eq!(config.resolved_name_by(true), NameBy::WriteTime);
    }

    /// Only a write-time-named object store is worth warning about: a source-positioned one
    /// names objects by source position, so concurrency cannot reorder them.
    #[cfg(feature = "object-store")]
    #[test]
    fn write_time_named_object_store_is_detected_through_the_output_tree() {
        fn bucket(name_by: NameBy) -> Endpoint {
            Endpoint::new(EndpointType::ObjectStore(
                crate::models::ObjectStoreConfig {
                    url: "s3://bucket/data".to_string(),
                    name_by,
                    ..Default::default()
                },
            ))
        }

        let fanout = Endpoint::new(EndpointType::Fanout(vec![
            Endpoint::new_memory("ordinary", 1),
            bucket(NameBy::WriteTime),
        ]));
        assert!(output_has_write_time_named_object_store("test", &fanout, false).unwrap());

        let by_position = Endpoint::new(EndpointType::Fanout(vec![bucket(NameBy::SourcePosition)]));
        assert!(!output_has_write_time_named_object_store("test", &by_position, false).unwrap());
    }

    /// A polling cursor is a replay position, so an idempotent sink accepts it — where a
    /// plain `sqlx` queue read (no `cursor_column`) still has nothing to sequence by.
    #[cfg(feature = "sqlx")]
    #[test]
    fn sqlx_is_a_replay_source_only_in_cursor_or_cdc_mode() {
        let cursor = EndpointType::Sqlx(crate::models::SqlxConfig {
            table: "orders".to_string(),
            cursor_column: Some("id".to_string()),
            ..Default::default()
        });
        assert!(supports_source_metadata(&cursor));

        let queue = EndpointType::Sqlx(crate::models::SqlxConfig {
            table: "orders".to_string(),
            ..Default::default()
        });
        assert!(!supports_source_metadata(&queue));
    }

    #[tokio::test]
    async fn idempotent_output_rejects_a_source_without_replay_position() {
        let result = create_consumer_from_route_with_source_metadata(
            "idempotent-route",
            &Endpoint::new_memory("input", 1),
            true,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("memory has no durable source position"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("requires an input that carries a replay position"));
    }

    /// NATS emits `mqb.src.*` provenance but no replayable offset. The guard must reject it up
    /// front rather than let the route start and die on the first message. The rejection happens
    /// before any connection attempt, so this needs no broker.
    #[cfg(feature = "nats")]
    #[tokio::test]
    async fn idempotent_output_rejects_a_source_with_provenance_but_no_offset() {
        let nats = Endpoint::new(EndpointType::Nats(crate::models::NatsConfig {
            subject: Some("orders".to_string()),
            stream: Some("ORDERS".to_string()),
            source_metadata: true,
            ..Default::default()
        }));
        let result =
            create_consumer_from_route_with_source_metadata("idempotent-route", &nats, true).await;
        let error = match result {
            Ok(_) => panic!("a NATS subject is not a replay position"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("requires an input that carries a replay position"));
    }

    /// The route derives `source_metadata` from an idempotent output, so a plain `file`
    /// input must be stamped without the caller setting the flag on the source config.
    #[tokio::test]
    async fn file_input_is_stamped_when_the_output_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("orders.jsonl");
        std::fs::write(&input, "{\"id\":1}\n{\"id\":2}\n").unwrap();

        let file = Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
            input.to_string_lossy(),
        )));
        let mut consumer =
            create_consumer_from_route_with_source_metadata("idempotent-route", &file, true)
                .await
                .unwrap();

        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 2, "file input produced no rows");
        for (index, message) in batch.messages.iter().enumerate() {
            assert_eq!(
                message
                    .metadata
                    .get("mqb.src.file_path")
                    .map(String::as_str),
                Some(input.to_string_lossy().as_ref())
            );
            // The record index is the replay position, so it has to be the row's own index.
            assert_eq!(
                message
                    .metadata
                    .get("mqb.src.file_record")
                    .map(String::as_str),
                Some(index.to_string().as_str())
            );
            // `consume` reproduces its record index, so it carries no epoch.
            assert!(!message.metadata.contains_key("mqb.src.file_epoch"));
            assert!(crate::support::source_ranges::SourcePosition::from_message(message).is_ok());
        }
    }

    /// The epoch reads the same derived flag: a `group_subscribe` input restarts its record
    /// index, so without one the second run's names would collide with the first run's.
    #[tokio::test]
    async fn derived_source_metadata_also_gives_group_subscribe_a_run_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("orders.jsonl");
        std::fs::write(&input, "{\"id\":1}\n").unwrap();

        let file = Endpoint::new(EndpointType::File(crate::models::FileConfig {
            mode: Some(crate::models::FileConsumerMode::GroupSubscribe {
                group_id: "derived-epoch".to_string(),
                read_from_tail: false,
            }),
            ..crate::models::FileConfig::new(input.to_string_lossy())
        }));

        // Two runs of the same input: both restart the record index at 0, so only the epoch
        // keeps the second run's records from being named like the first run's.
        let mut positions = Vec::new();
        for _ in 0..2 {
            let mut consumer =
                create_consumer_from_route_with_source_metadata("idempotent-route", &file, true)
                    .await
                    .unwrap();
            let batch = consumer.receive_batch(10).await.unwrap();
            let first = batch.messages.first().expect("file input produced no rows");
            assert!(first.metadata.contains_key("mqb.src.file_epoch"));
            positions
                .push(crate::support::source_ranges::SourcePosition::from_message(first).unwrap());
        }

        assert_ne!(positions[0].source, positions[1].source);
        // A later run reads later records, so its objects must sort after the earlier run's.
        assert!(positions[0].source < positions[1].source);
    }

    #[tokio::test]
    async fn test_fanout_publisher_integration() {
        let ep1 = Endpoint::new_memory("fanout_1", 10);
        let ep2 = Endpoint::new_memory("fanout_2", 10);

        let chan1 = ep1.channel().unwrap();
        let chan2 = ep2.channel().unwrap();
        let fanout_ep = Endpoint::new(EndpointType::Fanout(vec![ep1, ep2]));

        let publisher = create_publisher_from_route("test_fanout", &fanout_ep)
            .await
            .expect("Failed to create fanout publisher");

        let msg = CanonicalMessage::new(b"fanout_payload".to_vec(), None);
        publisher.send(msg).await.expect("Failed to send message");

        assert_eq!(chan1.len(), 1);
        assert_eq!(chan2.len(), 1);

        let msg1 = chan1.drain_messages().pop().unwrap();
        let msg2 = chan2.drain_messages().pop().unwrap();

        assert_eq!(msg1.payload, "fanout_payload".as_bytes());
        assert_eq!(msg2.payload, "fanout_payload".as_bytes());
    }

    use crate::models::MemoryConfig;
    #[tokio::test]
    async fn test_factory_creates_memory_subscriber() {
        let endpoint = Endpoint {
            endpoint_type: EndpointType::Memory(
                MemoryConfig::new("mem".to_string(), None).with_subscribe(true),
            ),
            middlewares: vec![],
            handler: None,
        };

        let consumer = create_consumer_from_route("test", &endpoint).await.unwrap();
        // Check if it is a MemoryConsumer (MemorySubscriber was merged)
        let is_subscriber = consumer
            .as_any()
            .is::<crate::endpoints::memory::MemoryConsumer>();
        assert!(is_subscriber, "Factory should create MemoryConsumer");
    }

    #[cfg(feature = "websocket")]
    #[test]
    fn websocket_direct_route_support_requires_default_route_options() {
        let mut options = crate::models::RouteOptions::default();
        assert!(websocket_direct_route_options_allowed(&options));

        options.batch_size = 128;
        assert!(!websocket_direct_route_options_allowed(&options));
    }

    #[cfg(feature = "websocket")]
    #[test]
    fn websocket_direct_route_support_respects_execution_mode_and_output() {
        let input = Endpoint::new(EndpointType::WebSocket(
            crate::models::WebSocketConfig::new("127.0.0.1:0"),
        ));
        let response_route = crate::models::Route::new(input.clone(), Endpoint::new_response());
        assert!(matches!(
            websocket_direct_route_support(&response_route),
            WebSocketDirectRouteSupport::Supported
        ));

        let memory_route = crate::models::Route::new(input.clone(), Endpoint::new_memory("ws", 1));
        assert!(matches!(
            websocket_direct_route_support(&memory_route),
            WebSocketDirectRouteSupport::Unsupported("output is not response")
        ));

        let routed_input = Endpoint::new(EndpointType::WebSocket(
            crate::models::WebSocketConfig::new("127.0.0.1:0")
                .with_execution_mode(crate::models::WebSocketExecutionMode::Routed),
        ));
        let routed_route = crate::models::Route::new(routed_input, Endpoint::new_response());
        assert!(matches!(
            websocket_direct_route_support(&routed_route),
            WebSocketDirectRouteSupport::Unsupported("execution_mode is routed")
        ));
    }

    #[test]
    fn test_endpoint_middleware_ordering_helpers() {
        let endpoint = Endpoint::new_memory("test", 10)
            .with_metrics()
            .with_dlq(crate::models::DeadLetterQueueMiddleware::default())
            .with_retry(crate::models::RetryMiddleware::default());

        // Expected order: Retry, Dlq, Metrics
        assert_eq!(endpoint.middlewares.len(), 3);
        assert!(matches!(endpoint.middlewares[0], Middleware::Retry(_)));
        assert!(matches!(endpoint.middlewares[1], Middleware::Dlq(_)));
        assert!(matches!(endpoint.middlewares[2], Middleware::Metrics(_)));
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_http_inline_fast_path_allows_simple_output_publisher_middlewares() {
        assert!(output_middlewares_allow_http_inline_fast_path(&[
            Middleware::Buffer(crate::models::BufferMiddleware {
                max_messages: 16,
                max_delay_ms: 0,
            }),
            Middleware::Delay(crate::models::DelayMiddleware { delay_ms: 0 }),
            Middleware::Limiter(crate::models::LimiterMiddleware {
                messages_per_second: 1_000_000.0,
            }),
        ]));

        assert!(!output_middlewares_allow_http_inline_fast_path(&[
            Middleware::Retry(crate::models::RetryMiddleware::default()),
        ]));
        assert!(!output_middlewares_allow_http_inline_fast_path(&[
            Middleware::Dlq(Box::default()),
        ]));
    }

    #[test]
    fn test_consumer_middleware_ordering() {
        let endpoint = Endpoint::new_memory("test", 10)
            .with_deduplication(crate::models::DeduplicationMiddleware {
                store: None,
                sled_path: Some("".into()),
                ttl_seconds: 10,
                key: None,
            })
            .with_consumer_metrics();

        // Expected order in list: [Metrics, Dedup]
        // Consumer application (rev): Dedup -> Metrics.
        // Execution: Metrics( Dedup ( base ) ). Metrics is Outer.
        assert_eq!(endpoint.middlewares.len(), 2);
        assert!(matches!(endpoint.middlewares[0], Middleware::Metrics(_)));
        assert!(matches!(
            endpoint.middlewares[1],
            Middleware::Deduplication(_)
        ));
    }

    #[test]
    fn test_check_consumer_invalid_config() {
        let config = crate::models::MemoryConfig {
            topic: "test".to_string(),
            request_reply: true, // Invalid for consumer
            ..Default::default()
        };
        let endpoint = Endpoint::new(EndpointType::Memory(config));

        let warnings = check_consumer("test_route", &endpoint, None).unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.contains("request_reply") && w.contains("publisher-only")));
    }

    #[cfg(feature = "object-store")]
    mod relaxed_naming {
        use super::*;
        use crate::models::{
            DeduplicationMiddleware, Middleware, ObjectStoreConfig, TransformErrorPolicy,
            TransformMiddleware, WeakJoinMiddleware,
        };
        use crate::register_endpoint;

        fn bucket(name_by: NameBy) -> Endpoint {
            Endpoint::new(EndpointType::ObjectStore(ObjectStoreConfig {
                url: "s3://bucket/orders".to_string(),
                name_by,
                ..Default::default()
            }))
        }

        fn with_middleware(mut endpoint: Endpoint, middleware: Middleware) -> Endpoint {
            endpoint.middlewares.push(middleware);
            endpoint
        }

        fn filtered_source() -> Endpoint {
            with_middleware(
                Endpoint::new_memory("orders", 1),
                Middleware::Filter("amount > 100".to_string()),
            )
        }

        fn deduplication() -> Middleware {
            Middleware::Deduplication(DeduplicationMiddleware {
                store: None,
                sled_path: None,
                ttl_seconds: 60,
                key: None,
            })
        }

        fn rejecting_transform() -> Middleware {
            Middleware::Transform(TransformMiddleware {
                schema: Some(serde_json::json!({"type": "object"})),
                on_error: TransformErrorPolicy::Reject,
                ..Default::default()
            })
        }

        fn resolved_name_by(endpoint: &Endpoint) -> NameBy {
            match &endpoint.endpoint_type {
                EndpointType::ObjectStore(config) => config.name_by,
                other => panic!("not an object-store sink: {other:?}"),
            }
        }

        /// The measured case: a filter leaves holes in every batch, and each hole would
        /// start another object under source-range names.
        #[test]
        fn a_filter_takes_an_auto_named_object_store_off_source_position() {
            let (relaxed, reason) =
                relax_object_naming("test", true, &filtered_source(), &bucket(NameBy::Auto))
                    .unwrap()
                    .expect("naming relaxed");

            assert!(reason.contains("`filter`"), "names the cause: {reason}");
            assert_eq!(resolved_name_by(&relaxed), NameBy::WriteTime);
        }

        #[test]
        fn a_filter_on_a_referenced_input_relaxes_object_naming() {
            register_endpoint("filtered-reference", filtered_source());
            let input = Endpoint::new(EndpointType::Ref("filtered-reference".to_string()));

            let (relaxed, _) = relax_object_naming("test", true, &input, &bucket(NameBy::Auto))
                .unwrap()
                .expect("naming relaxed");

            assert_eq!(resolved_name_by(&relaxed), NameBy::WriteTime);
        }

        #[test]
        fn an_expression_transform_relaxes_object_naming() {
            let input = with_middleware(
                Endpoint::new_memory("orders", 1),
                Middleware::Transform(TransformMiddleware {
                    expression: Some("{ id: id }".to_string()),
                    on_error: TransformErrorPolicy::Reject,
                    ..Default::default()
                }),
            );

            let (relaxed, _) = relax_object_naming("test", true, &input, &bucket(NameBy::Auto))
                .unwrap()
                .expect("naming relaxed");
            assert_eq!(resolved_name_by(&relaxed), NameBy::WriteTime);
        }

        /// `auto` is the only setting this may touch: an explicit `source_position` is a
        /// request for replay-safe names, and the fragmentation is its price.
        #[test]
        fn an_explicitly_named_sink_keeps_source_position_under_a_filter() {
            assert!(relax_object_naming(
                "test",
                true,
                &filtered_source(),
                &bucket(NameBy::SourcePosition)
            )
            .unwrap()
            .is_none());
        }

        /// `idempotency: true` is the deprecated spelling of the same explicit request.
        #[test]
        fn the_deprecated_idempotency_alias_counts_as_explicit() {
            let sink = Endpoint::new(EndpointType::ObjectStore(ObjectStoreConfig {
                url: "s3://bucket/orders".to_string(),
                idempotency: Some(true),
                ..Default::default()
            }));

            assert!(relax_object_naming("test", true, &filtered_source(), &sink)
                .unwrap()
                .is_none());
        }

        /// The filter is the newest dropper, but three older ones fragment identically.
        #[test]
        fn deduplication_weak_join_and_a_rejecting_transform_relax_the_naming_too() {
            let weak_join = Middleware::WeakJoin(WeakJoinMiddleware {
                group_by: "correlation_id".to_string(),
                expected_count: 4,
                timeout_ms: 1000,
                branch_by: None,
                required: Vec::new(),
                on_timeout: Default::default(),
            });

            for middleware in [deduplication(), weak_join, rejecting_transform()] {
                let input = with_middleware(Endpoint::new_memory("orders", 1), middleware);
                let (relaxed, _) = relax_object_naming("test", true, &input, &bucket(NameBy::Auto))
                    .unwrap()
                    .expect("naming relaxed");

                assert_eq!(resolved_name_by(&relaxed), NameBy::WriteTime);
            }
        }

        /// A rejecting transform on the *sink* drops from the batch the object-store producer
        /// beneath it writes, so it counts wherever it sits.
        #[test]
        fn a_dropping_middleware_on_the_sink_counts_too() {
            let sink = with_middleware(bucket(NameBy::Auto), rejecting_transform());

            let (relaxed, _) =
                relax_object_naming("test", true, &Endpoint::new_memory("orders", 1), &sink)
                    .unwrap()
                    .expect("naming relaxed");

            assert_eq!(resolved_name_by(&relaxed), NameBy::WriteTime);
        }

        /// A transform that rewrites nothing never rejects anything, and one set to pass
        /// through keeps the rejected message in the batch. Neither leaves a hole.
        #[test]
        fn a_transform_that_cannot_drop_a_message_leaves_the_naming_alone() {
            for transform in [
                TransformMiddleware {
                    on_error: TransformErrorPolicy::Reject,
                    ..Default::default()
                },
                TransformMiddleware {
                    schema: Some(serde_json::json!({"type": "object"})),
                    on_error: TransformErrorPolicy::PassThrough,
                    ..Default::default()
                },
            ] {
                let input = with_middleware(
                    Endpoint::new_memory("orders", 1),
                    Middleware::Transform(transform),
                );

                assert!(
                    relax_object_naming("test", true, &input, &bucket(NameBy::Auto))
                        .unwrap()
                        .is_none()
                );
            }
        }

        /// An unfiltered route is the fast, replay-safe path the default exists for.
        #[test]
        fn middlewares_that_keep_every_message_leave_source_position_naming_in_place() {
            let input = with_middleware(
                Endpoint::new_memory("orders", 1),
                Middleware::Limiter(crate::models::LimiterMiddleware {
                    messages_per_second: 100.0,
                }),
            );

            assert!(
                relax_object_naming("test", true, &input, &bucket(NameBy::Auto))
                    .unwrap()
                    .is_none()
            );
        }

        /// A dropper nested in the output tree makes only the sinks beneath it sparse.
        #[test]
        fn a_nested_dropper_relaxes_only_the_sinks_beneath_it() {
            let filtered_leg = with_middleware(
                bucket(NameBy::Auto),
                Middleware::Filter("amount > 100".to_string()),
            );
            let fanout = Endpoint::new(EndpointType::Fanout(vec![
                filtered_leg,
                bucket(NameBy::Auto),
            ]));

            let (relaxed, _) =
                relax_object_naming("test", true, &Endpoint::new_memory("orders", 1), &fanout)
                    .unwrap()
                    .expect("naming relaxed");

            let EndpointType::Fanout(legs) = &relaxed.endpoint_type else {
                panic!("not a fanout");
            };
            assert_eq!(resolved_name_by(&legs[0]), NameBy::WriteTime);
            assert_eq!(resolved_name_by(&legs[1]), NameBy::Auto);
        }

        /// The file sink resolves `auto` to write_time on its own, since the two schemes are
        /// different sink structures there. Nothing to relax.
        #[test]
        fn a_file_sink_is_left_untouched() {
            assert!(relax_object_naming(
                "test",
                true,
                &filtered_source(),
                &file_named_by(NameBy::Auto)
            )
            .unwrap()
            .is_none());
        }

        /// A `when` switch with no `default` drops whatever matches nothing, so its own cases
        /// receive a sparse stream even when the route carries no filter middleware.
        #[cfg(feature = "filter")]
        #[test]
        fn a_when_switch_without_a_default_relaxes_its_cases() {
            use crate::models::{SwitchCase, SwitchConfig};

            fn switch_to(bucket: Endpoint, default: Option<Endpoint>) -> Endpoint {
                Endpoint::new(EndpointType::Switch(SwitchConfig {
                    metadata_key: String::new(),
                    cases: std::collections::HashMap::new(),
                    when: vec![SwitchCase {
                        condition: "amount > 100".to_string(),
                        to: bucket,
                    }],
                    default: default.map(Box::new),
                }))
            }

            let ordinary = Endpoint::new_memory("ordinary", 1);
            assert!(
                relax_object_naming("test", false, &filtered_source(), &bucket(NameBy::Auto))
                    .unwrap()
                    .is_none()
            );

            let (relaxed, _) = relax_object_naming(
                "test",
                true,
                &ordinary,
                &switch_to(bucket(NameBy::Auto), None),
            )
            .unwrap()
            .expect("naming relaxed");
            let EndpointType::Switch(config) = &relaxed.endpoint_type else {
                panic!("not a switch");
            };
            assert_eq!(resolved_name_by(&config.when[0].to), NameBy::WriteTime);

            let mut cases = std::collections::HashMap::new();
            cases.insert("paid".to_string(), bucket(NameBy::Auto));
            let metadata_switch = Endpoint::new(EndpointType::Switch(SwitchConfig {
                metadata_key: "status".to_string(),
                cases,
                when: Vec::new(),
                default: None,
            }));
            let (relaxed, _) = relax_object_naming("test", true, &ordinary, &metadata_switch)
                .unwrap()
                .expect("metadata switch naming relaxed");
            let EndpointType::Switch(config) = &relaxed.endpoint_type else {
                panic!("not a switch");
            };
            assert_eq!(resolved_name_by(&config.cases["paid"]), NameBy::WriteTime);

            // With a default every message still lands somewhere, so the switch itself drops
            // nothing and the naming is left to the route's own middlewares.
            assert!(relax_object_naming(
                "test",
                true,
                &ordinary,
                &switch_to(bucket(NameBy::Auto), Some(ordinary.clone()))
            )
            .unwrap()
            .is_none());
        }
    }

    /// `check_consumer` and `check_publisher` are the gate every route config passes before
    /// anything connects. These cover the shared walk — refs, recursion, structural nesting
    /// and policy — rather than the per-transport arms, which sit behind feature flags.
    mod validation {
        use super::*;
        use crate::models::{RequestForwardConfig, SwitchConfig};
        use crate::route::register_endpoint;
        use std::collections::HashMap;
        use std::sync::Arc;

        /// The ref registry is process-global and shared with every other test in the binary.
        fn unique(prefix: &str) -> String {
            format!("{prefix}_{}", fast_uuid_v7::gen_id_str())
        }

        struct NoopHandler;

        #[async_trait::async_trait]
        impl crate::traits::Handler for NoopHandler {
            async fn handle(
                &self,
                _msg: CanonicalMessage,
            ) -> Result<crate::outcomes::Handled, crate::HandlerError> {
                Ok(crate::outcomes::Handled::Ack)
            }
        }

        fn with_handler(mut endpoint: Endpoint) -> Endpoint {
            endpoint.handler = Some(Arc::new(NoopHandler));
            endpoint
        }

        fn null() -> Endpoint {
            Endpoint::new(EndpointType::Null)
        }

        fn dangling_ref() -> Endpoint {
            Endpoint::new(EndpointType::Ref(unique("never_registered")))
        }

        fn switch_over(cases: Vec<(&str, Endpoint)>, default: Option<Endpoint>) -> Endpoint {
            Endpoint::new(EndpointType::Switch(SwitchConfig {
                metadata_key: "kind".to_string(),
                cases: cases
                    .into_iter()
                    .map(|(key, endpoint)| (key.to_string(), endpoint))
                    .collect::<HashMap<_, _>>(),
                when: Vec::new(),
                default: default.map(Box::new),
            }))
        }

        #[test]
        fn a_handler_on_an_input_endpoint_warns_that_it_is_ignored() {
            let warnings =
                check_consumer("test", &with_handler(Endpoint::new_memory("in", 1)), None).unwrap();
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("only supported on output endpoints"));
        }

        #[test]
        fn a_ref_carries_up_the_warnings_of_what_it_points_at() {
            let name = unique("ref_target");
            register_endpoint(&name, with_handler(Endpoint::new_memory("in", 1)));

            let warnings =
                check_consumer("test", &Endpoint::new(EndpointType::Ref(name)), None).unwrap();
            assert_eq!(
                warnings.len(),
                1,
                "the referenced endpoint's warning must survive the hop"
            );
        }

        #[test]
        fn an_unregistered_ref_is_rejected_on_both_sides() {
            let missing = dangling_ref();
            for err in [
                check_consumer("test", &missing, None).unwrap_err(),
                check_publisher("test", &missing, None).unwrap_err(),
            ] {
                let err = err.to_string();
                assert!(err.contains("not found"), "{err}");
            }
        }

        /// A `ref` cycle is one typo away in any config file, so the depth guard is what stands
        /// between that typo and a blown stack.
        #[test]
        fn a_ref_cycle_stops_at_the_depth_limit_instead_of_overflowing() {
            let first = unique("cycle_first");
            let second = unique("cycle_second");
            register_endpoint(&first, Endpoint::new(EndpointType::Ref(second.clone())));
            register_endpoint(&second, Endpoint::new(EndpointType::Ref(first.clone())));

            let entry = Endpoint::new(EndpointType::Ref(first));
            for err in [
                check_consumer("test", &entry, None).unwrap_err(),
                check_publisher("test", &entry, None).unwrap_err(),
            ] {
                let err = err.to_string();
                assert!(err.contains("depth"), "{err}");
            }
        }

        /// A structural endpoint is a tree, so validation has to reach the leaves. Each of these
        /// buries the same unresolvable ref one level down a different arm.
        #[test]
        fn validation_reaches_leaves_through_fanout_switch_and_request() {
            let fanout = Endpoint::new(EndpointType::Fanout(vec![null(), dangling_ref()]));
            let switch_case = switch_over(vec![("paid", dangling_ref())], None);
            let switch_default = switch_over(vec![("paid", null())], Some(dangling_ref()));
            let request = Endpoint::new(EndpointType::Request(RequestForwardConfig {
                to: Box::new(null()),
                forward_to: Box::new(dangling_ref()),
            }));

            for endpoint in [fanout, switch_case, switch_default, request] {
                assert!(
                    check_publisher("test", &endpoint, None).is_err(),
                    "a broken leaf must not pass validation"
                );
            }
        }

        /// `reader` is the one publisher arm that flips role: what it wraps is validated as an
        /// input, so an input-only rejection has to still apply inside it.
        #[test]
        fn a_reader_validates_its_inner_endpoint_as_a_consumer() {
            let reader = Endpoint::new(EndpointType::Reader(Box::new(switch_over(
                vec![("paid", null())],
                None,
            ))));
            let err = check_publisher("test", &reader, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("only supported as an output"), "{err}");
        }

        #[test]
        fn an_input_only_rejection_names_the_route_that_carries_it() {
            let err = check_consumer(
                "orders_in",
                &switch_over(vec![("paid", null())], None),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("orders_in"), "{err}");
        }

        /// The policy list governs transports. Core types stay reachable whatever it says, so a
        /// policy of `["memory"]` must not lock a route out of `null`, `fanout` or `file`.
        #[test]
        fn the_policy_list_never_blocks_a_core_endpoint_type() {
            let allowed: &[&str] = &["memory"];
            for endpoint in [
                null(),
                Endpoint::new_memory("plain", 1),
                Endpoint::new(EndpointType::Fanout(vec![null()])),
            ] {
                assert!(check_publisher("test", &endpoint, Some(allowed)).is_ok());
            }

            let file = Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
                "/tmp/policy_probe.jsonl",
            )));
            assert!(check_consumer("test", &file, Some(allowed)).is_ok());
        }

        /// `null` and `fanout` are sinks. They are refused as inputs on role grounds, not policy,
        /// so the absence of any policy at all must not let them through.
        #[test]
        fn output_only_types_are_refused_as_inputs_whatever_the_policy() {
            for endpoint in [null(), Endpoint::new(EndpointType::Fanout(vec![null()]))] {
                assert!(check_consumer("test", &endpoint, None).is_err());
                assert!(check_publisher("test", &endpoint, None).is_ok());
            }
        }

        /// Policy is enforced per node, not just at the root — otherwise a `fanout` would be a
        /// hole straight through it.
        #[cfg(feature = "http")]
        #[test]
        fn the_policy_list_rejects_a_disallowed_type_however_deeply_it_is_nested() {
            let http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig::new(
                "http://localhost:8080",
            )));
            let allowed: &[&str] = &["memory"];

            let err = check_publisher("test", &http, Some(allowed))
                .unwrap_err()
                .to_string();
            assert!(err.contains("not allowed by policy"), "{err}");

            let nested = Endpoint::new(EndpointType::Fanout(vec![null(), http.clone()]));
            assert!(check_publisher("test", &nested, Some(allowed)).is_err());

            assert!(check_publisher("test", &http, Some(&["memory", "http"])).is_ok());
        }
    }
}
