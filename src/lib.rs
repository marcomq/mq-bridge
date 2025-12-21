//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

pub mod canonical_message;
pub mod endpoints;
pub mod middleware;
pub mod models;
pub mod route;
pub mod traits;

pub use canonical_message::CanonicalMessage;
pub use models::Route;

pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
