//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Built-in Bridge publisher for a remote `mq-bridge` gRPC server.

use super::dynamic::reject_unsupported_call_metadata;
use super::proto;
use super::{bridge_to_canonical, canonical_to_bridge, configured_client, make_endpoint};
use crate::models::GrpcConfig;
use crate::traits::{MessagePublisher, PublisherError, SentBatch};
use crate::CanonicalMessage;
use anyhow::Result;
use async_trait::async_trait;
use proto::bridge_client::BridgeClient;
use proto::BridgeMessage;
use std::any::Any;
use std::time::Duration;
use tonic::transport::Channel;
use tracing::error;

use super::dynamic::DynamicPublisher;

/// Builds the gRPC output for a route: a descriptor-driven call when a descriptor source
/// is configured, otherwise the built-in Bridge publisher.
pub async fn create_grpc_publisher(config: &GrpcConfig) -> Result<Box<dyn MessagePublisher>> {
    if config.descriptor_set_bytes.is_some()
        || config.descriptor_set_path.is_some()
        || config.reflection
    {
        let url = config.tls.normalize_url(&config.url);
        Ok(Box::new(DynamicPublisher::new(config, &url).await?))
    } else {
        Ok(Box::new(GrpcPublisher::new(config).await?))
    }
}

// ── Publisher ─────────────────────────────────────────────────────────────────

pub struct GrpcPublisher {
    client: BridgeClient<Channel>,
    // Retains the shared registry entry so concurrent publishers reuse this channel.
    _shared_channel: std::sync::Arc<Channel>,
    url: String,
    request_timeout: Option<Duration>,
    overall_timeout: Option<Duration>,
    topic: Option<String>,
}

impl GrpcPublisher {
    pub async fn new(config: &GrpcConfig) -> Result<Self> {
        reject_unsupported_call_metadata(config, "Bridge publisher")?;
        // Use a lazy channel so the publisher route can start before a server-mode
        // gRPC consumer has finished binding its embedded listener.
        let url = config.tls.normalize_url(&config.url);
        // Share one channel across publishers with the same connection settings; the
        // channel multiplexes and the topic is per-message.
        let identity = crate::support::connection_registry::connection_identity((
            &url,
            config.tls.required,
            &config.tls.ca_file,
            &config.tls.cert_file,
            &config.tls.key_file,
            config.tls.accept_invalid_certs,
            config.connect_timeout_ms.or(config.timeout_ms),
            config.initial_stream_window_size,
            config.initial_connection_window_size,
            config.http2_keepalive_interval_ms,
            config.http2_keepalive_timeout_ms,
        ));
        let config_clone = config.clone();
        let url_for_build = url.clone();
        let shared_channel = crate::support::connection_registry::get_or_create(
            "grpc-channel",
            identity,
            config.shared.unwrap_or(true),
            move || async move {
                let endpoint = make_endpoint(&config_clone, &url_for_build).await?;
                Ok(endpoint.connect_lazy())
            },
        )
        .await?;
        let client = configured_client(config, (*shared_channel).clone());
        Ok(Self {
            client,
            _shared_channel: shared_channel,
            url,
            request_timeout: config
                .request_timeout_ms
                .or(config.timeout_ms)
                .map(Duration::from_millis),
            overall_timeout: config.overall_timeout_ms.map(Duration::from_millis),
            topic: Some(
                config
                    .topic
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
        })
    }

    /// Opens the response stream, bounding only call setup. `overall_timeout` bounds the
    /// response-handling phase separately in `send_batch`.
    async fn publish_batch_stream(
        &self,
        messages: Vec<BridgeMessage>,
    ) -> Result<tonic::Streaming<proto::PublishResponse>, PublisherError> {
        let mut client = self.client.clone();
        let call = client.publish_batch(tokio_stream::iter(messages));
        let response = match self.request_timeout {
            Some(timeout) => tokio::time::timeout(timeout, call).await.map_err(|_| {
                PublisherError::Retryable(anyhow::anyhow!("gRPC publish request timed out"))
            })?,
            None => call.await,
        }
        .map_err(|status| {
            PublisherError::Retryable(anyhow::anyhow!("gRPC publish_batch error: {status:?}"))
        })?;
        Ok(response.into_inner())
    }
}

#[async_trait]
impl MessagePublisher for GrpcPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Preserve the original messages so we can map response ids back to originals.
        let original_messages = messages;
        let bridge_messages_vec: Vec<BridgeMessage> = original_messages
            .iter()
            .cloned()
            .map(|msg| canonical_to_bridge(msg, self.topic.as_deref()))
            .collect();

        // Process responses and enforce an overall timeout if configured.
        let mut id_map: std::collections::HashMap<String, Vec<CanonicalMessage>> =
            std::collections::HashMap::new();
        for msg in &original_messages {
            let id_str = fast_uuid_v7::format_uuid(msg.message_id).to_string();
            id_map.entry(id_str).or_default().push(msg.clone());
        }
        let total_messages = original_messages.len();

        let process_fut = async {
            let mut stream = self.publish_batch_stream(bridge_messages_vec).await?;
            let mut responses = Vec::new();
            let mut failed: Vec<(CanonicalMessage, PublisherError)> = Vec::new();
            let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

            loop {
                match stream.message().await {
                    Ok(Some(r)) => match r.result {
                        Some(proto::publish_response::Result::Ack(ack)) => {
                            seen_ids.insert(ack.id.clone());
                            if ack.status != 0 {
                                if let Some(origs) = id_map.get(&ack.id) {
                                    for orig in origs {
                                        failed.push((
                                            orig.clone(),
                                            PublisherError::Retryable(anyhow::anyhow!(ack
                                                .reason
                                                .clone())),
                                        ));
                                    }
                                } else {
                                    return Err(PublisherError::Retryable(anyhow::anyhow!(ack
                                        .reason
                                        .clone())));
                                }
                            }
                        }
                        Some(proto::publish_response::Result::Reply(reply)) => {
                            seen_ids.insert(reply.id.clone());
                            responses.push(bridge_to_canonical(reply));
                        }
                        Some(proto::publish_response::Result::Error(err)) => {
                            // Treat explicit error responses as a retryable batch-level failure.
                            return Err(PublisherError::Retryable(anyhow::anyhow!(err)));
                        }
                        None => {}
                    },
                    Ok(None) => break,
                    Err(e) => {
                        error!("Error reading publish batch response stream: {:?}", e);
                        return Err(PublisherError::Retryable(anyhow::anyhow!(format!(
                            "gRPC stream error: {:?}",
                            e
                        ))));
                    }
                }
            }

            // Any ids that were not seen are treated as missing responses -> retryable.
            for (id, origs) in &id_map {
                if !seen_ids.contains(id) {
                    for orig in origs {
                        failed.push((
                            orig.clone(),
                            PublisherError::Retryable(anyhow::anyhow!("missing response for id")),
                        ));
                    }
                }
            }

            Ok((responses, failed)) as Result<_, PublisherError>
        };

        let (responses, failed): (
            Vec<crate::CanonicalMessage>,
            Vec<(crate::CanonicalMessage, PublisherError)>,
        ) = if let Some(timeout) = self.overall_timeout {
            tokio::time::timeout(timeout, process_fut)
                .await
                .map_err(|_| {
                    PublisherError::Retryable(anyhow::anyhow!("gRPC publish batch timed out"))
                })??
        } else {
            process_fut.await?
        };

        let total = total_messages;
        if failed.is_empty() && responses.is_empty() {
            Ok(SentBatch::Ack)
        } else if failed.len() == total {
            Err(PublisherError::Retryable(anyhow::anyhow!(
                "All messages in batch failed"
            )))
        } else {
            Ok(SentBatch::Partial {
                responses: if responses.is_empty() {
                    None
                } else {
                    Some(responses)
                },
                failed,
            })
        }
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        crate::traits::EndpointStatus {
            healthy: true,
            target: self.url.clone(),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
