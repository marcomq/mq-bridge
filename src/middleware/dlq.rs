//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use crate::endpoints::create_publisher_from_route;
use crate::models::DeadLetterQueueMiddleware;
use crate::traits::MessagePublisher;
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

pub struct DlqPublisher {
    inner: Box<dyn MessagePublisher>,
    dlq_publisher: Arc<dyn MessagePublisher>,
    dlq_retry_attempts: usize,
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
            dlq_retry_attempts: config.dlq_retry_attempts,
        })
    }

    /// Attempt to send a message to the DLQ with configurable retries and exponential backoff.
    /// Returns the primary send error if DLQ retries fail, ensuring the caller can retry the primary route.
    async fn send_to_dlq_with_retry(
        &self,
        message: CanonicalMessage,
        primary_error: &str,
    ) -> anyhow::Result<()> {
        let mut attempt = 0;
        let mut backoff_ms = 100u64;

        loop {
            attempt += 1;
            match self.dlq_publisher.send(message.clone()).await {
                Ok(_) => {
                    info!("Message successfully sent to DLQ on attempt {}", attempt);
                    return Ok(());
                }
                Err(e) if attempt < self.dlq_retry_attempts => {
                    warn!(
                        "DLQ send failed on attempt {} of {}: {}. Retrying in {}ms...",
                        attempt, self.dlq_retry_attempts, e, backoff_ms
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5000); // Cap backoff at 5s
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
                        self.dlq_retry_attempts,
                        dlq_error
                    ));
                }
            }
        }
    }
}

#[async_trait]
impl MessagePublisher for DlqPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        match self.inner.send(message.clone()).await {
            Ok(response) => Ok(response),
            Err(e) => {
                let error_msg = e.to_string();
                error!("Failed to send message: {}", error_msg);

                // Attempt to send the message to the DLQ with retry/backoff logic.
                match self.send_to_dlq_with_retry(message, &error_msg).await {
                    Ok(()) => {
                        // Message successfully sent to DLQ; return original error to signal route failure
                        Err(e)
                    }
                    Err(dlq_combined_error) => {
                        // DLQ send failed; propagate the combined error that includes both contexts
                        Err(dlq_combined_error)
                    }
                }
            }
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        match self.inner.send_batch(messages.clone()).await {
            Ok((responses, failed)) if failed.is_empty() => Ok((responses, failed)),
            Ok((responses, failed)) => {
                let error_msg = format!("{} messages failed to send", failed.len());
                error!(
                    "Failed to send a batch of {} messages. Attempting to send to DLQ.",
                    failed.len()
                );

                // --- Retry logic for bulk DLQ send ---
                let mut attempt = 0;
                let mut backoff_ms = 100u64;
                let mut messages_to_retry = failed.clone();

                loop {
                    attempt += 1;
                    match self
                        .dlq_publisher
                        .send_batch(messages_to_retry.clone())
                        .await
                    {
                        Ok((_, dlq_failed)) if dlq_failed.is_empty() => {
                            info!(
                                "Batch of {} messages successfully sent to DLQ on attempt {}.",
                                failed.len(),
                                attempt
                            );
                            // Return the successful responses and the original failed messages to the caller.
                            return Ok((responses, failed));
                        }
                        Ok((_, dlq_failed)) if attempt < self.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send partially failed on attempt {} of {}: {} of {} messages failed. Retrying in {}ms...",
                                attempt, self.dlq_retry_attempts, dlq_failed.len(), messages_to_retry.len(), backoff_ms
                            );
                            messages_to_retry = dlq_failed; // Only retry the ones that failed this attempt.
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(5000); // Cap backoff at 5s
                        }
                        Err(e) if attempt < self.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send failed on attempt {} of {}: {}. Retrying in {}ms...",
                                attempt, self.dlq_retry_attempts, e, backoff_ms
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(5000); // Cap backoff at 5s
                        }
                        Err(dlq_error) => {
                            error!("DLQ bulk send failed after {} attempts: {}. Original primary send error: {}", attempt, dlq_error, error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries: {}", error_msg, self.dlq_retry_attempts, dlq_error));
                        }
                        Ok((_, dlq_failed)) => {
                            error!("DLQ bulk send failed after {} attempts. {} messages could not be sent to DLQ. Original primary send error: {}", attempt, dlq_failed.len(), error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries, with {} messages remaining.", error_msg, self.dlq_retry_attempts, dlq_failed.len()));
                        }
                    }
                }
            }
            Err(e) => {
                let error_msg = e.to_string();
                error!(
                    "Failed to send a batch of {} messages (complete failure). Attempting to send all to DLQ.",
                    messages.len()
                );

                // Attempt to send all messages to the DLQ with retry logic
                let mut attempt = 0;
                let mut backoff_ms = 100u64;
                let mut messages_to_retry = messages.clone();

                loop {
                    attempt += 1;
                    match self
                        .dlq_publisher
                        .send_batch(messages_to_retry.clone())
                        .await
                    {
                        Ok((_, dlq_failed)) if dlq_failed.is_empty() => {
                            info!("Batch of {} messages successfully sent to DLQ on attempt {} after complete primary failure.", messages.len(), attempt);
                            return Ok((None, messages));
                        }
                        Ok((_, dlq_failed)) if attempt < self.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send partially failed on attempt {} of {}: {} of {} messages failed. Retrying in {}ms...",
                                attempt, self.dlq_retry_attempts, dlq_failed.len(), messages_to_retry.len(), backoff_ms
                            );
                            messages_to_retry = dlq_failed;
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(5000);
                        }
                        Err(dlq_error) if attempt < self.dlq_retry_attempts => {
                            warn!(
                                "DLQ bulk send failed on attempt {} of {}: {}. Retrying in {}ms...",
                                attempt, self.dlq_retry_attempts, dlq_error, backoff_ms
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(5000);
                        }
                        Err(dlq_error) => {
                            error!("DLQ bulk send failed after {} attempts: {}. Original primary send error: {}", attempt, dlq_error, error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries: {}", error_msg, self.dlq_retry_attempts, dlq_error));
                        }
                        Ok((_, dlq_failed)) => {
                            error!("DLQ bulk send failed after {} attempts. {} messages could not be sent to DLQ. Original primary send error: {}", attempt, dlq_failed.len(), error_msg);
                            return Err(anyhow::anyhow!("Primary send failed: {}. DLQ bulk send also failed after {} retries, with {} messages remaining.", error_msg, self.dlq_retry_attempts, dlq_failed.len()));
                        }
                    }
                }
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
