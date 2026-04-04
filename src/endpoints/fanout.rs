use crate::traits::{EndpointStatus, MessagePublisher, PublisherError, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;

pub struct FanoutPublisher {
    publishers: Vec<Arc<dyn MessagePublisher>>,
}

impl FanoutPublisher {
    /// Creates a new `FanoutPublisher`.
    ///
    /// Messages sent to this publisher will be cloned and sent to each of the provided
    /// `MessagePublisher` instances.
    pub fn new(publishers: Vec<Arc<dyn MessagePublisher>>) -> Self {
        Self { publishers }
    }
}

#[async_trait]
impl MessagePublisher for FanoutPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>, // This `messages` vector represents a single logical batch from the caller.
                                         // Each element in this vector will be sent to *each* sub-publisher.
    ) -> Result<SentBatch, PublisherError> {
        use futures::future::join_all;

        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        let mut all_responses = Vec::new();
        let mut all_failed = Vec::new();

        // Collect futures for sending the cloned batch to all publishers concurrently.
        let futures: Vec<_> = self
            .publishers
            .iter()
            .map(|p| p.send_batch(messages.clone())) // Clone the entire batch for each sub-publisher
            .collect();

        // Await all sends concurrently.
        let results = join_all(futures).await;

        for result in results {
            match result {
                Ok(SentBatch::Ack) => {
                    // This sub-publisher successfully processed the batch without returning specific responses.
                }
                Ok(SentBatch::Partial {
                    responses, failed, ..
                }) => {
                    if let Some(resps) = responses {
                        all_responses.extend(resps);
                    }
                    all_failed.extend(failed);
                }
                Err(e) => {
                    // If any sub-publisher returns a hard error, we propagate it immediately.
                    // This means the entire fan-out operation failed.
                    return Err(e);
                }
            }
        }

        if all_failed.is_empty() && all_responses.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: if all_responses.is_empty() {
                    None
                } else {
                    Some(all_responses)
                },
                failed: all_failed,
                commit: None,
            })
        }
    }

    async fn status(&self) -> EndpointStatus {
        use futures::future::join_all;

        let status_futs = self.publishers.iter().map(|p| p.status());
        let results = join_all(status_futs).await;

        let mut healthy = true;
        let mut pending = 0;
        let mut capacity = 0;
        let mut error: Option<String> = None;
        let mut details = Vec::new();

        for status in results {
            if !status.healthy {
                healthy = false;
                if error.is_none() {
                    error = status.error.clone();
                }
            }
            pending += status.pending.unwrap_or(0);
            capacity += status.capacity.unwrap_or(0);
            details.push(status);
        }

        EndpointStatus {
            healthy,
            pending: Some(pending),
            capacity: Some(capacity),
            error,
            details: serde_json::json!({ "destinations": details }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
