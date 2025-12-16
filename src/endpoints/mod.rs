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
use crate::middleware::apply_middlewares_to_consumer;
use crate::models::{Endpoint, EndpointType};
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::{anyhow, Result};
use std::sync::Arc;

/// Creates a `MessageConsumer` based on the route's "in" configuration.
pub async fn create_consumer_from_route(
    route_name: &str,
    endpoint: &Endpoint,
) -> Result<Box<dyn MessageConsumer>> {
    let consumer = create_base_consumer(route_name, &endpoint.endpoint_type).await?;
    apply_middlewares_to_consumer(consumer, endpoint, route_name).await
}

async fn create_base_consumer(
    route_name: &str,
    endpoint_type: &EndpointType,
) -> Result<Box<dyn MessageConsumer>> {
    match endpoint_type {
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(Box::new(kafka::KafkaConsumer::new(&cfg.config, topic)?))
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let subject = cfg.subject.as_deref().unwrap_or(route_name);
            let stream_name = cfg.stream.as_deref().ok_or_else(|| {
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
        EndpointType::Memory(cfg) => Ok(Box::new(memory::MemoryConsumer::new(cfg)?)),
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
    endpoint: &Endpoint,
) -> Result<Arc<dyn MessagePublisher>> {
    // This function was partially refactored. It should create a base publisher
    // and then apply middlewares, similar to `create_consumer_from_route`.
    // However, since the middleware application logic already exists in `middleware/mod.rs`,
    // we can simply call it after creating the base publisher.
    // For now, let's fix the immediate compilation errors by creating the base publisher
    // and wrapping it correctly. The middleware logic can be added back cleanly.
    let publisher = create_base_publisher(route_name, &endpoint.endpoint_type).await?;
    crate::middleware::apply_middlewares_to_publisher(publisher, endpoint, route_name).await
}

async fn create_base_publisher(
    route_name: &str,
    endpoint_type: &EndpointType,
) -> Result<Box<dyn MessagePublisher>> {
    let publisher = match endpoint_type {
        #[cfg(feature = "kafka")]
        EndpointType::Kafka(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(
                Box::new(kafka::KafkaPublisher::new(&cfg.config, topic).await?)
                    as Box<dyn MessagePublisher>,
            )
        }
        #[cfg(feature = "nats")]
        EndpointType::Nats(cfg) => {
            let subject = cfg.subject.as_deref().unwrap_or(route_name);
            Ok(Box::new(
                nats::NatsPublisher::new(&cfg.config, subject, cfg.stream.as_deref()).await?,
            ) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "amqp")]
        EndpointType::Amqp(cfg) => {
            let queue = cfg.queue.as_deref().unwrap_or(route_name);
            Ok(
                Box::new(amqp::AmqpPublisher::new(&cfg.config, queue).await?)
                    as Box<dyn MessagePublisher>,
            )
        }
        #[cfg(feature = "mqtt")]
        EndpointType::Mqtt(cfg) => {
            let topic = cfg.topic.as_deref().unwrap_or(route_name);
            Ok(
                Box::new(mqtt::MqttPublisher::new(&cfg.config, topic, route_name).await?)
                    as Box<dyn MessagePublisher>,
            )
        }
        EndpointType::File(cfg) => {
            Ok(Box::new(file::FilePublisher::new(cfg).await?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "http")]
        EndpointType::Http(cfg) => {
            let mut sink = http::HttpPublisher::new(&cfg.config).await?;
            if let Some(url) = &cfg.config.url {
                sink = sink.with_url(url);
            }
            Ok(Box::new(sink) as Box<dyn MessagePublisher>)
        }
        EndpointType::Static(cfg) => Ok(Box::new(static_endpoint::StaticEndpointPublisher::new(
            cfg,
        )?) as Box<dyn MessagePublisher>),
        EndpointType::Memory(cfg) => {
            Ok(Box::new(memory::MemoryPublisher::new(cfg)?) as Box<dyn MessagePublisher>)
        }
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            let collection = cfg.collection.as_deref().unwrap_or(route_name);
            Ok(
                Box::new(mongodb::MongoDbPublisher::new(&cfg.config, collection).await?)
                    as Box<dyn MessagePublisher>,
            )
        }
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported publisher endpoint type",
            route_name
        )),
    }?;
    Ok(publisher)
}
