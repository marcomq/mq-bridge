use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Sent, SentBatch,
    READER_MAX_MESSAGES_KEY,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ReaderPublisher {
    consumer: Arc<Mutex<Box<dyn MessageConsumer>>>,
}

impl ReaderPublisher {
    pub fn new(consumer: Box<dyn MessageConsumer>) -> Self {
        Self {
            consumer: Arc::new(Mutex::new(consumer)),
        }
    }
}

#[async_trait]
impl MessagePublisher for ReaderPublisher {
    async fn send(&self, trigger: CanonicalMessage) -> Result<Sent, PublisherError> {
        let mut consumer = self.consumer.lock().await;
        // We ignore the incoming message payload and just read from the consumer.
        // The incoming message acts purely as a trigger.
        let requested_count = trigger
            .metadata
            .get(READER_MAX_MESSAGES_KEY)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1000);

        match consumer.receive_batch(requested_count).await {
            Ok(batch) => {
                let mut msgs = batch.messages;
                let count = msgs.len();

                if let Some(reply_path) = trigger.metadata.get(crate::traits::REPLY_PATH_KEY) {
                    for msg in msgs.iter_mut() {
                        msg.metadata.insert(
                            crate::traits::REPLY_PATH_KEY.to_string(),
                            reply_path.clone(),
                        );
                    }
                }

                if count == 0 {
                    Ok(Sent::Ack)
                } else {
                    Ok(Sent::Responses(msgs, Some(Arc::new(batch.commit))))
                }
            }
            Err(e) => match e {
                ConsumerError::EndOfStream => Err(PublisherError::NonRetryable(anyhow::anyhow!(e))),
                _ => Err(PublisherError::Retryable(anyhow::anyhow!(e))),
            },
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let count = messages.len();
        if count == 0 {
            return Ok(SentBatch::Ack);
        }

        // Use the first trigger message to find the reply path
        let reply_path = messages[0]
            .metadata
            .get(crate::traits::REPLY_PATH_KEY)
            .cloned();

        let mut consumer = self.consumer.lock().await;
        match consumer.receive_batch(count).await {
            Ok(batch) => {
                let mut msgs = batch.messages;
                if msgs.is_empty() {
                    Ok(SentBatch::Ack)
                } else {
                    if let Some(rp) = reply_path {
                        for msg in msgs.iter_mut() {
                            msg.metadata
                                .insert(crate::traits::REPLY_PATH_KEY.to_string(), rp.clone());
                        }
                    }
                    Ok(SentBatch::Partial {
                        responses: Some(msgs),
                        failed: vec![],
                        commit: Some(Arc::new(batch.commit)),
                    })
                }
            }
            Err(e) => match e {
                ConsumerError::EndOfStream => Err(PublisherError::NonRetryable(anyhow::anyhow!(e))),
                _ => Err(PublisherError::Retryable(anyhow::anyhow!(e))),
            },
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
