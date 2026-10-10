//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//! Move messages between brokers, databases, files, HTTP services, and in-memory
//! channels without coupling application code to a specific transport.
//!
//! `mq-bridge` is an asynchronous, embeddable integration library. It gives each
//! transport the same message, consumer, and publisher interfaces, then composes
//! them into routes. A route can transform, filter, batch, retry, rate-limit,
//! deduplicate, or fan out messages before they reach their destination.
//!
//! Unlike a standalone message broker or ETL service, `mq-bridge` runs inside
//! your Rust application. If you prefer a zero-code service configured with YAML,
//! see [`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app).
//!
//! # Quick start
//!
//! Every endpoint implements the same [`traits::MessagePublisher`] interface.
//! This example uses an in-memory endpoint, so it runs without external services:
//!
//! ```
//! use mq_bridge::{
//!     CanonicalMessage,
//!     endpoints::memory::MemoryPublisher,
//!     traits::MessagePublisher,
//! };
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let publisher = MemoryPublisher::new_local("docs-quick-start", 16);
//! let channel = publisher.channel();
//!
//! publisher
//!     .send(CanonicalMessage::from("hello from mq-bridge"))
//!     .await?;
//!
//! let messages = channel.drain_messages();
//! assert_eq!(messages[0].get_payload_str(), "hello from mq-bridge");
//! # Ok(())
//! # }
//! ```
//!
//! Replace the memory endpoint with Kafka, NATS, AMQP, MQTT, MongoDB, SQL,
//! HTTP, WebSocket, or another supported endpoint without changing the message
//! model. Transport integrations are enabled with [Cargo features](#cargo-features).
//!
//! ## Routes from a config file
//!
//! Like the Python and Node bindings, routes and publishers load straight from a
//! config file (YAML with the `yaml` feature, JSON always):
//!
//! ```no_run
//! use mq_bridge::{CanonicalMessage, Handled, Publisher, Route};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Every route and publisher in the file, running in the background:
//! mq_bridge::deploy_file("routes.yaml").await?;
//!
//! // Or one route with a handler:
//! Route::from_file("routes.yaml", "orders")?
//!     .with_handler(|msg: CanonicalMessage| async move { Ok(Handled::Publish(msg)) })
//!     .deploy("orders")
//!     .await?;
//!
//! Publisher::from_file("routes.yaml", "audit").await?
//!     .send_json(&serde_json::json!({"order_id": 42}))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Core concepts
//!
//! - [`CanonicalMessage`] is the transport-independent payload and metadata format.
//! - [`models::Endpoint`] configures a message source or destination.
//! - [`Route`] connects an input endpoint to an output endpoint and optionally
//!   applies a handler.
//! - [`Publisher`] creates a reusable publisher from endpoint configuration.
//! - [`traits::MessageConsumer`] and [`traits::MessagePublisher`] are the extension
//!   points for custom transports.
//! - [`middleware`] contains reusable reliability and processing layers.
//!
//! Most applications work through [`Route`] and [`Publisher`]. Direct consumer
//! usage is available when an application needs to control acknowledgement,
//! batching, and concurrency itself.
//!
//! # Cargo features
//!
//! The only default features are `file` and `dir-spool`. Enable the integrations
//! your application uses:
//!
//! ```toml
//! [dependencies]
//! mq-bridge = { version = "0.4", features = ["kafka", "http"] }
//! ```
//!
//! Common feature groups include:
//!
//! - `middleware` — metrics, deduplication, compression, and encryption.
//! - `portable` — integrations that build on common operating systems without
//!   specialized native SDKs.
//! - `full` — all supported integrations; some require native build tools or
//!   runtime libraries.
//!
//! ## Feature flags
//!
//! | Feature | Enables |
//! | :--- | :--- |
//! | `kafka`, `nats`, `amqp`, `mqtt` | Kafka, NATS (JetStream), RabbitMQ / AMQP and MQTT endpoints |
//! | `redis-streams`, `aws`, `zeromq`, `ibm-mq` | Redis Streams, AWS SQS/SNS, ZeroMQ and IBM MQ endpoints |
//! | `http`, `grpc`, `websocket` | HTTP, gRPC and WebSocket endpoints, as server or client |
//! | `sqlx` | PostgreSQL, MySQL / MariaDB and SQLite as source or sink |
//! | `postgres-cdc` | Postgres change data capture (logical replication, `pgoutput`) |
//! | `mongodb` | MongoDB source and sink, including change streams (CDC) |
//! | `clickhouse` | ClickHouse bulk insert and cursor reads |
//! | `http-bulk` | `http_bulk` endpoint: JSON documents in bulk to and from search engines and similar HTTP APIs |
//! | `object-store` | S3, GCS, Azure Blob and local-directory object storage |
//! | `parquet` | `format: parquet` on the object-store endpoint |
//! | `compression`, `encryption` | gzip / lz4 / zstd and AEAD encryption for files, objects and payloads |
//! | `dedup`, `filter`, `aggregate` | The `deduplication`, `filter` and `aggregate` middlewares |
//! | `metrics`, `otel` | Metrics and OpenTelemetry span middlewares |
//! | `avro` | Confluent-framed Avro payloads with a schema registry |
//! | `yaml` | YAML config files; JSON works without it |
//! | `plugin`, `plugin-sdk` | Load native endpoint plugins; author one |
//! | `schema` | JSON Schema for the config models |
//! | `rustls-ring`, `rustls-aws-lc` | The TLS crypto provider; pick one when using TLS |
//!
//! The in-memory endpoint ([`endpoints::memory`]) and the `retry`, `dlq` and
//! `transform` middlewares need no feature; the file endpoint needs `file`,
//! which is on by default.
//!
//! # Capabilities
//!
//! The README's [capabilities at a glance](https://github.com/marcomq/mq-bridge#capabilities-at-a-glance)
//! table maps each capability (brokers, CDC, SQL, Parquet on object storage,
//! warehouses via Parquet, schema validation, broker-free tests) to its endpoint,
//! feature and book page. The [book](https://marcomq.github.io/mq-bridge/)
//! documents this library as well as the zero-code app.
//!
//! See the [endpoint capability table](https://marcomq.github.io/mq-bridge/reference/endpoints.html#consumer-vs-subscriber-and-nack-support)
//! for individual transports, platform requirements, and configuration examples.
//!
//! # Where to go next
//!
//! - Start with [`Route`], [`Publisher`], and [`CanonicalMessage`] for the primary API.
//! - Browse [`endpoints`] for transport implementations and [`models`] for their
//!   configuration types.
//! - Read the [architecture guide](https://github.com/marcomq/mq-bridge/blob/dev/docs/ARCHITECTURE.md)
//!   for routing, handlers, batching, and delivery semantics.
//! - Read the [documentation book](https://marcomq.github.io/mq-bridge/) for setup,
//!   connector configuration, and recipes.
//!
//! # Reliability model
//!
//! Publishing can distinguish success, partial success, retryable failure, and
//! permanent failure. Consumers return explicit commit callbacks, allowing routes
//! to preserve correct acknowledgement ordering for both cumulative-ack brokers
//! and transports with independent acknowledgements. See [`SentBatch`],
//! [`ReceivedBatch`], and [`traits::MessageDisposition`] for the underlying types.

#![warn(rustdoc::broken_intra_doc_links)]
#![warn(rustdoc::missing_crate_level_docs)]

pub mod canonical_message;
// Not feature-gated: the trait, the file backend and the URL parser need no
// optional dependency, and each external backend already reports its own
// missing feature at runtime.
pub mod checkpoint;
pub mod command_handler;
mod config_file;
pub mod endpoints;
pub mod errors;
pub mod event_handler;
pub mod event_store;
pub mod extensions;
pub mod middleware;
pub mod models;
pub mod outcomes;
#[cfg(feature = "plugin")]
pub mod plugin;
pub mod publisher;
pub mod response;
pub mod route;
pub mod shutdown;
pub mod support;
#[cfg(feature = "test-utils")]
pub mod test_utils;
pub mod traits;
pub mod type_handler;

pub use anyhow;
pub use canonical_message::{CanonicalMessage, MessageContext};
pub use config_file::deploy_file;
pub use errors::HandlerError;
pub use models::Route;
pub use outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};
pub use publisher::Publisher;

pub use endpoints::memory::get_or_create_channel;
pub use publisher::{get_publisher, list_publishers, register_publisher, unregister_publisher};
pub use route::{
    get_route, list_routes, register_endpoint, route_outcome, route_status, stop_route,
    RouteOutcome,
};

// Re-export the underlying driver crate for each feature-gated endpoint, so
// downstream code can depend on the exact same version mq-bridge builds against
// (and share types with it) without adding — and keeping in sync — its own
// dependency entry. Each is gated on the feature that pulls the crate in.
#[cfg(feature = "nats")]
pub use async_nats;
#[cfg(feature = "amqp")]
pub use lapin;
#[cfg(feature = "mongodb")]
pub use mongodb;
#[cfg(feature = "otel")]
pub use opentelemetry;
#[cfg(feature = "kafka")]
pub use rdkafka;
#[cfg(feature = "redis-streams")]
pub use redis;
#[cfg(feature = "clickhouse")]
pub use reqwest;
#[cfg(feature = "mqtt")]
pub use rumqttc;
#[cfg(feature = "websocket")]
pub use tokio_websockets;
#[cfg(feature = "zeromq")]
pub use zeromq;
#[cfg(feature = "aws")]
pub use {aws_config, aws_sdk_sns, aws_sdk_sqs};
#[cfg(feature = "grpc")]
pub use {prost, tonic};
// `sqlx` is also enabled transitively by `postgres-cdc`; the integration tests
// use this re-export instead of a duplicate dev-dependency.
#[cfg(any(feature = "ibm-mq", feature = "ibm-mq-static"))]
pub use mqi;
#[cfg(feature = "postgres-cdc")]
pub use pgwire_replication;
#[cfg(feature = "sqlx")]
pub use sqlx;

pub mod consumer {
    pub use crate::middleware::apply_middlewares_to_consumer as apply_middlewares;
}

/// The application name, derived from the package name in Cargo.toml.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
