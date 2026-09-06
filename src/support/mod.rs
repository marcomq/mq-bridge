//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Cross-cutting support utilities used across endpoints and middleware:
//! cryptographic primitives, payload (de)compression, `${...}` string
//! interpolation, the shared connection registry, and the endpoint-plugin C ABI.

pub mod base64_engine;
#[cfg(feature = "compression")]
pub(crate) mod compression;
#[cfg(any(feature = "compression", feature = "http"))]
pub(crate) mod compression_pool;
pub mod connection_registry;
#[cfg(feature = "encryption")]
pub mod crypto;
pub(crate) mod crypto_envelope;
pub mod interpolation;
pub(crate) mod parallel;
/// The stable C ABI shared with dynamically loaded endpoint plugins.
#[cfg(feature = "plugin")]
pub mod plugin_abi;
pub mod source_ranges;
