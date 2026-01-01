// src/endpoints/response.rs
use crate::traits::{MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;

/// A publisher that simply returns the message it receives as a response.
/// This is useful when used with a Handler to reflect the handler's output
/// back to the source (e.g., for HTTP request-response).
#[derive(Clone)]
pub struct ResponsePublisher;

#[async_trait]
impl MessagePublisher for ResponsePublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        Ok(Sent::Response(message))
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Return all messages as responses
        Ok(SentBatch::Partial {
            responses: Some(messages),
            failed: Vec::new(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_handler::CommandPublisher;
    use crate::traits::Handled;

    #[tokio::test]
    async fn test_response_publisher_echo() {
        let publisher = ResponsePublisher;
        let msg = CanonicalMessage::new(b"test".to_vec(), None);
        let result = publisher.send(msg).await.unwrap();

        match result {
            Sent::Response(r) => assert_eq!(&r.payload[..], b"test"),
            _ => panic!("Expected Sent::Response"),
        }
    }

    #[tokio::test]
    async fn test_response_publisher_with_handler() {
        // This simulates the full flow: Handler -> CommandPublisher -> ResponsePublisher -> Route
        let response_pub = ResponsePublisher;

        // A handler that modifies the message and returns it
        let handler = |mut msg: CanonicalMessage| async move {
            msg.payload = [b"handled: ", &msg.payload[..]].concat().into();
            Ok(Handled::Publish(msg))
        };

        let publisher = CommandPublisher::new(response_pub, handler);

        let msg = CanonicalMessage::new(b"input".to_vec(), None);
        let result = publisher.send(msg).await.unwrap();

        match result {
            Sent::Response(r) => assert_eq!(&r.payload[..], b"handled: input"),
            _ => panic!("Expected Sent::Response"),
        }
    }
}
