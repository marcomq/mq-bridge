//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::create_publisher_from_route;
use crate::models::DeadLetterQueueMiddleware;
use crate::traits::{MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

pub struct DlqPublisher {
    inner: Box<dyn MessagePublisher>,
    dlq_publisher: Arc<dyn MessagePublisher>,
    config: DeadLetterQueueMiddleware,
}

impl DlqPublisher {
    pub async fn new(
        inner: Box<dyn MessagePublisher>,
        config: &DeadLetterQueueMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        info!(
            "DLQ Middleware enabled for route '{}' with {} retry attempts",
            route_name, config.dlq_retry_attempts
        );
        // Box::pin is used here to break the recursive async type definition.
        // create_publisher_from_route -> apply_middlewares -> DlqPublisher::new -> create_publisher_from_route
        let dlq_publisher =
            Box::pin(create_publisher_from_route(route_name, &config.endpoint)).await?;
        Ok(Self {
            inner,
            dlq_publisher,
            config: config.clone(),
        })
    }

    fn next_backoff(&self, current: u64) -> u64 {
        let next = (current as f64 * self.config.dlq_multiplier) as u64;
        std::cmp::min(next, self.config.dlq_max_interval_ms)
    }

    /// Attempt to send a message to the DLQ with configurable retries and exponential backoff.
    /// Returns the primary send error if DLQ retries fail, ensuring the caller can retry the primary route.
    async fn send_to_dlq_with_retry(
        &self,
        message: CanonicalMessage,
        primary_error: &str,
    ) -> anyhow::Result<()> {
        let mut attempt = 0;
        let mut backoff_ms = self.config.dlq_initial_interval_ms;

        loop {
            attempt += 1;
            match self.dlq_publisher.send(message.clone()).await {
                Ok(_) => {
                    debug!("Message successfully sent to DLQ on attempt {}", attempt);
                    return Ok(());
                }
                Err(e) if attempt < self.config.dlq_retry_attempts => {
                    warn!(
                        "DLQ send failed on attempt {} of {}: {}. Retrying in {}ms...",
                        attempt, self.config.dlq_retry_attempts, e, backoff_ms
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = self.next_backoff(backoff_ms);
                }
                Err(dlq_error) => {
                    // Final retry exhausted; log comprehensively and return original error
                    error!(
                        "DLQ send failed after {} attempts: {}. Original primary send error: {}",
                        attempt, dlq_error, primary_error
                    );
                    // Return the original primary error so the caller can retry the route
                    return Err(anyhow::anyhow!(
                        "Primary send failed: {}. DLQ send also failed after {} retries: {}",
                        primary_error,
                        self.config.dlq_retry_attempts,
                        dlq_error
                    ));
                }
            }
        }
    }
}

#[async_trait]
impl MessagePublisher for DlqPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.inner.send(message.clone()).await {
            Ok(response) => Ok(response),
            Err(e) => {
                let error_msg = e.to_string();
                error!("Failed to send message: {}", error_msg);

                // Attempt to send the message to the DLQ with retry/backoff logic.
                match self.send_to_dlq_with_retry(message, &error_msg).await {
                    Ok(()) => {
                        // Message successfully sent to DLQ. We return Ok(None) to signal that
                        // the message was "handled" (by the DLQ) and should be committed upstream.
                        Ok(Sent::Ack)
                    }
                    Err(dlq_combined_error) => {
                        // DLQ send failed; propagate the combined error that includes both contexts
                        Err(dlq_combined_error.into())
                    }
                }
            }
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        match self.inner.send_batch(messages.clone()).await {
            Ok(SentBatch::Ack) => Ok(SentBatch::Ack),
            Ok(SentBatch::Partial { responses, failed }) if failed.is_empty() => {
                Ok(SentBatch::Partial { responses, failed })
            }
            Ok(SentBatch::Partial { responses, failed }) => {
                let error_msg = format!("{} messages failed to send", failed.len());
                error!(
                    "Failed to send a batch of {} messages. Attempting to send to DLQ.",
                    failed.len()
                );

                // --- Retry logic for bulk DLQ send ---
                let mut attempt = 0;
                let mut backoff_ms = self.config.dlq_initial_interval_ms;
                let mut messages_to_retry: Vec<CanonicalMessage> =
                    failed.iter().map(|(msg, _)| msg.clone()).collect();

                loop {
                    attempt += 1;
                    match self
                        .dlq_publisher
                        .send_batch(messages_to_retry.clone())
                        .await
                    {
                        Ok(outcome) => {
                            let dlq_failed = match outcome {
                                SentBatch::Ack => Vec::new(),
                                SentBatch::Partial { failed, .. } => failed,
                            };

                            if dlq_failed.is_empty() {
                                debug!(
                                    "Batch of {} messages successfully sent to DLQ on attempt {}.",
                                    failed.len(),
                                    attempt
                                );
                                return Ok(SentBatch::Partial {
                                    responses,
                                    failed: Vec::new(),
                                });
                            }

                            if attempt < self.config.dlq_retry_attempts {
                                warn!(
                                    "DLQ bulk send partially failed on attempt {} of {}: {} of {} messages failed. Retrying in {}ms...",
                                    attempt, self.config.dlq_retry_attempts, dlq_failed.len(), messages_to_retry.len(), backoff_ms
                                );
                                messages_to_retry =
                                    dlq_failed.into_iter().map(|(msg, _)| msg).collect();
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms = self.next_backoff(backoff_ms);
                            } else {
                                error!("DLQ bulk send failed after {} attempts. {} messages could not be sent to DLQ. Original primary send error: {}", attempt, dlq_failed.len(), error_msg);
                                return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries, with {} messages remaining.", error_msg, self.config.dlq_retry_attempts, dlq_failed.len()).into());
                            }
                        }
                        Err(e) if attempt < self.config.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send failed on attempt {} of {}: {}. Retrying in {}ms...",
                                attempt, self.config.dlq_retry_attempts, e, backoff_ms
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = self.next_backoff(backoff_ms);
                        }
                        Err(dlq_error) => {
                            error!("DLQ bulk send failed after {} attempts: {}. Original primary send error: {}", attempt, dlq_error, error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries: {}", error_msg, self.config.dlq_retry_attempts, dlq_error).into());
                        }
                    }
                }
            }
            Err(e @ PublisherError::NonRetryable(_)) => {
                let error_msg = e.to_string();
                error!(
                    "Failed to send a batch of {} messages (complete failure). Attempting to send all to DLQ.",
                    messages.len()
                );

                // Attempt to send all messages to the DLQ with retry logic
                let mut attempt = 0;
                let mut backoff_ms = self.config.dlq_initial_interval_ms;
                let mut messages_to_retry = messages.clone();

                loop {
                    attempt += 1;
                    match self
                        .dlq_publisher
                        .send_batch(messages_to_retry.clone())
                        .await
                    {
                        Ok(SentBatch::Ack) => {
                            debug!("Batch of {} messages successfully sent to DLQ on attempt {} after complete primary failure.", messages.len(), attempt);
                            return Ok(SentBatch::Ack);
                        }
                        Ok(SentBatch::Partial {
                            failed: dlq_failed, ..
                        }) if attempt < self.config.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send partially failed on attempt {} of {}: {} of {} messages failed. Retrying in {}ms...",
                                attempt, self.config.dlq_retry_attempts, dlq_failed.len(), messages_to_retry.len(), backoff_ms
                            );
                            messages_to_retry =
                                dlq_failed.into_iter().map(|(msg, _)| msg).collect();
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = self.next_backoff(backoff_ms);
                        }
                        Err(dlq_error) if attempt < self.config.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send failed on attempt {} of {}: {}. Retrying in {}ms...",
                                attempt, self.config.dlq_retry_attempts, dlq_error, backoff_ms
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = self.next_backoff(backoff_ms);
                        }
                        Err(dlq_error) => {
                            error!("DLQ bulk send failed after {} attempts: {}. Original primary send error: {}", attempt, dlq_error, error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries: {}", error_msg, self.config.dlq_retry_attempts, dlq_error).into());
                        }
                        Ok(SentBatch::Partial {
                            failed: dlq_failed, ..
                        }) => {
                            error!("DLQ bulk send failed after {} attempts. {} messages could not be sent to DLQ. Original primary send error: {}", attempt, dlq_failed.len(), error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries, with {} messages remaining.", error_msg, self.config.dlq_retry_attempts, dlq_failed.len()).into());
                        }
                    }
                }
            }
            Err(e @ PublisherError::Retryable(_)) => Err(e), // Propagate retryable errors
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
