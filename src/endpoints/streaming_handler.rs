use crate::traits::{
    HandlerError, MessageDisposition, MessagePublisher, PublisherError, Sent, SentBatch,
    StreamingHandler, Yielder,
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
        let reply_callback = if message.metadata.contains_key(crate::traits::REPLY_PATH_KEY) {
            crate::traits::ReplyRegistry::get(original_id)
        } else {
            None
        };

        let (tx, mut rx) = mpsc::channel::<crate::traits::MessageDisposition>(32); // Buffer for yielded messages
        let yielder = Yielder::new(
            tx,
            original_id,
            original_correlation_id.clone(),
            reply_callback.clone(),
        );

        let handler_clone = self.handler.clone();
        let inner_publisher_clone = self.inner.clone();

        let handler_task =
            tokio::spawn(async move { handler_clone.handle_stream(message, yielder).await });

        let mut yielded_responses = Vec::new();

        // Drain the receiver until the sender (yielder) is dropped
        while let Some(disposition) = rx.recv().await {
            match disposition {
                crate::traits::MessageDisposition::Reply(msg) => {
                    // Forward to inner publisher immediately to support true streaming
                    match inner_publisher_clone.send(msg).await {
                        Ok(Sent::Ack) => {}
                        Ok(Sent::Response(resp)) => yielded_responses.push(resp),
                        Ok(Sent::Responses(mut resps, commit)) => {
                            yielded_responses.append(&mut resps);
                            if let Some(c) = commit {
                                c(crate::traits::CommitDisposition::All(
                                    MessageDisposition::Ack,
                                ))
                                .await
                                .map_err(|e| {
                                    handler_task.abort();
                                    PublisherError::Retryable(anyhow::anyhow!(
                                        "Failed to commit responses in streaming handler: {}",
                                        e
                                    ))
                                })?;
                            }
                        }
                        Err(e) => {
                            handler_task.abort();
                            return Err(e);
                        }
                    }
                }
                crate::traits::MessageDisposition::ReplyBatch(msgs) => {
                    for msg in msgs {
                        match inner_publisher_clone.send(msg).await {
                            Ok(Sent::Ack) => {}
                            Ok(Sent::Response(resp)) => yielded_responses.push(resp),
                            Ok(Sent::Responses(mut resps, commit)) => {
                                yielded_responses.append(&mut resps);
                                if let Some(c) = commit {
                                    c(crate::traits::CommitDisposition::All(MessageDisposition::Ack))
                                    .await
                                    .map_err(|e| {
                                        handler_task.abort();
                                        PublisherError::Retryable(anyhow::anyhow!("Failed to commit batch responses in streaming handler: {}", e))
                                    })?;
                                }
                            }
                            Err(e) => {
                                handler_task.abort();
                                return Err(e);
                            }
                        }
                    }
                }
                _ => {
                    debug!("StreamingHandlerPublisher received non-Reply disposition from yielder. Ignoring.");
                }
            }
        }

        // Now that the stream is drained, await the handler result
        handler_task.await.map_err(|e| {
            HandlerError::NonRetryable(anyhow::anyhow!("Handler task panicked: {}", e))
        })??;

        if yielded_responses.is_empty() {
            Ok(Sent::Ack)
        } else {
            Ok(Sent::Responses(yielded_responses, None))
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // For send_batch, we iterate and call send for each message.
        // This ensures each message gets its own Yielder context.
        crate::traits::send_batch_helper(
            self,
            messages,
            MessageDisposition::Ack,
            |publisher, message| Box::pin(publisher.send(message)),
        )
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
    use crate::traits::HandlerError;
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
        ) -> Result<(), HandlerError> {
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

            yielder
                .send(CanonicalMessage::from(format!(
                    "final: {}",
                    original_payload
                )))
                .await
                .map_err(|e| HandlerError::NonRetryable(e))?;
            Ok(())
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

    #[tokio::test]
    async fn test_streaming_handler_mixed_mode() {
        use crate::models::Endpoint;
        use crate::route::Route;

        let in_topic = format!("mixed_in_{}", fast_uuid_v7::gen_id_str());
        let out_topic = format!("mixed_out_{}", fast_uuid_v7::gen_id_str());

        let ep_in = Endpoint::new_memory(&in_topic, 10);
        let ep_out = Endpoint::new_memory(&out_topic, 10);

        let handler = |msg: CanonicalMessage, yielder: Yielder| async move {
            let n = msg.get_payload_str().parse::<usize>().unwrap_or(0);
            for i in 0..n {
                yielder
                    .send(CanonicalMessage::from(format!("yield_{}", i)))
                    .await
                    .unwrap();
            }
            Ok(())
        };

        let route = Route::new(ep_in.clone(), ep_out.clone()).with_streaming_handler(handler);
        route.deploy("mixed_test").await.unwrap();

        let chan_in = ep_in.channel().unwrap();
        let chan_out = ep_out.channel().unwrap();
        chan_in.send_message("3".into()).await.unwrap();

        let mut received: Vec<CanonicalMessage> = Vec::new();
        for _ in 0..20 {
            received.extend(chan_out.drain_messages());
            if received.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert_eq!(received.len(), 3);
        assert_eq!(received[0].get_payload_str(), "yield_0");
        assert_eq!(received[1].get_payload_str(), "yield_1");
        assert_eq!(received[2].get_payload_str(), "yield_2");
        Route::stop("mixed_test").await;
    }
}
