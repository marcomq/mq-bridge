//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Descriptor-driven gRPC: an arbitrary method described by a descriptor set or
//! server reflection is used directly as a route's input or output.

use super::{make_endpoint, GrpcStatusError, RawProtobufCodec, GRPC_BATCH_POLL_MS};
use crate::models::GrpcConfig;
use crate::traits::{ConsumerError, MessageConsumer, MessagePublisher, PublisherError, SentBatch};
use crate::CanonicalMessage;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use sha2::{Digest, Sha256};
use std::any::Any;
use std::time::Duration;
use tonic::metadata::{Ascii, Binary, MetadataKey, MetadataMap, MetadataValue};
use tonic::transport::Channel;
use tonic::Request;
use tonic::Status;
use tracing::warn;

/// Await `call`, failing with a clear error if `deadline` passes first. No deadline
/// configured means no bound, matching the rest of the endpoint.
async fn with_deadline<T>(
    call: impl std::future::Future<Output = Result<T, Status>>,
    deadline: Option<Duration>,
) -> Result<T> {
    match deadline {
        Some(deadline) => tokio::time::timeout(deadline, call)
            .await
            .map_err(|_| anyhow::anyhow!("dynamic gRPC call timed out after {deadline:?}"))?
            .map_err(|status| anyhow::Error::new(GrpcStatusError::from(status))),
        None => call
            .await
            .map_err(|status| anyhow::Error::new(GrpcStatusError::from(status))),
    }
}

/// Binary metadata names survive `extract_secrets` hex-encoded, so a name that is not a
/// usable `-bin` key on its own is retried decoded before it is rejected.
fn binary_metadata_key(name: &str) -> Result<MetadataKey<Binary>> {
    if let Ok(key) = MetadataKey::<Binary>::from_bytes(name.as_bytes()) {
        return Ok(key);
    }
    let decoded = crate::models::decode_secret_map_key(name)
        .and_then(|decoded| MetadataKey::<Binary>::from_bytes(decoded.as_bytes()).ok());
    decoded.ok_or_else(|| anyhow::anyhow!("invalid binary gRPC metadata key '{name}'"))
}

/// A credential carried as static metadata is held to the same bar as `bearer_token`,
/// under either the text or the `-bin` key, and hex-encoded by the secret round trip.
fn is_credential_metadata_key(name: &str, api_key_name: &str) -> bool {
    // Both sides are normalized: an `api_key_name` that itself ends in `-bin` would
    // otherwise never match the metadata key carrying it.
    let normalize = |name: &str| {
        let name = name.to_ascii_lowercase();
        name.strip_suffix("-bin").unwrap_or(&name).to_owned()
    };
    let api_key_name = normalize(api_key_name);
    let matches = |name: &str| {
        let name = normalize(name);
        name == "authorization" || name == api_key_name
    };
    matches(name)
        || crate::models::decode_secret_map_key(name).is_some_and(|decoded| matches(&decoded))
}

/// Attaches the configured static metadata and credentials. Error text never
/// includes a configured value, so an unusable credential cannot leak through logs.
pub(super) fn apply_call_metadata(config: &GrpcConfig, metadata: &mut MetadataMap) -> Result<()> {
    let normalized_url = config.tls.normalize_url(&config.url);
    let api_key_name = config.api_key_name.as_deref().unwrap_or("x-api-key");
    let sends_credentials = config.bearer_token.is_some()
        || config.api_key.is_some()
        || config
            .metadata
            .keys()
            .chain(config.binary_metadata.keys())
            .any(|name| is_credential_metadata_key(name, api_key_name));
    if sends_credentials
        && !normalized_url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        anyhow::bail!(
            "gRPC bearer_token, api_key and credential metadata require an https:// endpoint"
        );
    }

    for (name, value) in &config.metadata {
        let key = MetadataKey::<Ascii>::from_bytes(name.as_bytes())
            .map_err(|error| anyhow::anyhow!("invalid gRPC metadata key '{name}': {error}"))?;
        let value = MetadataValue::<Ascii>::try_from(value.as_str()).map_err(|error| {
            anyhow::anyhow!("invalid gRPC metadata value for '{name}': {error}")
        })?;
        metadata.insert(key, value);
    }
    for (name, value) in &config.binary_metadata {
        metadata.insert_bin(
            binary_metadata_key(name)?,
            MetadataValue::<Binary>::from_bytes(value),
        );
    }
    if let Some(token) = &config.bearer_token {
        let value = MetadataValue::<Ascii>::try_from(format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("bearer_token is not a valid gRPC metadata value"))?;
        metadata.insert("authorization", value);
    }
    if let Some(api_key) = &config.api_key {
        let key = MetadataKey::<Ascii>::from_bytes(api_key_name.as_bytes())
            .map_err(|error| anyhow::anyhow!("invalid api_key_name '{api_key_name}': {error}"))?;
        let value = MetadataValue::<Ascii>::try_from(api_key.as_str())
            .map_err(|_| anyhow::anyhow!("api_key is not a valid gRPC metadata value"))?;
        metadata.insert(key, value);
    }

    Ok(())
}

/// Metadata and credentials are only attached to descriptor-driven calls. Accepting them
/// silently elsewhere would connect unauthenticated, so those modes reject them instead.
pub(super) fn reject_unsupported_call_metadata(config: &GrpcConfig, mode: &str) -> Result<()> {
    let mut set = Vec::new();
    if !config.metadata.is_empty() {
        set.push("metadata");
    }
    if !config.binary_metadata.is_empty() {
        set.push("binary_metadata");
    }
    if config.bearer_token.is_some() {
        set.push("bearer_token");
    }
    if config.api_key.is_some() {
        set.push("api_key");
    }
    if set.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "gRPC {mode} mode does not send {}; these apply only to dynamic \
         descriptor-driven calls. Use TLS client certificates for the Bridge protocol.",
        set.join(", ")
    )
}

fn dynamic_request(config: &GrpcConfig, payload: Vec<u8>) -> Result<Request<Vec<u8>>> {
    let mut request = Request::new(payload);
    apply_call_metadata(config, request.metadata_mut())?;
    Ok(request)
}

/// Connects and resolves the configured service/method from a descriptor set, a
/// descriptor file, or server reflection. Shared by the dynamic source and sink so both
/// accept the same configuration and produce the same errors.
async fn resolve_dynamic_method(
    config: &GrpcConfig,
    url: &str,
) -> Result<(Channel, prost_reflect::MethodDescriptor)> {
    let service_name = config
        .service_name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("dynamic gRPC requires service_name"))?;
    let method_name = config
        .method_name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("dynamic gRPC requires method_name"))?;

    let channel = make_endpoint(config, url).await?.connect().await?;
    let request_deadline = config
        .request_timeout_ms
        .or(config.timeout_ms)
        .map(Duration::from_millis);
    let pool = if let Some(bytes) = &config.descriptor_set_bytes {
        DescriptorPool::decode(bytes.as_slice())?
    } else if let Some(path) = &config.descriptor_set_path {
        let bytes = tokio::fs::read(path).await?;
        DescriptorPool::decode(bytes.as_slice())?
    } else if config.reflection {
        reflected_descriptor_pool(config, channel.clone(), service_name, request_deadline).await?
    } else {
        anyhow::bail!(
            "dynamic gRPC requires descriptor_set_bytes, descriptor_set_path, or reflection: true"
        );
    };

    let service = pool.get_service_by_name(service_name).ok_or_else(|| {
        anyhow::anyhow!(
            "gRPC service '{}' not found in the discovered descriptors",
            service_name
        )
    })?;
    let method = service
        .methods()
        .find(|method| method.name() == method_name)
        .ok_or_else(|| {
            anyhow::anyhow!("gRPC method '{}.{}' not found", service_name, method_name)
        })?;
    Ok((channel, method))
}

/// Names an RPC's streaming shape for capability errors.
fn method_shape(method: &prost_reflect::MethodDescriptor) -> &'static str {
    match (method.is_client_streaming(), method.is_server_streaming()) {
        (true, true) => "bidirectional-streaming",
        (true, false) => "client-streaming",
        (false, true) => "server-streaming",
        (false, false) => "unary",
    }
}

/// Encodes one canonical payload as the method's protobuf input message. A payload that
/// does not match the descriptor is permanent: retrying re-encodes the same bytes.
fn encode_dynamic_input(
    method: &prost_reflect::MethodDescriptor,
    payload: &[u8],
) -> Result<Vec<u8>, PublisherError> {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let message =
        DynamicMessage::deserialize(method.input(), &mut deserializer).map_err(|error| {
            PublisherError::NonRetryable(anyhow::anyhow!(
                "gRPC payload does not match '{}': {error}",
                method.input().full_name()
            ))
        })?;
    let mut bytes = Vec::with_capacity(message.encoded_len());
    message.encode(&mut bytes).map_err(|error| {
        PublisherError::NonRetryable(anyhow::anyhow!("gRPC payload encode failed: {error}"))
    })?;
    Ok(bytes)
}

/// Decodes a protobuf response into a canonical message carrying the originating id, so
/// the route can correlate it as a reply.
fn decode_dynamic_output(
    method: &prost_reflect::MethodDescriptor,
    bytes: &[u8],
    correlation_id: Option<u128>,
) -> Result<CanonicalMessage, PublisherError> {
    let message = DynamicMessage::decode(method.output(), bytes).map_err(|error| {
        PublisherError::NonRetryable(anyhow::anyhow!("gRPC response decode failed: {error}"))
    })?;
    let payload = serde_json::to_vec(&message).map_err(|error| {
        PublisherError::NonRetryable(anyhow::anyhow!("gRPC response encode failed: {error}"))
    })?;
    Ok(CanonicalMessage::new(payload, correlation_id))
}

pub(super) async fn reflected_descriptor_pool(
    config: &GrpcConfig,
    channel: Channel,
    service_name: &str,
    deadline: Option<Duration>,
) -> Result<DescriptorPool> {
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
    use tonic_reflection::pb::v1::ServerReflectionRequest;

    let reflection_request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::FileContainingSymbol(
            service_name.to_owned(),
        )),
    };
    // Reflection is an ordinary RPC, so a server that guards it needs the same
    // credentials as the call the descriptors are being fetched for.
    let mut request = Request::new(tokio_stream::iter([reflection_request]));
    apply_call_metadata(config, request.metadata_mut())?;
    let mut client = ServerReflectionClient::new(channel);
    let call = client.server_reflection_info(request);
    let mut responses = with_deadline(call, deadline).await?.into_inner();
    let response = with_deadline(responses.message(), deadline)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("gRPC reflection returned no descriptor for '{service_name}'")
        })?;
    let descriptors = match response.message_response {
        Some(MessageResponse::FileDescriptorResponse(response)) => response.file_descriptor_proto,
        Some(MessageResponse::ErrorResponse(error)) => anyhow::bail!(
            "gRPC reflection failed for '{service_name}' with code {}: {}",
            error.error_code,
            error.error_message
        ),
        _ => anyhow::bail!("gRPC reflection returned an unexpected response for '{service_name}'"),
    };
    let files = descriptors
        .into_iter()
        .map(|bytes| prost_types::FileDescriptorProto::decode(bytes.as_slice()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut pool = DescriptorPool::new();
    pool.add_file_descriptor_protos(files)?;
    Ok(pool)
}

/// `overall_timeout_ms` caps the lifetime of the RPC, so it is permanent: a reconnect
/// would restart the call and recompute the deadline, turning the cap into an endless
/// restart loop. The idle timeout stays retryable, where reconnecting is the point.
fn overall_deadline_exceeded() -> ConsumerError {
    ConsumerError::Permanent(anyhow::anyhow!("dynamic gRPC overall deadline exceeded"))
}

enum DynamicResponse {
    Unary(Option<Vec<u8>>),
    // Boxed: an inline `Streaming` is an order of magnitude larger than the unary arm.
    Streaming(Box<tonic::Streaming<Vec<u8>>>),
}

pub(super) struct DynamicConsumer {
    response: DynamicResponse,
    output: MessageDescriptor,
    service_name: String,
    method_name: String,
    response_index: u64,
    idle_stream_timeout: Option<Duration>,
    overall_deadline: Option<tokio::time::Instant>,
    exit_on_empty: bool,
}

impl DynamicConsumer {
    pub(super) async fn new(config: &GrpcConfig, url: &str) -> Result<Self> {
        let (channel, method) = resolve_dynamic_method(config, url).await?;
        let service_name = method.parent_service().full_name().to_owned();
        let method_name = method.name().to_owned();
        let request_deadline = config
            .request_timeout_ms
            .or(config.timeout_ms)
            .map(Duration::from_millis);
        if method.is_client_streaming() {
            anyhow::bail!(
                "dynamic gRPC method '{}.{}' is {}; a gRPC *input* consumes responses, so it \
                 supports unary and server-streaming methods only. A method that streams \
                 requests is a sink: use it as the route's output instead",
                service_name,
                method_name,
                method_shape(&method)
            );
        }
        if config.server_streaming && !method.is_server_streaming() {
            warn!(
                service = service_name,
                method = method_name,
                "Ignoring deprecated server_streaming hint; the RPC shape is descriptor-derived"
            );
        }

        let request_json = config
            .request
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let request_text = serde_json::to_string(&request_json)?;
        let mut deserializer = serde_json::Deserializer::from_str(&request_text);
        let request = DynamicMessage::deserialize(method.input(), &mut deserializer)?;
        let mut request_bytes = Vec::with_capacity(request.encoded_len());
        request.encode(&mut request_bytes)?;

        let mut client = tonic::client::Grpc::new(channel);
        if let Some(max) = config.max_decoding_message_size {
            client = client.max_decoding_message_size(max);
        }
        if let Some(max) = config.max_encoding_message_size {
            client = client.max_encoding_message_size(max);
        }
        client
            .ready()
            .await
            .map_err(|error| anyhow::anyhow!("dynamic gRPC service was not ready: {error}"))?;
        let path = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(format!(
            "/{service_name}/{method_name}"
        ))?;
        let overall_timeout = config.overall_timeout_ms.map(Duration::from_millis);
        let overall_deadline = overall_timeout.map(|timeout| tokio::time::Instant::now() + timeout);
        let response = if method.is_server_streaming() {
            let call = client.server_streaming(
                dynamic_request(config, request_bytes)?,
                path,
                RawProtobufCodec,
            );
            DynamicResponse::Streaming(Box::new(
                with_deadline(call, request_deadline).await?.into_inner(),
            ))
        } else {
            let call = client.unary(
                dynamic_request(config, request_bytes)?,
                path,
                RawProtobufCodec,
            );
            DynamicResponse::Unary(Some(
                with_deadline(call, request_deadline).await?.into_inner(),
            ))
        };
        Ok(Self {
            response,
            output: method.output(),
            service_name: service_name.to_owned(),
            method_name: method_name.to_owned(),
            response_index: 0,
            idle_stream_timeout: config.idle_stream_timeout_ms.map(Duration::from_millis),
            overall_deadline,
            exit_on_empty: false,
        })
    }

    /// A body that does not match the descriptor is a permanent error, not a connection
    /// one: reconnecting re-reads the same bytes and fails identically.
    fn decode_message(&mut self, bytes: &[u8]) -> Result<CanonicalMessage, ConsumerError> {
        // Claim the position before decoding. Ids are advertised as deterministic, so the
        // index has to follow stream position; a skipped response would otherwise shift
        // every id after it.
        let index = self.response_index;
        self.response_index = self.response_index.saturating_add(1);

        let message = DynamicMessage::decode(self.output.clone(), bytes)
            .map_err(|error| ConsumerError::Permanent(error.into()))?;
        let payload =
            serde_json::to_vec(&message).map_err(|error| ConsumerError::Permanent(error.into()))?;

        let mut hasher = Sha256::new();
        hasher.update(b"mqbridge.dynamic-response.v1\0");
        hasher.update(self.service_name.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.method_name.as_bytes());
        hasher.update(b"\0");
        hasher.update(index.to_be_bytes());
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut id = [0_u8; 16];
        id.copy_from_slice(&digest[..16]);

        Ok(
            CanonicalMessage::new(payload, Some(u128::from_be_bytes(id)))
                .with_metadata_kv("grpc.service", self.service_name.clone())
                .with_metadata_kv("grpc.method", self.method_name.clone())
                .with_metadata_kv("grpc.response_index", index.to_string())
                .with_metadata_kv("grpc.ack_guarantee", "none"),
        )
    }
}

#[async_trait]
impl MessageConsumer for DynamicConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<crate::outcomes::ReceivedBatch, ConsumerError> {
        let max_messages = max_messages.max(1);
        let mut raw = Vec::with_capacity(max_messages);
        let idle_timeout = self.idle_stream_timeout;
        let overall_deadline = self.overall_deadline;
        let exit_on_empty = self.exit_on_empty;
        match &mut self.response {
            DynamicResponse::Unary(message) => {
                if let Some(message) = message.take() {
                    raw.push(message);
                }
            }
            DynamicResponse::Streaming(stream) => {
                while raw.len() < max_messages {
                    let overall_remaining = overall_deadline.map(|deadline| {
                        deadline.saturating_duration_since(tokio::time::Instant::now())
                    });
                    if overall_remaining == Some(Duration::ZERO) {
                        return Err(overall_deadline_exceeded());
                    }
                    let next_result = if raw.is_empty() {
                        let wait = crate::traits::drain_gated(exit_on_empty, stream.message());
                        let timeout = match (idle_timeout, overall_remaining) {
                            (Some(idle), Some(overall)) => Some(idle.min(overall)),
                            (Some(idle), None) => Some(idle),
                            (None, Some(overall)) => Some(overall),
                            (None, None) => None,
                        };
                        let next = match timeout {
                            Some(timeout) => {
                                tokio::time::timeout(timeout, wait).await.map_err(|_| {
                                    if overall_deadline.is_some_and(|deadline| {
                                        tokio::time::Instant::now() >= deadline
                                    }) {
                                        overall_deadline_exceeded()
                                    } else {
                                        ConsumerError::Connection(anyhow::anyhow!(
                                            "dynamic gRPC response stream idle timeout exceeded"
                                        ))
                                    }
                                })?
                            }
                            None => wait.await,
                        };
                        match next {
                            Some(result) => result,
                            None => return Ok(crate::outcomes::ReceivedBatch::empty()),
                        }
                    } else {
                        let poll = Duration::from_millis(GRPC_BATCH_POLL_MS);
                        let timeout =
                            overall_remaining.map_or(poll, |remaining| poll.min(remaining));
                        match tokio::time::timeout(timeout, stream.message()).await {
                            Ok(result) => result,
                            Err(_) if timeout == poll => break,
                            Err(_) => return Err(overall_deadline_exceeded()),
                        }
                    };
                    match next_result {
                        Ok(Some(message)) => raw.push(message),
                        Ok(None) => break,
                        Err(status) => {
                            return Err(ConsumerError::Connection(anyhow::Error::new(
                                GrpcStatusError::from(status),
                            )))
                        }
                    }
                }
            }
        }
        if raw.is_empty() {
            return Err(ConsumerError::EndOfStream);
        }

        // Skip what will not decode rather than failing the batch: these bytes are already
        // off the stream and cannot be re-read, so discarding the whole batch for one bad
        // message would silently drop every healthy message alongside it.
        let mut messages = Vec::with_capacity(raw.len());
        for bytes in &raw {
            match self.decode_message(bytes) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    warn!(%error, "Dropping a dynamic gRPC response that does not match the descriptor")
                }
            }
        }
        if messages.is_empty() {
            return Err(ConsumerError::Permanent(anyhow::anyhow!(
                "every message in the dynamic gRPC batch failed to decode"
            )));
        }
        Ok(crate::outcomes::ReceivedBatch {
            messages,
            // Dynamic services define no acknowledgement operation. This only tells the
            // route that the already-received response may be released locally.
            commit: Box::new(|_| Box::pin(async { Ok(()) })),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── Dynamic publisher ─────────────────────────────────────────────────────────

/// In-flight unary sends per batch. Matches `GRPC_ACK_CONCURRENCY`: both bound work that
/// one HTTP/2 connection multiplexes.
const GRPC_DYNAMIC_SEND_CONCURRENCY: usize = 64;

/// Calls an arbitrary descriptor-defined method as a route's output.
///
/// Unary methods make one call per message. Client-streaming methods make one call per
/// batch, which is also the acknowledgement granularity: the single reply covers every
/// message in the batch, and a failure part-way through cannot say which ones the server
/// already consumed, so a retry redelivers all of them.
pub(super) struct DynamicPublisher {
    client: tonic::client::Grpc<Channel>,
    method: prost_reflect::MethodDescriptor,
    path: tonic::codegen::http::uri::PathAndQuery,
    config: GrpcConfig,
    service_name: String,
    method_name: String,
    request_timeout: Option<Duration>,
    overall_timeout: Option<Duration>,
}

impl DynamicPublisher {
    pub(super) async fn new(config: &GrpcConfig, url: &str) -> Result<Self> {
        let (channel, method) = resolve_dynamic_method(config, url).await?;
        let service_name = method.parent_service().full_name().to_owned();
        let method_name = method.name().to_owned();
        if method.is_server_streaming() {
            anyhow::bail!(
                "dynamic gRPC method '{}.{}' is {}; a gRPC *output* publishes messages and \
                 consumes one reply, so it supports unary and client-streaming methods only. \
                 A method that streams responses is a source: use it as the route's input instead",
                service_name,
                method_name,
                method_shape(&method)
            );
        }
        // A bare `request:` in YAML deserializes to null; only a real value is a mistake here.
        if config
            .request
            .as_ref()
            .is_some_and(|value| !value.is_null())
        {
            anyhow::bail!(
                "dynamic gRPC output does not use `request`: the published messages are the \
                 requests. Remove `request`, or move this endpoint to the route's input"
            );
        }

        let mut client = tonic::client::Grpc::new(channel);
        if let Some(max) = config.max_decoding_message_size {
            client = client.max_decoding_message_size(max);
        }
        if let Some(max) = config.max_encoding_message_size {
            client = client.max_encoding_message_size(max);
        }
        let path = tonic::codegen::http::uri::PathAndQuery::from_maybe_shared(format!(
            "/{service_name}/{method_name}"
        ))?;

        Ok(Self {
            client,
            method,
            path,
            config: config.clone(),
            service_name,
            method_name,
            request_timeout: config
                .request_timeout_ms
                .or(config.timeout_ms)
                .map(Duration::from_millis),
            overall_timeout: config.overall_timeout_ms.map(Duration::from_millis),
        })
    }

    async fn send_unary(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let results = futures::stream::iter(messages.into_iter().map(|message| async move {
            let bytes = match encode_dynamic_input(&self.method, &message.payload) {
                Ok(bytes) => bytes,
                Err(error) => return (message, Err(error)),
            };
            let request = match dynamic_request(&self.config, bytes) {
                Ok(request) => request,
                Err(error) => return (message, Err(PublisherError::NonRetryable(error))),
            };
            let mut client = self.client.clone();
            if let Err(error) = client.ready().await {
                return (
                    message,
                    Err(PublisherError::Retryable(anyhow::anyhow!(
                        "dynamic gRPC service was not ready: {error}"
                    ))),
                );
            }
            let call = client.unary(request, self.path.clone(), RawProtobufCodec);
            let response = match self.request_timeout {
                Some(timeout) => match tokio::time::timeout(timeout, call).await {
                    Ok(response) => response,
                    Err(_) => {
                        return (
                            message,
                            Err(PublisherError::Retryable(anyhow::anyhow!(
                                "dynamic gRPC call timed out"
                            ))),
                        )
                    }
                },
                None => call.await,
            };
            match response {
                Ok(response) => {
                    let reply = decode_dynamic_output(
                        &self.method,
                        response.get_ref(),
                        Some(message.message_id),
                    );
                    match reply {
                        Ok(reply) => (message, Ok(reply)),
                        Err(error) => (message, Err(error)),
                    }
                }
                Err(status) => (message, Err(status_to_publisher_error(status))),
            }
        }))
        .buffered(GRPC_DYNAMIC_SEND_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        let mut responses = Vec::with_capacity(results.len());
        let mut failed = Vec::new();
        for (message, result) in results {
            match result {
                Ok(reply) => responses.push(reply),
                Err(error) => failed.push((message, error)),
            }
        }
        Ok(SentBatch::Partial {
            responses: Some(responses),
            failed,
        })
    }

    async fn send_client_streaming(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Encode everything first: a payload that does not match the descriptor fails
        // permanently, and finding that out mid-stream would leave a half-sent RPC.
        let mut encoded = Vec::with_capacity(messages.len());
        let mut failed = Vec::new();
        let mut streamed = Vec::with_capacity(messages.len());
        for message in messages {
            match encode_dynamic_input(&self.method, &message.payload) {
                Ok(bytes) => {
                    encoded.push(bytes);
                    streamed.push(message);
                }
                Err(error) => failed.push((message, error)),
            }
        }
        if encoded.is_empty() {
            return Ok(SentBatch::Partial {
                responses: Some(Vec::new()),
                failed,
            });
        }

        let correlation_id = streamed.first().map(|message| message.message_id);
        let request = dynamic_request(&self.config, Vec::new())
            .map_err(PublisherError::NonRetryable)?
            .map(|_| tokio_stream::iter(encoded));
        let mut client = self.client.clone();
        client.ready().await.map_err(|error| {
            PublisherError::Retryable(anyhow::anyhow!(
                "dynamic gRPC service was not ready: {error}"
            ))
        })?;
        let call = client.client_streaming(request, self.path.clone(), RawProtobufCodec);
        let response = match self.request_timeout {
            Some(timeout) => tokio::time::timeout(timeout, call).await.map_err(|_| {
                PublisherError::Retryable(anyhow::anyhow!("dynamic gRPC call timed out"))
            })?,
            None => call.await,
        };

        match response {
            Ok(response) => {
                match decode_dynamic_output(&self.method, response.get_ref(), correlation_id) {
                    Ok(reply) => Ok(SentBatch::Partial {
                        responses: Some(vec![reply]),
                        failed,
                    }),
                    Err(error) => {
                        let message = error.to_string();
                        failed.extend(streamed.into_iter().map(|sent| {
                            (
                                sent,
                                PublisherError::NonRetryable(anyhow::anyhow!(message.clone())),
                            )
                        }));
                        Ok(SentBatch::Partial {
                            responses: Some(Vec::new()),
                            failed,
                        })
                    }
                }
            }
            // One reply covers the whole stream, so a failure fails every message in it.
            Err(status) => {
                let error = status_to_publisher_error(status);
                let message = error.to_string();
                failed.extend(streamed.into_iter().map(|sent| {
                    (
                        sent,
                        match error {
                            PublisherError::NonRetryable(_) => {
                                PublisherError::NonRetryable(anyhow::anyhow!(message.clone()))
                            }
                            _ => PublisherError::Retryable(anyhow::anyhow!(message.clone())),
                        },
                    )
                }));
                Ok(SentBatch::Partial {
                    responses: Some(Vec::new()),
                    failed,
                })
            }
        }
    }
}

/// gRPC codes that mean "this request will never succeed" become non-retryable so the
/// route dead-letters instead of replaying a request the server already rejected.
fn status_to_publisher_error(status: Status) -> PublisherError {
    let permanent = matches!(
        status.code(),
        tonic::Code::InvalidArgument
            | tonic::Code::NotFound
            | tonic::Code::AlreadyExists
            | tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::FailedPrecondition
            | tonic::Code::OutOfRange
            | tonic::Code::Unimplemented
    );
    let error = anyhow::Error::new(GrpcStatusError::from(status));
    if permanent {
        PublisherError::NonRetryable(error)
    } else {
        PublisherError::Retryable(error)
    }
}

#[async_trait]
impl MessagePublisher for DynamicPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        let send = async {
            if self.method.is_client_streaming() {
                self.send_client_streaming(messages).await
            } else {
                self.send_unary(messages).await
            }
        };
        match self.overall_timeout {
            Some(timeout) => tokio::time::timeout(timeout, send).await.map_err(|_| {
                PublisherError::Retryable(anyhow::anyhow!("dynamic gRPC batch timed out"))
            })?,
            None => send.await,
        }
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        crate::traits::EndpointStatus {
            healthy: true,
            details: serde_json::json!({
                "mode": "dynamic-client",
                "service": self.service_name,
                "method": self.method_name,
                "shape": method_shape(&self.method),
                "acknowledgement_guarantee": if self.method.is_client_streaming() {
                    "batch"
                } else {
                    "per-message"
                },
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
