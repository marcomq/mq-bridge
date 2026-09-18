//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Built-in Bridge consumer: consumes from a remote `mq-bridge` gRPC server.

use super::dynamic::reject_unsupported_call_metadata;
use super::proto;
use super::{
    bridge_to_canonical, configured_client, make_endpoint, GRPC_ACK_CONCURRENCY, GRPC_BATCH_POLL_MS,
};
use crate::models::GrpcConfig;
use crate::traits::{ConsumerError, MessageConsumer, MessageDisposition};
use anyhow::Result;
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use proto::bridge_client::BridgeClient;
use proto::{BridgeMessage, SubscribeRequest};
use std::any::Any;
use std::collections::HashMap;
use std::time::Duration;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{debug, error, info, trace, warn};

use super::dynamic::DynamicConsumer;
use super::server::ServerModeConsumer;

// ── Consumer ──────────────────────────────────────────────────────────────────

pub struct GrpcConsumer {
    inner: GrpcConsumerInner,
    url: String,
    pub(super) bound_addr: Option<std::net::SocketAddr>,
}

enum GrpcConsumerInner {
    Client(Box<ClientModeConsumer>),
    Dynamic(Box<DynamicConsumer>),
    Server(ServerModeConsumer),
}

impl GrpcConsumer {
    pub async fn new(config: &GrpcConfig) -> Result<Self> {
        let url = config.tls.normalize_url(&config.url);
        let (inner, bound_addr) = if config.server_mode {
            let s = ServerModeConsumer::new(config, &url).await?;
            let addr = s.bound_addr();
            (GrpcConsumerInner::Server(s), Some(addr))
        } else if config.descriptor_set_bytes.is_some()
            || config.descriptor_set_path.is_some()
            || config.reflection
        {
            (
                GrpcConsumerInner::Dynamic(Box::new(DynamicConsumer::new(config, &url).await?)),
                None,
            )
        } else {
            (
                GrpcConsumerInner::Client(Box::new(ClientModeConsumer::new(config, &url).await?)),
                None,
            )
        };
        Ok(Self {
            inner,
            url,
            bound_addr,
        })
    }

    /// True when `receive_batch` is cancel-safe. Server mode is mpsc-backed (a
    /// dropped read consumes nothing); client mode reads a tonic stream directly,
    /// where a cancelled `message()` may drop an in-flight frame.
    pub(crate) fn is_cancel_safe(&self) -> bool {
        matches!(self.inner, GrpcConsumerInner::Server(_))
    }
}

#[async_trait]
impl MessageConsumer for GrpcConsumer {
    // Client mode sends acknowledgement RPCs and server mode resolves per-message
    // completion channels. Both operations are independent across messages.
    fn commit_requires_order(&self) -> bool {
        false
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        match &mut self.inner {
            GrpcConsumerInner::Client(c) => c.set_exit_on_empty(exit_on_empty),
            GrpcConsumerInner::Dynamic(c) => c.set_exit_on_empty(exit_on_empty),
            GrpcConsumerInner::Server(s) => s.set_exit_on_empty(exit_on_empty),
        }
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<crate::outcomes::ReceivedBatch, ConsumerError> {
        match &mut self.inner {
            GrpcConsumerInner::Client(c) => c.receive_batch(max_messages).await,
            GrpcConsumerInner::Dynamic(c) => c.receive_batch(max_messages).await,
            GrpcConsumerInner::Server(s) => s.receive_batch(max_messages).await,
        }
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        // Server mode: healthy as long as the embedded server task is still
        // running. Client mode: a tonic client stream has no cheap liveness
        // probe, so report healthy and leave verification to the next receive.
        let (healthy, details) = match &self.inner {
            GrpcConsumerInner::Server(s) => (
                s.server_is_running(),
                serde_json::json!({ "mode": "server", "bound_addr": self.bound_addr }),
            ),
            GrpcConsumerInner::Client(_) => (true, serde_json::json!({ "mode": "client" })),
            GrpcConsumerInner::Dynamic(_) => (
                true,
                serde_json::json!({
                    "mode": "dynamic-client",
                    "acknowledgement_guarantee": "none"
                }),
            ),
        };
        crate::traits::EndpointStatus {
            healthy,
            target: self.url.clone(),
            error: if healthy {
                None
            } else {
                Some("gRPC server task stopped".to_string())
            },
            details,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(super) struct ClientModeConsumer {
    client: BridgeClient<Channel>,
    stream: tonic::Streaming<BridgeMessage>,
    consumer_id: String,
    /// Drain mode: only then does an idle first-message read time out into an empty batch.
    exit_on_empty: bool,
}

impl ClientModeConsumer {
    pub(super) async fn new(config: &GrpcConfig, url: &str) -> Result<Self> {
        debug!(grpc_url = %url, "Creating gRPC client consumer (client mode)");
        reject_unsupported_call_metadata(config, "Bridge client")?;
        let endpoint = make_endpoint(config, url).await?;
        let channel = endpoint.connect().await?;
        let mut client = configured_client(config, channel);
        let topic = config
            .topic
            .clone()
            .unwrap_or_else(|| "default".to_string());
        debug!(grpc_url = %config.url, subscribe_topic = %topic, "gRPC client consumer subscribing to topic");
        // A fresh id per consumer, not the topic: competing consumers on one topic would
        // otherwise share a pending set, so the first ack would remove the entry and every
        // other consumer's ack for the same message would be rejected as unknown. Set
        // `consumer_id` explicitly to keep redelivery across reconnects.
        let consumer_id = config
            .consumer_id
            .clone()
            .unwrap_or_else(|| fast_uuid_v7::gen_id().to_string());
        let request = Request::new(SubscribeRequest {
            topic: topic.clone(),
            consumer_id: consumer_id.clone(),
        });
        let request_timeout = config
            .request_timeout_ms
            .or(config.timeout_ms)
            .map(Duration::from_millis);
        let stream = if let Some(timeout) = request_timeout {
            tokio::time::timeout(timeout, client.subscribe(request))
                .await
                .map_err(|_| anyhow::anyhow!("gRPC subscribe timed out"))??
        } else {
            client.subscribe(request).await?
        }
        .into_inner();
        info!(grpc_url = %url, "gRPC client consumer connected and subscription started");
        Ok(Self {
            client,
            stream,
            consumer_id,
            exit_on_empty: false,
        })
    }
}

#[async_trait]
impl MessageConsumer for ClientModeConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<crate::outcomes::ReceivedBatch, ConsumerError> {
        receive_from_stream(
            &mut self.stream,
            self.client.clone(),
            self.consumer_id.clone(),
            max_messages,
            self.exit_on_empty,
        )
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Reads a batch from a tonic server-streaming response.
/// Blocks on the first message; polls briefly for subsequent ones to fill the batch.
async fn receive_from_stream(
    stream: &mut tonic::Streaming<BridgeMessage>,
    client: BridgeClient<Channel>,
    consumer_id: String,
    max_messages: usize,
    exit_on_empty: bool,
) -> Result<crate::outcomes::ReceivedBatch, ConsumerError> {
    let max_messages = max_messages.max(1);
    let mut messages = Vec::with_capacity(max_messages);
    let mut message_ids = Vec::with_capacity(max_messages);
    loop {
        let result = if messages.is_empty() {
            // Drain mode: a brief idle timeout on the first message yields an empty batch.
            match crate::traits::drain_gated(exit_on_empty, stream.message()).await {
                Some(r) => Ok(r),
                None => return Ok(crate::outcomes::ReceivedBatch::empty()),
            }
        } else {
            tokio::time::timeout(Duration::from_millis(GRPC_BATCH_POLL_MS), stream.message()).await
        };
        match result {
            Ok(Ok(Some(msg))) => {
                message_ids.push(msg.id.clone());
                messages.push(bridge_to_canonical(msg));
                if messages.len() >= max_messages {
                    break;
                }
            }
            Ok(Ok(None)) => {
                trace!("gRPC stream closed by server (None)");
                break;
            }
            Err(_) => {
                trace!("gRPC stream poll timed out while filling batch (normal exit)");
                break;
            }
            Ok(Err(e)) => {
                error!("gRPC stream returned error while receiving: {:?}", e);
                return Err(ConsumerError::Connection(e.into()));
            }
        }
    }
    if messages.is_empty() {
        Err(ConsumerError::EndOfStream)
    } else {
        let commit = grpc_client_commit(client, consumer_id, message_ids);
        Ok(crate::outcomes::ReceivedBatch { messages, commit })
    }
}

fn grpc_client_commit(
    client: BridgeClient<Channel>,
    consumer_id: String,
    message_ids: Vec<String>,
) -> crate::traits::BatchCommitFunc {
    Box::new(move |dispositions| {
        Box::pin(async move {
            if dispositions.len() != message_ids.len() {
                anyhow::bail!(
                    "gRPC batch commit length mismatch: dispositions={}, messages={}",
                    dispositions.len(),
                    message_ids.len()
                );
            }
            // Acks are independent, so they go out concurrently. Awaiting them one at a
            // time would cost `batch_size` round trips per commit.
            let client = &client;
            let consumer_id = &consumer_id;
            futures::stream::iter(message_ids.into_iter().zip(dispositions))
                .map(|(id, disposition)| async move {
                    let status = match disposition {
                        MessageDisposition::Ack | MessageDisposition::Reply(_) => {
                            proto::ack::Status::Ack
                        }
                        MessageDisposition::Nack => proto::ack::Status::Nack,
                    };
                    let mut metadata = HashMap::new();
                    metadata.insert("mq_bridge.consumer_id".to_string(), consumer_id.clone());
                    let response = client
                        .clone()
                        .acknowledge(Request::new(proto::Ack {
                            id: id.clone(),
                            status: status as i32,
                            reason: String::new(),
                            metadata,
                        }))
                        .await?
                        .into_inner();
                    if !response.success {
                        warn!(ack_id = %id, error = %response.error, "gRPC acknowledge rejected");
                    }
                    Ok::<(), tonic::Status>(())
                })
                .buffer_unordered(GRPC_ACK_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
            Ok(())
        })
    })
}
