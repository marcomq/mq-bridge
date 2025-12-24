//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::errors::PublisherError;
use crate::traits::{send_batch_helper, Handler, MessagePublisher};
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
    pub fn new(handler: Arc<dyn Handler>) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl MessagePublisher for EventPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.handler.handle(message).await {
            Ok(_) => Ok(Sent::Ack),  // Ignore result (Ack or Publish), just Ack.
            Err(e) => Err(e.into()), // Converts HandlerError to PublisherError
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
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
}
