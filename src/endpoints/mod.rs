//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

#[cfg(feature = "amqp")]
pub mod amqp;
pub mod file;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod memory;
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "nats")]
pub mod nats;
pub mod static_endpoint;

use crate::models::EndpointType;
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::{anyhow, Result};
use std::sync::Arc;

/// Creates a `MessageConsumer` based on the route's "in" configuration.
pub async fn create_consumer_from_route(
    route_name: &str,
    endpoint: &EndpointType,
) -> Result<Box<dyn MessageConsumer>> {
    match endpoint {
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(Box::new(kafka::KafkaConsumer::new(&cfg.config, topic)?))
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let subject = cfg.subject.as_deref().unwrap_or(route_name);
            let stream_name = cfg
                .stream
                .as_deref()
                .or(cfg.stream.as_deref())
                .ok_or_else(|| {
                    anyhow!(
                        "[route:{}] NATS consumer must specify a 'stream' or have a 'default_stream'",
                        route_name
                    )
                })?;
            Ok(Box::new(
                nats::NatsConsumer::new(&cfg.config, stream_name, subject).await?,
            ))
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let queue = cfg.queue.as_deref().unwrap_or(route_name);
            Ok(Box::new(amqp::AmqpConsumer::new(&cfg.config, queue).await?))
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(Box::new(
                mqtt::MqttConsumer::new(&cfg.config, topic, route_name).await?,
            ))
        }
        EndpointType::File(path) => Ok(Box::new(file::FileConsumer::new(path).await?)),
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => Ok(Box::new(http::HttpConsumer::new(&cfg.config).await?)),
        EndpointType::Static(cfg) => {
            Ok(Box::new(static_endpoint::StaticRequestConsumer::new(cfg)?))
        }
        EndpointType::Memory(cfg) => Ok(Box::new(memory::MemoryConsumer::new(&cfg)?)),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            let collection = cfg.collection.as_deref().unwrap_or(route_name);
            Ok(Box::new(
                mongodb::MongoDbConsumer::new(&cfg.config, collection).await?,
            ))
        }
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported consumer endpoint type",
            route_name
        )),
    }
}

/// Creates a `MessagePublisher` based on the route's "out" configuration.
pub async fn create_publisher_from_route(
    route_name: &str,
    endpoint: &EndpointType,
) -> Result<Arc<dyn MessagePublisher>> {
    match endpoint {
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(Arc::new(
                kafka::KafkaPublisher::new(&cfg.config, topic).await?,
            ))
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let subject = cfg.subject.as_deref().unwrap_or(route_name);
            Ok(Arc::new(
                nats::NatsPublisher::new(&cfg.config, subject, cfg.stream.as_deref()).await?,
            ))
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let queue = cfg.queue.as_deref().unwrap_or(route_name);
            Ok(Arc::new(
                amqp::AmqpPublisher::new(&&cfg.config, queue).await?,
            ))
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(Arc::new(
                mqtt::MqttPublisher::new(&cfg.config, topic, route_name).await?,
            ))
        }
        EndpointType::File(cfg) => Ok(Arc::new(file::FilePublisher::new(cfg).await?)),
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => {
            let mut sink = http::HttpPublisher::new(&cfg.config).await?;
            if let Some(url) = &cfg.config.url {
                sink = sink.with_url(url);
            }
            Ok(Arc::new(sink))
        }
        EndpointType::Static(cfg) => Ok(Arc::new(static_endpoint::StaticEndpointPublisher::new(
            cfg,
        )?)),
        EndpointType::Memory(cfg) => Ok(Arc::new(memory::MemoryPublisher::new(&cfg)?)),
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            let collection = cfg.collection.as_deref().unwrap_or(route_name);
            Ok(Arc::new(
                mongodb::MongoDbPublisher::new(&cfg.config, collection).await?,
            ))
        }
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported publisher endpoint type",
            route_name
        )),
    }
}

/// Creates a `MessagePublisher` for the DLQ if configured.
#[allow(unused_variables)]
pub async fn create_dlq_from_route(
    route_name: &str,
    endpoint: &EndpointType,
) -> Result<Arc<dyn MessagePublisher>> {
    tracing::info!("DLQ configured for route {}", route_name);
    let publisher = create_publisher_from_route(route_name, endpoint).await?;
    return Ok(publisher);
}

/*

impl ConsumerEndpoint {
    pub fn channel(&self) -> Result<memory::MemoryChannel> {
        match self {
            ConsumerEndpoint::Memory(cfg) => Ok(memory::get_or_create_channel(&cfg.config)),
            _ => Err(anyhow!("channel() called on non-memory ConsumerEndpoint")),
        }
    }
}

impl PublisherEndpoint {
    pub fn channel(&self) -> Result<memory::MemoryChannel> {
        match self {
            PublisherEndpoint::Memory(cfg) => Ok(memory::get_or_create_channel(&cfg.config)),
            _ => Err(anyhow!("channel() called on non-memory ConsumerEndpoint")),
        }
    }
}*/
