//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
pub mod canonical_message;
pub mod command_handler;
pub mod endpoints;
pub mod errors;
pub mod event_handler;
pub mod extensions;
pub mod middleware;
pub mod models;
pub mod outcomes;
pub mod publisher;
pub mod route;
#[cfg(feature = "test-utils")]
pub mod test_utils;
pub mod traits;
pub mod type_handler;

pub use anyhow;
pub use canonical_message::{CanonicalMessage, MessageContext};
pub use errors::HandlerError;
pub use models::Route;
pub use outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};
pub use publisher::Publisher;

pub use endpoints::memory::get_or_create_channel;
pub use publisher::{get_publisher, unregister_publisher};
pub use route::{get_route, list_routes, stop_route};

pub mod consumer {
    pub use crate::middleware::apply_middlewares_to_consumer as apply_middlewares;
}

/// The application name, derived from the package name in Cargo.toml.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
