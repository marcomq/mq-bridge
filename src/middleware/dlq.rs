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
use tracing::{debug, error, info};

pub struct DlqPublisher {
    inner: Box<dyn MessagePublisher>,
    dlq_publisher: Arc<dyn MessagePublisher>,
}

impl DlqPublisher {
    pub async fn new(
        inner: Box<dyn MessagePublisher>,
        config: &DeadLetterQueueMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        info!("DLQ Middleware enabled for route '{}'", route_name);
        // Box::pin is used here to break the recursive async type definition.
        // create_publisher -> apply_middlewares -> DlqPublisher::new -> create_publisher
        let dlq_publisher =
            Box::pin(create_publisher_from_route(route_name, &config.endpoint)).await?;
        Ok(Self {
            inner,
            dlq_publisher,
        })
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

                // Attempt to send the message to the DLQ.
                // If this fails, we immediately return the original error.
                // Use a retry middleware if you want to retry sending the message to the DLQ.
                match self.dlq_publisher.send(message).await {
                    Ok(_) => {
                        // Message successfully sent to DLQ. We return Ok(None) to signal that
                        // the message was "handled" (by the DLQ) and should be committed upstream.
                        Ok(Sent::Ack)
                    }
                    Err(dlq_combined_error) => {
                        // DLQ send failed; propagate the combined error that includes both contexts
                        Err(anyhow::anyhow!(
                            "Primary send failed: {}. DLQ send also failed: {}",
                            error_msg,
                            dlq_combined_error
                        )
                        .into())
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

                let messages_to_retry: Vec<CanonicalMessage> =
                    failed.iter().map(|(msg, _)| msg.clone()).collect();

                match self.dlq_publisher.send_batch(messages_to_retry).await {
                    Ok(SentBatch::Ack) => Ok(SentBatch::Partial {
                        responses,
                        failed: Vec::new(),
                    }),
                    Ok(SentBatch::Partial {
                        failed: dlq_failed, ..
                    }) => {
                        error!(
                            "DLQ bulk send partially failed. {} messages could not be sent to DLQ.",
                            dlq_failed.len()
                        );
                        Ok(SentBatch::Partial {
                            responses,
                            failed: dlq_failed,
                        })
                    }
                    Err(dlq_error) => {
                        error!(
                            "DLQ bulk send failed: {}. Original primary send error: {}",
                            dlq_error, error_msg
                        );
                        Err(anyhow::anyhow!(
                            "Primary send failed: {}. DLQ bulk send also failed: {}",
                            error_msg,
                            dlq_error
                        )
                        .into())
                    }
                }
            }
            Err(e @ PublisherError::NonRetryable(_)) => {
                let error_msg = e.to_string();
                error!(
                    "Failed to send a batch of {} messages (complete failure). Attempting to send all to DLQ.",
                    messages.len()
                );

                // Attempt to send all messages to the DLQ
                match self.dlq_publisher.send_batch(messages).await {
                    Ok(SentBatch::Ack) => {
                        debug!("Batch successfully sent to DLQ after complete primary failure.");
                        Ok(SentBatch::Ack)
                    }
                    Ok(SentBatch::Partial {
                        failed: dlq_failed, ..
                    }) => {
                        error!(
                            "DLQ bulk send partially failed. {} messages could not be sent to DLQ.",
                            dlq_failed.len()
                        );
                        Ok(SentBatch::Partial {
                            responses: None,
                            failed: dlq_failed,
                        })
                    }
                    Err(dlq_error) => {
                        error!(
                            "DLQ bulk send failed: {}. Original primary send error: {}",
                            dlq_error, error_msg
                        );
                        Err(anyhow::anyhow!(
                            "Primary send failed: {}. DLQ bulk send also failed: {}",
                            error_msg,
                            dlq_error
                        )
                        .into())
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
