//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Structural endpoints.
//!
//! These do not talk to an external system. They compose, route, or terminate the
//! message flow and appear wherever an endpoint is expected. Their config variants
//! are tagged `"format": "structural_endpoint"` in the JSON schema.
//!
//! See `docs/REFERENCE.md` for fields, defaults and examples.

pub mod fanout;
pub mod null;
pub mod reader;
pub mod request;
pub mod response;
pub mod static_endpoint;
pub mod stream_buffer;
pub mod switch;
