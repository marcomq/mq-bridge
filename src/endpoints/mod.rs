//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

#[cfg(feature = "amqp")]
pub mod amqp;
#[cfg(feature = "aws")]
pub mod aws;
pub mod fanout;
pub mod file;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "ibm-mq")]
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
pub mod null;
pub mod reader;
pub mod response;
#[cfg(feature = "sled")]
pub mod sled;
#[cfg(feature = "sqlx")]
pub mod sqlx;
pub mod static_endpoint;
pub mod streaming_handler;
pub mod switch;
#[cfg(feature = "zeromq")]
pub mod zeromq;
use crate::endpoints::memory::{get_or_create_channel, MemoryChannel};
use crate::middleware::apply_middlewares_to_consumer;
use crate::models::{Endpoint, EndpointType, MemoryConfig, Middleware, ResponseConfig};
use crate::route::get_endpoint_factory;
use crate::traits::{BoxFuture, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
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
        #[cfg(feature = "zeromq")]
        EndpointType::ZeroMq(_) => Ok(warnings),
        #[cfg(feature = "ibm-mq")]
        EndpointType::IbmMq(_) => Ok(warnings),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
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
        EndpointType::File(_) => Ok(warnings),
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

/// Creates a `MessageConsumer` based on the route's "in" configuration.
pub async fn create_consumer_from_route(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<Box<dyn MessageConsumer>> {
    let resolved_endpoint = resolve_endpoint(endpoint, route_name)?;
    check_consumer(route_name, &resolved_endpoint, None)?;
    let consumer = create_base_consumer(route_name, &resolved_endpoint).await?;
    apply_middlewares_to_consumer(consumer, &resolved_endpoint, route_name).await
}

async fn create_base_consumer(
    route_name: &str,
    endpoint: &Endpoint,
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
            Ok(boxed(kafka::KafkaConsumer::new(&config).await?))
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let mut config = cfg.clone();
            if config.subject.is_none() {
                config.subject = Some(route_name.to_string());
            }
            Ok(boxed(nats::NatsConsumer::new(&config).await?))
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let mut config = cfg.clone();
            if config.queue.is_none() {
                config.queue = Some(route_name.to_string());
            }
            Ok(boxed(amqp::AmqpConsumer::new(&config).await?))
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
            Ok(boxed(mqtt::MqttConsumer::new(cfg).await?))
        }
        #[cfg(feature = "ibm-mq")]
        EndpointType::IbmMq(cfg) => {
            let mut config = cfg.clone();
            if config.queue.is_none() && config.topic.is_none() {
                config.queue = Some(route_name.to_string());
            }
            Ok(boxed(ibm_mq::IbmMqConsumer::new(&config).await?))
        }
        #[cfg(feature = "zeromq")]
        EndpointType::ZeroMq(cfg) => Ok(boxed(zeromq::ZeroMqConsumer::new(cfg).await?)),
        EndpointType::File(cfg) => Ok(boxed(file::FileConsumer::new(cfg).await?)),
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(cfg) => Ok(boxed(grpc::GrpcConsumer::new(cfg).await?)),
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => Ok(boxed(sqlx::SqlxConsumer::new(cfg).await?)),
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => Ok(boxed(http::HttpConsumer::new(cfg).await?)),
        EndpointType::Static(cfg) => Ok(boxed(static_endpoint::StaticRequestConsumer::new(cfg)?)),
        EndpointType::Memory(cfg) => Ok(boxed(memory::MemoryConsumer::new(cfg)?)),
        #[cfg(feature = "sled")]
        EndpointType::Sled(cfg) => Ok(boxed(sled::SledConsumer::new(cfg)?)),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            let mut config = cfg.clone();
            if config.collection.is_none() {
                config.collection = Some(route_name.to_string());
            }
            if config.change_stream {
                if config.ttl_seconds.is_none() {
                    config.ttl_seconds = Some(86400); // Remove events by default after 24 hours
                }
                Ok(boxed(mongodb::MongoDbSubscriber::new(&config).await?))
            } else {
                Ok(boxed(mongodb::MongoDbConsumer::new(&config).await?))
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
        #[cfg(feature = "zeromq")]
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
        #[cfg(feature = "ibm-mq")]
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
            for endpoint in cfg.cases.values() {
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
pub async fn create_publisher_from_route(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<Arc<dyn MessagePublisher>> {
    check_publisher(route_name, endpoint, None)?;
    create_publisher_with_depth(route_name.to_string(), endpoint.clone(), 0).await
}

fn create_publisher_with_depth(
    route_name: String,
    endpoint: Endpoint,
    depth: usize,
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
                    let mut publisher: Box<dyn MessagePublisher> = Box::new(inner);

                    if let Some(handler) = &endpoint.handler {
                        publisher = Box::new(crate::command_handler::CommandPublisher::new(
                            publisher,
                            handler.clone(),
                        ));
                    }
                    return crate::middleware::apply_middlewares_to_publisher(
                        publisher,
                        &endpoint,
                        &route_name,
                    )
                    .await;
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

            return create_publisher_with_depth(route_name, merged, depth + 1).await;
        }

        let mut publisher =
            create_base_publisher(&route_name, &endpoint.endpoint_type, depth).await?;
        if let Some(handler) = &endpoint.handler {
            publisher = Box::new(crate::command_handler::CommandPublisher::new(
                publisher,
                handler.clone(),
            ));
        }
        crate::middleware::apply_middlewares_to_publisher(publisher, &endpoint, &route_name).await
    })
}

async fn create_base_publisher(
    route_name: &str,
    endpoint_type: &EndpointType,
    depth: usize,
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
        #[cfg(feature = "zeromq")]
        EndpointType::ZeroMq(cfg) => {
            Ok(Box::new(zeromq::ZeroMqPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "grpc")]
        EndpointType::Grpc(cfg) => {
            Ok(Box::new(grpc::GrpcPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) => {
            Ok(Box::new(sqlx::SqlxPublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => {
            let sink = http::HttpPublisher::new(cfg).await?;
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
        EndpointType::File(cfg) => {
            Ok(Box::new(file::FilePublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        EndpointType::Static(cfg) => Ok(Box::new(static_endpoint::StaticEndpointPublisher::new(
            cfg,
        )?) as Box<dyn MessagePublisher>),
        EndpointType::Memory(cfg) => {
            Ok(Box::new(memory::MemoryPublisher::new(cfg)?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "sled")]
        EndpointType::Sled(cfg) => {
            Ok(Box::new(sled::SledPublisher::new(cfg)?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "ibm-mq")]
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
                )
                .await?;
                publishers.push(p);
            }
            Ok(Box::new(fanout::FanoutPublisher::new(publishers)) as Box<dyn MessagePublisher>)
        }
        EndpointType::StreamingHandler(cfg) => {
            Ok(create_streaming_handler_publisher(route_name, endpoint_type, depth, cfg).await?)
        }
        EndpointType::Switch(cfg) => {
            let mut cases = std::collections::HashMap::new();
            for (key, endpoint) in &cfg.cases {
                let p = create_publisher_with_depth(
                    route_name.to_string(),
                    endpoint.clone(),
                    depth + 1,
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
                    )
                    .await?,
                )
            } else {
                None
            };
            Ok(Box::new(switch::SwitchPublisher::new(
                cfg.metadata_key.clone(),
                cases,
                default,
            )) as Box<dyn MessagePublisher>)
        }
        EndpointType::Response(_) => {
            Ok(Box::new(response::ResponsePublisher) as Box<dyn MessagePublisher>)
        }
        EndpointType::Reader(inner) => {
            let consumer = create_consumer_from_route(route_name, inner).await?;
            Ok(Box::new(reader::ReaderPublisher::new(consumer)) as Box<dyn MessagePublisher>)
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

async fn create_streaming_handler_publisher(
    route_name: &str,
    _endpoint_type: &EndpointType,
    depth: usize,
    config: &crate::models::StreamingHandlerConfig,
) -> Result<Box<dyn MessagePublisher>> {
    let inner_publisher =
        create_publisher_with_depth(route_name.to_string(), config.output.clone(), depth + 1)
            .await?;

    // This is where you'd resolve the actual StreamingHandler implementation.
    // For now, we'll use a placeholder or assume a default.
    // In a real scenario, you might have a factory for StreamingHandlers.
    let handler: Arc<dyn crate::traits::StreamingHandler> = Arc::new(
        crate::type_handler::TypeHandler::new() // Reuse TypeHandler for dispatching to streaming handlers.
            .add_streaming_handler(
                "default",
                |msg: CanonicalMessage,
                 _ctx: crate::MessageContext,
                 yielder: crate::traits::Yielder| async move {
                    // Default behavior: just pass the message through
                    yielder.send(msg).await?;
                    Ok(crate::outcomes::Handled::Ack)
                },
            ),
    );

    Ok(Box::new(streaming_handler::StreamingHandlerPublisher::new(
        // No longer generic
        handler,
        inner_publisher,
    )) as Box<dyn MessagePublisher>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, EndpointType};
    use crate::CanonicalMessage;

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

    #[test]
    fn test_consumer_middleware_ordering() {
        let endpoint = Endpoint::new_memory("test", 10)
            .with_deduplication(crate::models::DeduplicationMiddleware {
                sled_path: "".into(),
                ttl_seconds: 10,
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
}
