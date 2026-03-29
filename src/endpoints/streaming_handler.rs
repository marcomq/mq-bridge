use crate::traits::{
    Handled, HandlerError, MessagePublisher, PublisherError, Sent, SentBatch, StreamingHandler,
    Yielder,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

/// A `MessagePublisher` that wraps a `StreamingHandler`.
///
/// It allows a handler to yield multiple responses for a single input message,
/// which are then sent to an `inner` publisher.
pub struct StreamingHandlerPublisher {
    handler: Arc<dyn StreamingHandler>,
    inner: Arc<dyn MessagePublisher>, // The actual downstream publisher
}

impl StreamingHandlerPublisher {
    pub fn new(handler: Arc<dyn StreamingHandler>, inner: Arc<dyn MessagePublisher>) -> Self {
        Self { handler, inner }
    }
}

#[async_trait]
impl MessagePublisher for StreamingHandlerPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let original_id = message.message_id;
        let original_correlation_id = message.metadata.get("correlation_id").cloned();

        let (tx, mut rx) = mpsc::channel::<crate::traits::MessageDisposition>(32); // Buffer for yielded messages
        let yielder = Yielder::new(tx, original_id, original_correlation_id.clone());

        let handler_clone = self.handler.clone();
        let inner_publisher_clone = self.inner.clone();

        let handler_task =
            tokio::spawn(async move { handler_clone.handle_stream(message, yielder).await });

        let mut yielded_responses = Vec::new();

        // Drain the receiver until the sender (yielder) is dropped
        while let Some(disposition) = rx.recv().await {
            if let crate::traits::MessageDisposition::Reply(msg) = disposition {
                yielded_responses.push(msg);
            } else {
                debug!("StreamingHandlerPublisher received non-Reply disposition from yielder. Ignoring.");
            }
        }

        // Send all collected responses to the inner publisher
        if !yielded_responses.is_empty() {
            match inner_publisher_clone.send_batch(yielded_responses).await {
                Ok(SentBatch::Ack) => {}
                Ok(SentBatch::Partial {
                    responses: _,
                    failed,
                }) => {
                    // If the inner publisher returns partial, we need to handle it.
                    // For now, simplify: if any failed, return first error.
                    if let Some((_, e)) = failed.into_iter().next() {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // Now that the stream is drained, await the handler result
        let final_handled_result = handler_task.await.map_err(|e| {
            HandlerError::NonRetryable(anyhow::anyhow!("Handler task panicked: {}", e))
        })?;

        // Process final Handled result
        match final_handled_result {
            Ok(Handled::Publish(mut msg)) => {
                // Ensure final message also has correlation
                msg.metadata
                    .entry("correlation_id".to_string())
                    .or_insert_with(|| {
                        original_correlation_id
                            .clone()
                            .unwrap_or_else(|| format!("{:032x}", original_id))
                    });
                // Send the final message from Handled::Publish to the inner publisher
                inner_publisher_clone.send(msg).await
            }
            Ok(Handled::Ack) => Ok(Sent::Ack),
            Err(e) => Err(e), // Convert HandlerError to PublisherError
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // For send_batch, we iterate and call send for each message.
        // This ensures each message gets its own Yielder context.
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;
    use crate::traits::{Handled, HandlerError};
    use crate::CanonicalMessage;
    use async_trait::async_trait;
    use std::time::Duration;

    // A simple StreamingHandler that yields two messages and returns a final one.
    struct MyTestStreamingHandler;

    #[async_trait]
    impl StreamingHandler for MyTestStreamingHandler {
        async fn handle_stream(
            &self,
            msg: CanonicalMessage,
            yielder: Yielder,
        ) -> Result<Handled, HandlerError> {
            let original_payload = msg.get_payload_str();

            // Yield first message
            yielder
                .send(CanonicalMessage::from(format!(
                    "yielded_1: {}",
                    original_payload
                )))
                .await
                .map_err(|e| HandlerError::NonRetryable(e))?;
            tokio::time::sleep(Duration::from_millis(10)).await; // Simulate some work

            // Yield second message
            yielder
                .send(CanonicalMessage::from(format!(
                    "yielded_2: {}",
                    original_payload
                )))
                .await
                .map_err(|e| HandlerError::NonRetryable(e))?;
            tokio::time::sleep(Duration::from_millis(10)).await; // Simulate more work

            // Return a final message
            Ok(Handled::Publish(CanonicalMessage::from(format!(
                "final: {}",
                original_payload
            ))))
        }
    }

    #[tokio::test]
    async fn test_streaming_handler_publisher_multiple_yields() {
        let input_message = CanonicalMessage::from("initial_request");
        let output_topic = "streaming_output_topic";

        // The inner publisher where all yielded and final messages will land
        let inner_publisher = Arc::new(MemoryPublisher::new_local(output_topic, 10));
        let output_channel = inner_publisher.channel();

        // The StreamingHandlerPublisher wraps our test handler and the inner publisher
        let streaming_publisher = StreamingHandlerPublisher::new(
            Arc::new(MyTestStreamingHandler),
            inner_publisher.clone(),
        );

        streaming_publisher
            .send(input_message.clone())
            .await
            .unwrap();

        // Verify messages in the output channel
        let received_messages = output_channel.drain_messages();
        assert_eq!(received_messages.len(), 3);
        assert_eq!(
            received_messages[0].get_payload_str(),
            "yielded_1: initial_request"
        );
        assert_eq!(
            received_messages[1].get_payload_str(),
            "yielded_2: initial_request"
        );
        assert_eq!(
            received_messages[2].get_payload_str(),
            "final: initial_request"
        );
    }
}
