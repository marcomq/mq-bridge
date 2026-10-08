//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Every built-in mq-bridge endpoint as one loadable plugin library.
//!
//! A host that embeds mq-bridge without the endpoint features loads this library by
//! path and keeps writing `kafka:`, `file:`, … in its routes: an endpoint the host has
//! no feature for is served by the factory registered here under the same name.
//!
//! The library carries its own engine. `memory` channels, `ref` names and factories
//! registered in the host are therefore not visible to the endpoints created here.

use anyhow::Context;
use async_trait::async_trait;
use mq_bridge::endpoints::{create_consumer_from_route, create_publisher_from_route};
use mq_bridge::models::Endpoint;
use mq_bridge::traits::{CustomEndpointFactory, MessageConsumer, MessagePublisher};
use serde_json::Value;

/// Builds the endpoint the host would have built from `<name>: <config>`.
fn endpoint(name: &str, config: &Value) -> anyhow::Result<Endpoint> {
    install_crypto_provider();
    let mut tagged = serde_json::Map::new();
    tagged.insert(name.to_string(), config.clone());
    serde_json::from_value(Value::Object(tagged))
        .with_context(|| format!("invalid `{name}` endpoint config"))
}

fn install_crypto_provider() {
    #[cfg(feature = "rustls-aws-lc")]
    {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// One factory type per endpoint name, all delegating to this library's engine.
macro_rules! built_in_endpoints {
    ($($factory:ident => $name:literal),+ $(,)?) => {
        $(
            #[derive(Debug, Default)]
            pub struct $factory;

            #[async_trait]
            impl CustomEndpointFactory for $factory {
                async fn create_consumer(
                    &self,
                    route_name: &str,
                    config: &Value,
                ) -> anyhow::Result<Box<dyn MessageConsumer>> {
                    create_consumer_from_route(route_name, &endpoint($name, config)?).await
                }

                async fn create_publisher(
                    &self,
                    route_name: &str,
                    config: &Value,
                ) -> anyhow::Result<Box<dyn MessagePublisher>> {
                    let publisher =
                        create_publisher_from_route(route_name, &endpoint($name, config)?).await?;
                    Ok(Box::new(publisher))
                }
            }
        )+

        mq_bridge::export_endpoint_plugins! {
            $({ name: $name, factory: $factory }),+
        }
    };
}

// Names are `EndpointType::name()`. An endpoint this library was built without fails at
// creation with the engine's own "unsupported endpoint type" error.
built_in_endpoints! {
    File => "file",
    DirSpool => "dir_spool",
    ObjectStore => "object_store",
    Sled => "sled",
    Kafka => "kafka",
    Nats => "nats",
    Amqp => "amqp",
    Mqtt => "mqtt",
    MongoDb => "mongodb",
    Http => "http",
    WebSocket => "websocket",
    Grpc => "grpc",
    Aws => "aws",
    IbmMq => "ibmmq",
    ZeroMq => "zeromq",
    RedisStreams => "redis_streams",
    Sqlx => "sqlx",
    ClickHouse => "clickhouse",
    HttpBulk => "http_bulk",
    PostgresCdc => "postgres_cdc",
}
