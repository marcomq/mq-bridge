//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::errors::PublisherError;
use crate::traits::{Handler, HandlerError, MessagePublisher};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;

use crate::traits::{Sent, SentBatch};

/// A publisher middleware that intercepts messages and passes them to a `Handler`.
/// This middleware is terminal; it consumes the message and does not pass it to an inner publisher.
pub struct EventPublisher {
    handler: Arc<dyn Handler>,
}

impl EventPublisher {
    pub fn new(handler: impl Handler + 'static) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }
}

#[async_trait]
impl MessagePublisher for EventPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.handler.handle(message).await {
            Ok(_) => Ok(Sent::Ack), // Ignore result (Ack or Publish), just Ack.
            Err(e) => Err(e),       // Converts HandlerError to PublisherError
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let results = self.handler.handle_many(messages.clone()).await;
        if results.len() != messages.len() {
            return Err(PublisherError::NonRetryable(anyhow::anyhow!(
                "handler returned {} results for {} messages",
                results.len(),
                messages.len()
            )));
        }

        let mut failed = Vec::new();
        let mut iter = messages.into_iter().zip(results);
        while let Some((message, result)) = iter.next() {
            match result {
                Ok(_) => {}
                Err(HandlerError::NonRetryable(err)) => {
                    failed.push((message, PublisherError::NonRetryable(err)));
                }
                Err(HandlerError::Retryable(err)) => {
                    failed.push((message, PublisherError::Retryable(err)));
                    for (remaining, _) in iter {
                        failed.push((
                            remaining,
                            PublisherError::Retryable(anyhow::anyhow!(
                                "Batch aborted due to previous error"
                            )),
                        ));
                    }
                    break;
                }
                Err(HandlerError::Connection(err)) => {
                    failed.push((message, PublisherError::Connection(err)));
                    for (remaining, _) in iter {
                        failed.push((
                            remaining,
                            PublisherError::Connection(anyhow::anyhow!(
                                "Batch aborted due to previous connection error"
                            )),
                        ));
                    }
                    break;
                }
            }
        }

        if failed.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed,
            })
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Handled;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn test_event_handler() {
        let event_handled = Arc::new(AtomicBool::new(false));
        let handler = Arc::new({
            let flag = event_handled.clone();
            move |_msg: CanonicalMessage| {
                let flag_clone = flag.clone();
                async move {
                    flag_clone.store(true, Ordering::SeqCst);
                    Ok(Handled::Ack)
                }
            }
        });
        let publisher = EventPublisher::new(handler);
        publisher
            .send(CanonicalMessage::new(b"event1".to_vec(), None))
            .await
            .unwrap();
        assert!(event_handled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_event_handler_send_batch_retryable_error_aborts_remainder() {
        struct BatchHandler;

        #[async_trait]
        impl Handler for BatchHandler {
            async fn handle(&self, _msg: CanonicalMessage) -> Result<Handled, HandlerError> {
                unreachable!("send_batch should use handle_many")
            }

            async fn handle_many(
                &self,
                msgs: Vec<CanonicalMessage>,
            ) -> Vec<Result<Handled, HandlerError>> {
                msgs.into_iter()
                    .map(|msg| {
                        if msg.get_payload_str() == "two" {
                            Err(HandlerError::Retryable(anyhow::anyhow!(
                                "temporary failure"
                            )))
                        } else {
                            Ok(Handled::Ack)
                        }
                    })
                    .collect()
            }
        }

        let publisher = EventPublisher::new(BatchHandler);
        let result = publisher
            .send_batch(vec!["one".into(), "two".into(), "three".into()])
            .await
            .unwrap();

        match result {
            SentBatch::Partial { responses, failed } => {
                assert!(responses.is_none());
                assert_eq!(failed.len(), 2);
                assert_eq!(failed[0].0.get_payload_str(), "two");
                assert_eq!(failed[1].0.get_payload_str(), "three");
                assert!(matches!(failed[0].1, PublisherError::Retryable(_)));
                assert!(matches!(failed[1].1, PublisherError::Retryable(_)));
            }
            other => panic!("expected partial failure, got {other:?}"),
        }
    }
}
