//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

#[cfg(feature = "amqp")]
pub mod amqp;
pub mod fanout;
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
            Ok(Box::new(
                kafka::KafkaConsumer::new(&cfg.config, topic).await?,
            ))
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
    create_publisher_with_depth(route_name, endpoint, 0).await
}

async fn create_publisher_with_depth(
    route_name: &str,
    endpoint: &Endpoint,
    depth: usize,
) -> Result<Arc<dyn MessagePublisher>> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(anyhow!("Fanout recursion depth exceeded limit of {}", MAX_DEPTH));
    }
    let publisher = create_base_publisher(route_name, &endpoint.endpoint_type, depth).await?;
    crate::middleware::apply_middlewares_to_publisher(publisher, endpoint, route_name).await
}

async fn create_base_publisher(
    route_name: &str,
    endpoint_type: &EndpointType,
    depth: usize,
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
            let stream_name = cfg.stream.as_deref().unwrap_or_default();
            Ok(
                Box::new(nats::NatsPublisher::new(&cfg.config, stream_name, subject).await?)
                    as Box<dyn MessagePublisher>,
            )
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
        EndpointType::Fanout(endpoints) => {
            let mut publishers = Vec::with_capacity(endpoints.len());
            for endpoint in endpoints {
                let p = Box::pin(create_publisher_with_depth(route_name, endpoint, depth + 1)).await?;
                publishers.push(p);
            }
            Ok(Box::new(fanout::FanoutPublisher::new(publishers)) as Box<dyn MessagePublisher>)
        }
        #[allow(unreachable_patterns)]
        _ => Err(anyhow!(
            "[route:{}] Unsupported publisher endpoint type",
            route_name
        )),
    }?;
    Ok(publisher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::get_or_create_channel;
    use crate::models::{Endpoint, EndpointType, MemoryConfig};
    use crate::CanonicalMessage;

    #[tokio::test]
    async fn test_fanout_publisher_integration() {
        let mem_cfg1 = MemoryConfig {
            topic: "fanout_1".to_string(),
            capacity: Some(10),
        };
        let mem_cfg2 = MemoryConfig {
            topic: "fanout_2".to_string(),
            capacity: Some(10),
        };

        let ep1 = Endpoint::new(EndpointType::Memory(mem_cfg1.clone()));
        let ep2 = Endpoint::new(EndpointType::Memory(mem_cfg2.clone()));

        let fanout_ep = Endpoint::new(EndpointType::Fanout(vec![ep1, ep2]));

        let publisher = create_publisher_from_route("test_fanout", &fanout_ep)
            .await
            .expect("Failed to create fanout publisher");

        let msg = CanonicalMessage::new(b"fanout_payload".to_vec(), None);
        publisher.send(msg).await.expect("Failed to send message");

        let chan1 = get_or_create_channel(&mem_cfg1);
        let chan2 = get_or_create_channel(&mem_cfg2);

        assert_eq!(chan1.len(), 1);
        assert_eq!(chan2.len(), 1);

        let msg1 = chan1.drain_messages().pop().unwrap();
        let msg2 = chan2.drain_messages().pop().unwrap();

        assert_eq!(msg1.payload, "fanout_payload".as_bytes());
        assert_eq!(msg2.payload, "fanout_payload".as_bytes());
    }
}
