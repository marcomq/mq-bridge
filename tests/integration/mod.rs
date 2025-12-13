#[cfg(feature = "amqp")]
pub mod amqp;
pub mod common;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod memory;
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mqtt")]
pub mod mqtt;

#[cfg(all(
    feature = "nats",
    feature = "kafka",
    feature = "amqp",
    feature = "mqtt",
    feature = "http"
))]
pub mod all_endpoints;
#[cfg(feature = "nats")]
pub mod nats;
