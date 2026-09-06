//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! gRPC endpoint shared protocol and transport support.

mod consumer;
mod dynamic;
mod publisher;
mod server;

pub use consumer::GrpcConsumer;
pub use publisher::{create_grpc_publisher, GrpcPublisher};

use crate::models::GrpcConfig;
use crate::CanonicalMessage;
use anyhow::Result;
use bytes::{Buf, BufMut};
use std::collections::HashMap;
use std::time::Duration;
use tonic::metadata::MetadataMap;
use tonic::transport::Channel;
use tonic::transport::{Certificate, ClientTlsConfig, Identity};
use tonic::Status;
use uuid::Uuid;

pub mod proto {
    #![allow(clippy::all)]
    tonic::include_proto!("mqbridge");

    /// Encoded descriptors for the stable `mqbridge` public API.
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("mqbridge_descriptor");
}

use proto::bridge_client::BridgeClient;
use proto::BridgeMessage;

/// Structured failure returned by descriptor-driven gRPC calls.
///
/// `Display` and `Debug` intentionally omit trailing metadata values so credentials
/// returned by a peer cannot leak through ordinary error logging. Callers that need
/// protocol details can inspect [`Self::trailing_metadata`] explicitly.
pub struct GrpcStatusError {
    code: tonic::Code,
    message: String,
    trailing_metadata: MetadataMap,
}

impl GrpcStatusError {
    pub fn code(&self) -> tonic::Code {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn trailing_metadata(&self) -> &MetadataMap {
        &self.trailing_metadata
    }
}

impl From<Status> for GrpcStatusError {
    fn from(status: Status) -> Self {
        Self {
            code: status.code(),
            message: status.message().to_owned(),
            trailing_metadata: status.metadata().clone(),
        }
    }
}

impl std::fmt::Display for GrpcStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "gRPC status {:?}: {}", self.code, self.message)
    }
}

impl std::fmt::Debug for GrpcStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GrpcStatusError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("trailing_metadata_entries", &self.trailing_metadata.len())
            .finish()
    }
}

impl std::error::Error for GrpcStatusError {}

pub(super) const GRPC_BATCH_POLL_MS: u64 = 15; // Increased for better batching in performance tests, and to reduce "poll timed out" warnings
/// Acks for one committed batch that may be in flight at once.
pub(super) const GRPC_ACK_CONCURRENCY: usize = 64;

#[derive(Clone, Default)]
pub(super) struct RawProtobufCodec;

#[derive(Clone, Default)]
pub(super) struct RawProtobufEncoder;

#[derive(Clone, Default)]
pub(super) struct RawProtobufDecoder;

impl tonic::codec::Codec for RawProtobufCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawProtobufEncoder;
    type Decoder = RawProtobufDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        RawProtobufEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        RawProtobufDecoder
    }
}

impl tonic::codec::Encoder for RawProtobufEncoder {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut tonic::codec::EncodeBuf<'_>,
    ) -> std::result::Result<(), Self::Error> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl tonic::codec::Decoder for RawProtobufDecoder {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(
        &mut self,
        src: &mut tonic::codec::DecodeBuf<'_>,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        Ok(Some(src.copy_to_bytes(src.remaining()).to_vec()))
    }
}

pub(super) fn canonical_to_bridge(message: CanonicalMessage, topic: Option<&str>) -> BridgeMessage {
    let mut metadata: HashMap<String, String> = message
        .metadata
        .into_iter()
        .filter(|(key, _)| !crate::canonical_message::is_source_metadata_key(key))
        .collect();
    if let Some(topic) = topic {
        metadata
            .entry("mq_bridge.topic".to_string())
            .or_insert_with(|| topic.to_string());
    }
    BridgeMessage {
        payload: message.payload.to_vec(),
        id: fast_uuid_v7::format_uuid(message.message_id).to_string(),
        metadata,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(super) fn bridge_to_canonical(msg: BridgeMessage) -> CanonicalMessage {
    let message_id = if msg.id.is_empty() {
        None
    } else if let Ok(uuid) = Uuid::parse_str(&msg.id) {
        Some(uuid.as_u128())
    } else if msg.id.starts_with("0x") || msg.id.starts_with("0X") {
        u128::from_str_radix(msg.id.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
    } else {
        msg.id.parse::<u128>().ok()
    };
    CanonicalMessage::new(msg.payload, message_id).with_metadata(msg.metadata)
}

pub(super) async fn make_endpoint(
    config: &GrpcConfig,
    url: &str,
) -> Result<tonic::transport::Endpoint> {
    if config.tls.accept_invalid_certs {
        return Err(anyhow::anyhow!(
            "gRPC clients do not support tls.accept_invalid_certs"
        ));
    }
    let mut endpoint = tonic::transport::Endpoint::from_shared(url.to_string())?;

    if config.tls.required {
        let mut tls_config = ClientTlsConfig::new();
        if let Some(ca_path) = &config.tls.ca_file {
            let ca_pem = tokio::fs::read(ca_path).await?;
            let ca_cert = Certificate::from_pem(ca_pem);
            tls_config = tls_config.ca_certificate(ca_cert);
        }
        if let (Some(cert_path), Some(key_path)) = (&config.tls.cert_file, &config.tls.key_file) {
            let cert_pem = tokio::fs::read(cert_path).await?;
            let key_pem = tokio::fs::read(key_path).await?;
            let identity = Identity::from_pem(cert_pem, key_pem);
            tls_config = tls_config.identity(identity);
        }
        endpoint = endpoint.tls_config(tls_config)?;
    }

    if let Some(ms) = config.connect_timeout_ms.or(config.timeout_ms) {
        endpoint = endpoint.connect_timeout(Duration::from_millis(ms));
    }
    if let Some(v) = config.initial_stream_window_size {
        endpoint = endpoint.initial_stream_window_size(v);
    }
    if let Some(v) = config.initial_connection_window_size {
        endpoint = endpoint.initial_connection_window_size(v);
    }
    if let Some(ms) = config.http2_keepalive_interval_ms {
        endpoint = endpoint.http2_keep_alive_interval(Duration::from_millis(ms));
    }
    if let Some(ms) = config.http2_keepalive_timeout_ms {
        endpoint = endpoint.keep_alive_timeout(Duration::from_millis(ms));
    }

    Ok(endpoint)
}

pub(super) fn configured_client(config: &GrpcConfig, channel: Channel) -> BridgeClient<Channel> {
    let mut client = BridgeClient::new(channel);
    if let Some(max) = config.max_decoding_message_size {
        client = client.max_decoding_message_size(max);
    }
    if let Some(max) = config.max_encoding_message_size {
        client = client.max_encoding_message_size(max);
    }
    client
}

pub(super) fn parse_addr(url: &str) -> Result<std::net::SocketAddr> {
    let stripped = url.find("://").map(|p| &url[p + 3..]).unwrap_or(url);
    let host = stripped
        .find('/')
        .map(|p| &stripped[..p])
        .unwrap_or(stripped);
    host.parse()
        .map_err(|e| anyhow::anyhow!("Invalid gRPC server address '{}': {}", host, e))
}

#[cfg(test)]
mod tests;
