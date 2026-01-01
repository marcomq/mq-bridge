//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
pub mod canonical_message;
pub mod command_handler;
pub mod endpoints;
pub mod errors;
pub mod event_handler;
pub mod middleware;
pub mod models;
pub mod outcomes;
pub mod route;
pub mod traits;
pub mod type_handler;

pub use canonical_message::{CanonicalMessage, MessageContext};
pub use errors::HandlerError;
pub use models::Route;
pub use outcomes::{Handled, Received, ReceivedBatch, Sent, SentBatch};

/// The application name, derived from the package name in Cargo.toml.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
