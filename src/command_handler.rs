//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::traits::{send_batch_helper, CommandHandler, MessagePublisher};
use crate::traits::{Handled, HandlerError};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::future::Future;
use std::sync::Arc;

use crate::traits::{PublisherError, Sent, SentBatch};
#[async_trait]
impl<F, Fut> CommandHandler for F
where
    F: Fn(CanonicalMessage) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Handled, HandlerError>> + Send,
{
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        self(msg).await
    }
}

/// A publisher middleware that intercepts messages and passes them to a `CommandHandler`.
/// If the handler returns a new message, it is passed to the inner publisher.
pub struct CommandHandlerPublisher {
    inner: Box<dyn MessagePublisher>,
    handler: Arc<dyn CommandHandler>,
}

impl CommandHandlerPublisher {
    pub fn new(inner: impl MessagePublisher, handler: impl CommandHandler + 'static) -> Self {
        Self {
            inner: Box::new(inner),
            handler: Arc::new(handler),
        }
    }
}

#[async_trait]
impl MessagePublisher for CommandHandlerPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        match self.handler.handle(message).await {
            Ok(Handled::Publish(response_msg)) => self.inner.send(response_msg).await, // Propagate result
            Ok(Handled::Ack) => Ok(Sent::Ack),
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

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::endpoints::memory::MemoryPublisher;

    #[tokio::test]
    async fn test_command_handler_produces_response() {
        let memory_publisher = MemoryPublisher::new_local("test_command_out", 10);
        let channel = memory_publisher.channel();

        let handler = |msg: CanonicalMessage| async move {
            let response_payload = format!("response_to_{}", String::from_utf8_lossy(&msg.payload));
            Ok(Handled::Publish(CanonicalMessage::from_str(
                &response_payload,
            )))
        };

        let publisher = CommandHandlerPublisher::new(memory_publisher, handler);

        let msg = CanonicalMessage::from_str("command1");
        publisher.send(msg).await.unwrap();

        let received = channel.drain_messages();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].payload, "response_to_command1".as_bytes());
    }

    #[tokio::test]
    async fn test_command_handler_acks() {
        let memory_publisher = MemoryPublisher::new_local("test_command_out", 10);
        let channel = memory_publisher.channel();

        let handler = |_msg: CanonicalMessage| async move { Ok(Handled::Ack) };

        let publisher = CommandHandlerPublisher::new(memory_publisher, handler);

        let msg = CanonicalMessage::from_str("command1");
        let result = publisher.send(msg).await.unwrap();

        assert!(matches!(result, Sent::Ack));
        let received = channel.drain_messages();
        assert_eq!(received.len(), 0);
    }

    #[tokio::test]
    async fn test_command_handler_retryable_error() {
        let memory_publisher = MemoryPublisher::new_local("test_command_out", 10);

        let handler = |_msg: CanonicalMessage| async move {
            Err(HandlerError::Retryable(anyhow::anyhow!("db is down")))
        };

        let publisher = CommandHandlerPublisher::new(memory_publisher, handler);
        let result = publisher.send("command1".into()).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // The HandlerError is converted into a PublisherError
        assert!(matches!(err, PublisherError::Retryable(_)));
    }

    #[tokio::test]
    async fn test_command_handler_integration_with_memory_consumer() {
        use crate::endpoints::memory::MemoryConsumer;
        use crate::traits::MessageConsumer;

        // 1. Setup Input (MemoryConsumer)
        let mut consumer = MemoryConsumer::new_local("cmd_input", 10);
        let input_channel = consumer.channel();

        // 2. Setup Output (MemoryPublisher wrapped by CommandHandlerPublisher)
        let memory_publisher = MemoryPublisher::new_local("cmd_output", 10);
        let output_channel = memory_publisher.channel();

        // 3. Create Publisher Middleware with inline handler
        let publisher =
            CommandHandlerPublisher::new(memory_publisher, |msg: CanonicalMessage| async move {
                let payload = String::from_utf8_lossy(&msg.payload);
                let response = format!("processed_{}", payload);
                Ok(Handled::Publish(response.into()))
            });

        // 4. Inject message into input
        input_channel
            .send_message("test_data".into())
            .await
            .unwrap();

        // 5. Simulate Bridge Loop (Consume -> Publish)
        let received = consumer.receive().await.unwrap();
        let result = publisher.send(received.message).await.unwrap();

        // 6. Verify
        assert!(matches!(result, Sent::Ack));

        let output_msgs = output_channel.drain_messages();
        assert_eq!(output_msgs.len(), 1);
        assert_eq!(output_msgs[0].payload.to_vec(), b"processed_test_data");

        (received.commit)(None).await;
    }

    #[tokio::test]
    async fn test_command_handler_with_route_config() {
        use crate::models::{Endpoint, Route};

        let success = Arc::new(AtomicBool::new(false));
        let success_clone = success.clone();

        // 1. Define Handler
        let handler = move |mut msg: CanonicalMessage| {
            success_clone.store(true, Ordering::SeqCst);
            msg.set_payload_str(format!("modified {}", msg.get_payload_str()));
            async move { Ok(Handled::Publish(msg)) }
        };
        // 2. Define Route
        let route = Route {
            concurrency: 1,
            input: Endpoint::new_memory("route_in", 100),
            output: Endpoint::new_memory("route_out", 100),
        }
        .with_handler(Arc::new(handler));

        // 3. Inject Data
        let input_channel = route.input.channel().unwrap();
        input_channel
            .send_message(CanonicalMessage::from_str("hello"))
            .await
            .unwrap();

        // 4. Run
        let res = route.run_until_err("test_route", None);
        input_channel.close();
        res.await.ok(); // eof error due to closed channel

        // 5. Verify
        assert!(success.load(Ordering::SeqCst) == true);

        let msgs = route.output.channel().unwrap().drain_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get_payload_str(), "modified hello");
    }
}
