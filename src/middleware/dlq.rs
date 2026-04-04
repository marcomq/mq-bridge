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
                let is_non_retryable = match &e {
                    PublisherError::NonRetryable(_) => true,
                    // If retries are exhausted, we treat it as a non-retryable error for DLQ purposes.
                    PublisherError::Retryable(err) => err.to_string().contains("Retries exhausted"),
                };

                if !is_non_retryable {
                    // It's a transient error that hasn't exhausted retries yet, propagate it.
                    return Err(e);
                }

                // At this point, the error is either NonRetryable or an exhausted Retryable.
                // Both should go to the DLQ.
                let error_msg = e.to_string();
                error!(
                    "Message send failed permanently, sending to DLQ: {}",
                    error_msg
                );
                match self.dlq_publisher.send(message).await {
                    Ok(_) => Ok(Sent::Ack),
                    Err(dlq_error) => {
                        // If the DLQ itself has a connection error, we must propagate it to trigger a route restart.
                        // Otherwise, the message would be lost.
                        if let PublisherError::NonRetryable(err) = &dlq_error {
                            if err.to_string().contains("__CONNECTION_ERROR__") {
                                return Err(dlq_error);
                            }
                        }
                        Err(PublisherError::NonRetryable(anyhow::anyhow!(
                            "Primary send failed: '{}'. DLQ send also failed: {}",
                            error_msg,
                            dlq_error
                        )))
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
            Ok(SentBatch::Partial {
                responses,
                failed,
                commit,
            }) => {
                if failed.is_empty() {
                    return Ok(SentBatch::Partial {
                        responses,
                        failed,
                        commit,
                    });
                }

                let (retryable, mut non_retryable): (Vec<_>, Vec<_>) = failed
                    .into_iter()
                    .partition(|(_, e)| matches!(e, PublisherError::Retryable(_)));

                // Separate exhausted retries from still-retryable ones.
                let (exhausted, still_retryable): (Vec<_>, Vec<_>) = retryable
                    .into_iter()
                    .partition(|(_, e)| e.to_string().contains("Retries exhausted"));

                non_retryable.extend(exhausted);

                if non_retryable.is_empty() {
                    return Ok(SentBatch::Partial {
                        responses,
                        failed: still_retryable,
                        commit,
                    });
                }

                error!(
                    "{} messages failed with non-retryable errors. Sending to DLQ.",
                    non_retryable.len()
                );

                let messages_to_dlq: Vec<CanonicalMessage> =
                    non_retryable.iter().map(|(msg, _)| msg.clone()).collect();

                let final_failed = still_retryable;

                match self.dlq_publisher.send_batch(messages_to_dlq).await {
                    Ok(SentBatch::Ack) => Ok(SentBatch::Partial {
                        responses,
                        failed: final_failed,
                        commit,
                    }),
                    Ok(SentBatch::Partial {
                        failed: dlq_failed,
                        commit: dlq_commit,
                        ..
                    }) => {
                        let mut final_failed = final_failed;
                        error!(
                            "DLQ bulk send partially failed. {} messages could not be sent to DLQ.",
                            dlq_failed.len()
                        );
                        final_failed.extend(dlq_failed);
                        Ok(SentBatch::Partial {
                            responses,
                            failed: final_failed,
                            commit: dlq_commit,
                        })
                    }
                    Err(dlq_error) => {
                        // If the DLQ itself has a connection error, propagate it to restart the route.
                        if let PublisherError::NonRetryable(err) = &dlq_error {
                            if err.to_string().contains("__CONNECTION_ERROR__") {
                                return Err(dlq_error);
                            }
                        }
                        error!(
                            "DLQ send failed: {}. Propagating original errors.",
                            dlq_error
                        );
                        Err(anyhow::anyhow!(
                            "Primary send had non-retryable errors, but sending to DLQ also failed: {}",
                            dlq_error
                        )
                        .into())
                    }
                }
            }
            Err(e) => {
                let is_non_retryable = match &e {
                    PublisherError::NonRetryable(_) => true,
                    // If retries are exhausted, we treat it as a non-retryable error for DLQ purposes.
                    PublisherError::Retryable(err) => err.to_string().contains("Retries exhausted"),
                };

                if !is_non_retryable {
                    // It's a transient error that hasn't exhausted retries yet, propagate it.
                    return Err(e);
                }

                // At this point, the error is either NonRetryable or an exhausted Retryable.
                // Both should go to the DLQ.
                let error_msg = e.to_string();
                error!(
                    "Batch send failed permanently ({} messages). Attempting to send all to DLQ. Error: {}",
                    messages.len(),
                    error_msg
                );

                // We attempt to send the original batch to the DLQ.
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
                            commit: None,
                        })
                    }
                    Err(dlq_error) => {
                        // If the DLQ itself has a connection error, propagate it to restart the route.
                        if let PublisherError::NonRetryable(err) = &dlq_error {
                            if err.to_string().contains("__CONNECTION_ERROR__") {
                                return Err(dlq_error);
                            }
                        }
                        error!(
                            "DLQ send failed: {}. Propagating original error.",
                            dlq_error
                        );
                        // The original error `e` is what caused the DLQ attempt. We wrap it to indicate the DLQ also failed.
                        Err(PublisherError::NonRetryable(anyhow::anyhow!(
                            "Primary send failed: '{}'. DLQ send also failed: {}",
                            e,
                            dlq_error
                        )))
                    }
                }
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::retry::RetryPublisher;
    use crate::models::RetryMiddleware;
    use crate::CanonicalMessage;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct MockNonRetryablePublisher {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl MessagePublisher for MockNonRetryablePublisher {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            *self.calls.lock().unwrap() += 1;
            Err(PublisherError::NonRetryable(anyhow::anyhow!(
                "Always fails non-retryable"
            )))
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
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += _messages.len();
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
        let retry_publisher = RetryPublisher::new(Box::new(failing_target), retry_config);

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
        let retry_publisher = RetryPublisher::new(Box::new(failing_target), retry_config);

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

    #[derive(Clone)]
    struct MockFailingBatchPublisher {
        calls: Arc<Mutex<usize>>,
        fail_on_call: usize,
        partial_fail: bool,
    }

    #[async_trait]
    impl MessagePublisher for MockFailingBatchPublisher {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            unimplemented!()
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == self.fail_on_call {
                if self.partial_fail {
                    // Fail one message in the batch
                    let (head, _) = messages.split_at(1);
                    return Ok(SentBatch::Partial {
                        responses: None,
                        commit: None,
                        failed: vec![(
                            head[0].clone(),
                            PublisherError::NonRetryable(anyhow::anyhow!("Partial batch fail")),
                        )],
                    });
                } else {
                    // Fail the whole batch
                    return Err(PublisherError::NonRetryable(anyhow::anyhow!(
                        "Batch send failed"
                    )));
                }
            }
            // Succeed
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_dlq_send_batch_full_failure() {
        let target_calls = Arc::new(Mutex::new(0));
        // This publisher will fail the first time send_batch is called
        let failing_target = MockFailingBatchPublisher {
            calls: target_calls.clone(),
            fail_on_call: 1,
            partial_fail: false,
        };

        let dlq_calls = Arc::new(Mutex::new(0));
        let dlq_target = MockSuccessPublisher {
            calls: dlq_calls.clone(),
        };

        let dlq_middleware = DlqPublisher {
            inner: Box::new(failing_target),
            dlq_publisher: Arc::new(dlq_target),
        };

        let messages = vec![CanonicalMessage::from("1"), CanonicalMessage::from("2")];

        // Execute
        let result = dlq_middleware.send_batch(messages).await;

        // Assertions
        assert!(result.is_ok(), "DLQ should handle the batch failure");
        assert_eq!(
            *target_calls.lock().unwrap(),
            1,
            "Target should be called once"
        );
        // The successful DLQ publisher's `send` will be called for each message in the failed batch
        assert_eq!(
            *dlq_calls.lock().unwrap(),
            2,
            "DLQ should be called for each message in the failed batch"
        );
    }

    #[tokio::test]
    async fn test_dlq_send_batch_partial_failure() {
        let target_calls = Arc::new(Mutex::new(0));
        let failing_target = MockFailingBatchPublisher {
            calls: target_calls.clone(),
            fail_on_call: 1,
            partial_fail: true,
        };

        let dlq_calls = Arc::new(Mutex::new(0));
        let dlq_target = MockSuccessPublisher {
            calls: dlq_calls.clone(),
        };

        let dlq_middleware = DlqPublisher {
            inner: Box::new(failing_target),
            dlq_publisher: Arc::new(dlq_target),
        };

        let messages = vec![CanonicalMessage::from("1"), CanonicalMessage::from("2")];
        let result = dlq_middleware.send_batch(messages).await;

        assert!(result.is_ok());
        if let Ok(SentBatch::Partial { failed, .. }) = result {
            assert!(
                failed.is_empty(),
                "DLQ should have handled the failed message"
            );
        } else {
            panic!("Expected partial success");
        }

        assert_eq!(*target_calls.lock().unwrap(), 1);
        // Only the one failed message should go to DLQ
        assert_eq!(*dlq_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_dlq_failure_propagates_error() {
        let failing_target = MockNonRetryablePublisher {
            calls: Arc::new(Mutex::new(0)),
        };
        let failing_dlq = MockFailingPublisher {
            calls: Arc::new(Mutex::new(0)),
        };
        let dlq_middleware = DlqPublisher {
            inner: Box::new(failing_target.clone()),
            dlq_publisher: Arc::new(failing_dlq),
        };
        let result = dlq_middleware.send("test".into()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, PublisherError::NonRetryable(_)));
        assert!(err.to_string().contains("DLQ send also failed"));
        assert_eq!(*failing_target.calls.lock().unwrap(), 1);
    }
}
