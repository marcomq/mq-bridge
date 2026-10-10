use crate::models::RetryMiddleware;
use crate::traits::{BoxFuture, MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use std::any::Any;
use std::time::Duration;
use tracing::warn;

/// Metadata key carrying the 1-based delivery attempt, present only on **re**-deliveries
/// (attempt ≥ 2). Everything downstream of `retry` — including a handler, which `retry`
/// re-invokes once per attempt when it wraps one — is otherwise unable to tell a retry
/// from a first delivery, which makes per-message counting scale with `max_attempts`.
pub const RETRY_ATTEMPT_KEY: &str = "mq_bridge.retry.attempt";

pub struct RetryPublisher {
    inner: Box<dyn MessagePublisher>,
    config: RetryMiddleware,
}

/// Stamp the attempt number on a redelivery. Attempt 1 clears the key rather than
/// leaving it, so "no key" reliably means "first try" even for a message that arrived
/// already carrying one.
fn tag_attempt(message: &mut CanonicalMessage, attempt: usize) {
    if attempt > 1 {
        message
            .metadata
            .insert(RETRY_ATTEMPT_KEY.to_string(), attempt.to_string());
    } else {
        message.metadata.remove(RETRY_ATTEMPT_KEY);
    }
}

impl RetryPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: RetryMiddleware) -> Self {
        Self { inner, config }
    }

    /// `operation` receives the 1-based attempt number so it can mark redeliveries.
    async fn retry_op<F, Fut, T>(&self, operation: F) -> Result<T, PublisherError>
    where
        F: Fn(usize) -> Fut,
        Fut: std::future::Future<Output = Result<T, PublisherError>>,
    {
        let mut attempt = 0;
        let mut interval = self.config.initial_interval_ms;

        loop {
            attempt += 1;
            match operation(attempt).await {
                Ok(val) => return Ok(val),
                Err(e @ PublisherError::NonRetryable(_)) => return Err(e), // Don't retry non-retryable errors
                Err(e @ PublisherError::Connection(_)) => return Err(e), // Propagate connection errors
                Err(e @ PublisherError::Retryable(_)) => {
                    if attempt >= self.config.max_attempts {
                        return Err(PublisherError::Retryable(anyhow!(
                            "Retries exhausted after {} attempts: {}",
                            self.config.max_attempts,
                            e
                        )));
                    }
                    warn!(
                        "Operation failed (attempt {}/{}): {}. Retrying in {}ms...",
                        attempt, self.config.max_attempts, e, interval
                    );
                    self.sleep_and_backoff(&mut interval).await;
                }
            }
        }
    }

    async fn sleep_and_backoff(&self, interval: &mut u64) {
        tokio::time::sleep(Duration::from_millis(*interval)).await;
        *interval = (*interval as f64 * self.config.multiplier) as u64;
        if *interval > self.config.max_interval_ms {
            *interval = self.config.max_interval_ms;
        }
    }
}

#[async_trait]
impl MessagePublisher for RetryPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.retry_op(|attempt| {
            let mut msg = message.clone();
            tag_attempt(&mut msg, attempt);
            async { self.inner.send(msg).await }
        })
        .await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut current_messages = messages;
        let mut all_responses = Vec::new();
        let mut all_failed = Vec::new();
        // Set once an attempt delivered part of the batch: a later whole-batch error must
        // then fail only the remainder instead of the messages already sent.
        let mut partial_progress = false;

        // We reuse the retry_op logic manually here because the state (current_messages) changes
        let mut attempt = 0;
        let mut interval = self.config.initial_interval_ms;

        loop {
            attempt += 1;
            // `send_batch` consumes the batch and `Err` does not hand it back, so a copy is
            // kept to re-send from. Nothing is re-sent after the final attempt, so that one
            // moves instead of deep-copying every metadata map (unless a failure of it
            // still has to name the remaining messages).
            let mut outgoing = if attempt >= self.config.max_attempts && !partial_progress {
                std::mem::take(&mut current_messages)
            } else {
                current_messages.clone()
            };
            for msg in &mut outgoing {
                tag_attempt(msg, attempt);
            }
            match self.inner.send_batch(outgoing).await {
                Ok(SentBatch::Ack) => {
                    return if all_responses.is_empty() && all_failed.is_empty() {
                        Ok(SentBatch::Ack)
                    } else {
                        Ok(SentBatch::Partial {
                            responses: if all_responses.is_empty() {
                                None
                            } else {
                                Some(all_responses)
                            },
                            failed: all_failed,
                        })
                    };
                }
                Ok(SentBatch::Partial { responses, failed }) => {
                    if let Some(resps) = responses {
                        all_responses.extend(resps);
                    }

                    let (retryable, non_retryable): (Vec<_>, Vec<_>) = failed
                        .into_iter()
                        .partition(|(_, e)| matches!(e, PublisherError::Retryable(_)));

                    all_failed.extend(non_retryable);

                    if retryable.is_empty() {
                        return Ok(SentBatch::Partial {
                            responses: if all_responses.is_empty() {
                                None
                            } else {
                                Some(all_responses)
                            },
                            failed: all_failed,
                        });
                    }
                    if attempt >= self.config.max_attempts {
                        // After exhausting retries, convert all remaining retryable errors to non-retryable
                        // so the DLQ middleware will handle them.
                        let non_retryable_failures = retryable.into_iter().map(|(msg, e)| {
                            (
                                msg,
                                PublisherError::Retryable(anyhow!("Retries exhausted: {}", e)),
                            )
                        });
                        all_failed.extend(non_retryable_failures);
                        return Ok(SentBatch::Partial {
                            responses: if all_responses.is_empty() {
                                None
                            } else {
                                Some(all_responses)
                            },
                            failed: all_failed,
                        });
                    }
                    warn!("Batch send partially failed (attempt {}/{}): {} messages failed. Retrying...", attempt, self.config.max_attempts, retryable.len());
                    current_messages = retryable.into_iter().map(|(msg, _)| msg).collect();
                    partial_progress = true;
                }
                Err(e) => {
                    let exhausted = attempt >= self.config.max_attempts;
                    if partial_progress && (exhausted || !matches!(e, PublisherError::Retryable(_)))
                    {
                        let reason = e.to_string();
                        let max_attempts = self.config.max_attempts;
                        all_failed.extend(current_messages.into_iter().map(|msg| {
                            let error = match &e {
                                PublisherError::NonRetryable(_) => {
                                    PublisherError::NonRetryable(anyhow!("{reason}"))
                                }
                                PublisherError::Connection(_) => {
                                    PublisherError::Connection(anyhow!("{reason}"))
                                }
                                PublisherError::Retryable(_) => PublisherError::Retryable(anyhow!(
                                    "Retries exhausted after {max_attempts} attempts: {reason}"
                                )),
                            };
                            (msg, error)
                        }));
                        return Ok(SentBatch::Partial {
                            responses: if all_responses.is_empty() {
                                None
                            } else {
                                Some(all_responses)
                            },
                            failed: all_failed,
                        });
                    }
                    if matches!(e, PublisherError::NonRetryable(_)) {
                        return Err(e);
                    }
                    // Connection errors are treated as non-retryable and may be reported as part of a Partial result (failed_messages),
                    // not always as Err. The retry logic will not retry them whether they arrive via Err or Partial.
                    if matches!(e, PublisherError::Connection(_)) {
                        return Err(e);
                    }
                    if attempt >= self.config.max_attempts {
                        return Err(PublisherError::Retryable(anyhow!(
                            "Retries exhausted after {} attempts: {}",
                            self.config.max_attempts,
                            e
                        )));
                    }
                    warn!(
                        "Batch send failed (attempt {}/{}): {}. Retrying...",
                        attempt, self.config.max_attempts, e
                    );
                }
            }
            self.sleep_and_backoff(&mut interval).await;
        }
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::MessagePublisher;
    use crate::CanonicalMessage;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use std::any::Any;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct MockPublisher {
        attempts: Arc<Mutex<usize>>,
        succeed_after: usize,
    }

    #[async_trait]
    impl MessagePublisher for MockPublisher {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            let mut attempts = self.attempts.lock().unwrap();
            *attempts += 1;
            if *attempts > self.succeed_after {
                Ok(Sent::Ack)
            } else {
                Err(anyhow!("Simulated error").into())
            }
        }

        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut attempts = self.attempts.lock().unwrap();
            *attempts += 1;
            if *attempts > self.succeed_after {
                Ok(SentBatch::Ack)
            } else {
                Err(anyhow!("Simulated batch error").into())
            }
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Delivers the first message of the first batch, then fails every later call outright.
    struct PartialThenDown {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl MessagePublisher for PartialThenDown {
        async fn send(&self, _msg: CanonicalMessage) -> Result<Sent, PublisherError> {
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls > 1 {
                return Err(PublisherError::Retryable(anyhow!("broker blip")));
            }
            Ok(SentBatch::Partial {
                responses: None,
                failed: messages
                    .into_iter()
                    .skip(1)
                    .map(|m| (m, PublisherError::Retryable(anyhow!("busy"))))
                    .collect(),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// A whole-batch error on a later attempt must not fail the messages an earlier
    /// attempt already delivered, or the route redelivers them.
    #[tokio::test]
    async fn test_batch_error_after_partial_keeps_delivered_messages() {
        let config = RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };
        let publisher = RetryPublisher::new(
            Box::new(PartialThenDown {
                calls: Arc::new(Mutex::new(0)),
            }),
            config,
        );
        let batch: Vec<_> = (1..=3u128)
            .map(|id| CanonicalMessage::new(vec![], Some(id)))
            .collect();

        let SentBatch::Partial { failed, .. } = publisher.send_batch(batch).await.unwrap() else {
            panic!("expected a partial result");
        };
        let failed_ids: Vec<u128> = failed.iter().map(|(m, _)| m.message_id).collect();
        assert_eq!(failed_ids, vec![2, 3]);
        assert!(failed
            .iter()
            .all(|(_, e)| e.to_string().contains("Retries exhausted")));
    }

    #[tokio::test]
    async fn test_retry_success() {
        let attempts = Arc::new(Mutex::new(0));
        let mock = MockPublisher {
            attempts: attempts.clone(),
            succeed_after: 2, // Fails 2 times, succeeds on 3rd
        };

        let config = RetryMiddleware {
            max_attempts: 5,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };

        let retry_publisher = RetryPublisher::new(Box::new(mock), config);
        let msg = CanonicalMessage::new(vec![], None);

        let result = retry_publisher.send(msg).await;
        assert!(result.is_ok());
        assert_eq!(*attempts.lock().unwrap(), 3);
    }

    /// A redelivery must be distinguishable from a first delivery, so anything downstream
    /// (a handler, or a caller counting messages) does not count one message N times.
    #[tokio::test]
    async fn redeliveries_carry_an_attempt_marker() {
        #[derive(Clone)]
        struct RecordingPublisher {
            seen: Arc<Mutex<Vec<Option<String>>>>,
            succeed_after: usize,
        }

        #[async_trait]
        impl MessagePublisher for RecordingPublisher {
            async fn send(&self, msg: CanonicalMessage) -> Result<Sent, PublisherError> {
                let mut seen = self.seen.lock().unwrap();
                seen.push(msg.metadata.get(RETRY_ATTEMPT_KEY).cloned());
                if seen.len() > self.succeed_after {
                    Ok(Sent::Ack)
                } else {
                    Err(anyhow!("Simulated error").into())
                }
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

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mock = RecordingPublisher {
            seen: seen.clone(),
            succeed_after: 2,
        };
        let config = RetryMiddleware {
            max_attempts: 5,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };

        let retry_publisher = RetryPublisher::new(Box::new(mock), config);
        retry_publisher
            .send(CanonicalMessage::new(vec![], None))
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            vec![None, Some("2".to_string()), Some("3".to_string())],
            "attempt 1 must be unmarked; every redelivery must carry its attempt number"
        );
    }

    #[tokio::test]
    async fn test_retry_exhaustion() {
        let attempts = Arc::new(Mutex::new(0));
        let mock = MockPublisher {
            attempts: attempts.clone(),
            succeed_after: 10,
        };

        let config = RetryMiddleware {
            max_attempts: 3,
            initial_interval_ms: 1,
            max_interval_ms: 10,
            multiplier: 1.0,
        };

        let retry_publisher = RetryPublisher::new(Box::new(mock), config);
        let msg = CanonicalMessage::new(vec![], None);

        let result = retry_publisher.send(msg).await;
        assert!(result.is_err());
        assert_eq!(*attempts.lock().unwrap(), 3);
    }
}
