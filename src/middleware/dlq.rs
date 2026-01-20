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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use crate::middleware::retry::RetryPublisher;
    use crate::models::RetryMiddleware;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct MockFailingPublisher {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl MessagePublisher for MockFailingPublisher {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            *self.calls.lock().unwrap() += 1;
            Err(PublisherError::Retryable(anyhow::anyhow!("Always fails")))
        }

        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Clone)]
    struct MockSuccessPublisher {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl MessagePublisher for MockSuccessPublisher {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            *self.calls.lock().unwrap() += 1;
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_retry_before_dlq() {
        let target_calls = Arc::new(Mutex::new(0));
        let failing_target = MockFailingPublisher {
            calls: target_calls.clone(),
        };

        // Retry wrapper: max_attempts 4 means it tries 4 times total
        let retry_config = RetryMiddleware {
            max_attempts: 4,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };
        let retry_publisher = RetryPublisher::new(
            Box::new(failing_target),
            retry_config,
        );

        let dlq_calls = Arc::new(Mutex::new(0));
        let dlq_target = MockSuccessPublisher {
            calls: dlq_calls.clone(),
        };

        // DLQ wrapper: wraps the retry publisher
        let dlq_middleware = DlqPublisher {
            inner: Box::new(retry_publisher),
            dlq_publisher: Arc::new(dlq_target),
        };

        let msg = CanonicalMessage::new(b"test".to_vec(), None);

        // Execute
        let result = dlq_middleware.send(msg).await;

        // Assertions
        assert!(result.is_ok(), "DLQ should handle the failure");
        assert_eq!(
            *target_calls.lock().unwrap(),
            4,
            "Target should be called 4 times (max_attempts)"
        );
        assert_eq!(
            *dlq_calls.lock().unwrap(),
            1,
            "DLQ should be called exactly once after retries fail"
        );
    }

    #[tokio::test]
    async fn test_dlq_integration_with_memory() {
        use crate::endpoints::memory::MemoryPublisher;

        // 1. Setup DLQ destination (Memory)
        let dlq_topic = "dlq_topic";
        let dlq_publisher = MemoryPublisher::new_local(dlq_topic, 10);
        let dlq_channel = dlq_publisher.channel();

        // 2. Setup Failing Primary (Mock)
        let target_calls = Arc::new(Mutex::new(0));
        let failing_target = MockFailingPublisher {
            calls: target_calls.clone(),
        };

        // 3. Setup Retry (max_attempts = 3)
        let retry_config = RetryMiddleware {
            max_attempts: 3,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };
        let retry_publisher = RetryPublisher::new(
            Box::new(failing_target),
            retry_config,
        );

        // 4. Setup DLQ Middleware
        let dlq_middleware = DlqPublisher {
            inner: Box::new(retry_publisher),
            dlq_publisher: Arc::new(dlq_publisher),
        };

        let msg_payload = b"failed_message";
        let msg = CanonicalMessage::new(msg_payload.to_vec(), None);

        // 5. Send
        let result = dlq_middleware.send(msg).await;

        // 6. Verify
        assert!(result.is_ok(), "Send should succeed (handled by DLQ)");

        // Check retries happened
        assert_eq!(*target_calls.lock().unwrap(), 3); // max_attempts

        // Check message is in DLQ memory channel
        let dlq_msgs = dlq_channel.drain_messages();
        assert_eq!(dlq_msgs.len(), 1);
        assert_eq!(dlq_msgs[0].payload, msg_payload.as_slice());
    }
}
