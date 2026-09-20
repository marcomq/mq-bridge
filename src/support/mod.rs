//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Cross-cutting support utilities used across endpoints and middleware:
//! cryptographic primitives, payload (de)compression, `${...}` string
//! interpolation, the shared connection registry, the configuration schema a
//! custom endpoint declares about itself, and the endpoint-plugin C ABI.

pub mod base64_engine;
#[cfg(feature = "compression")]
pub(crate) mod compression;
#[cfg(any(feature = "compression", feature = "http"))]
pub(crate) mod compression_pool;
pub mod config_schema;
pub mod connection_registry;
#[cfg(feature = "encryption")]
pub mod crypto;
pub(crate) mod crypto_envelope;
pub mod interpolation;
pub(crate) mod pack;
pub(crate) mod parallel;
/// The stable C ABI shared with dynamically loaded endpoint plugins.
#[cfg(feature = "plugin")]
pub mod plugin_abi;
pub mod source_ranges;
