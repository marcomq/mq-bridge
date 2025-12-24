use crate::traits::{MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;

#[derive(Clone)]
pub struct NullPublisher;

#[async_trait]
impl MessagePublisher for NullPublisher {
    async fn send(&self, _message: CanonicalMessage) -> Result<Sent, PublisherError> {
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
