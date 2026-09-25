use crate::models::TimeoutMiddleware;
use crate::traits::{BoxFuture, EndpointStatus, MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::future::Future;
use std::time::Duration;

/// Bounds each send, turning one that never returns into a retryable error.
///
/// Elapsing drops the pending send future, but the sink may already have
/// accepted the batch, so a retry after a timeout can deliver it twice.
pub struct TimeoutPublisher {
    inner: Box<dyn MessagePublisher>,
    timeout: Duration,
}

impl TimeoutPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &TimeoutMiddleware) -> Self {
        Self {
            inner,
            timeout: Duration::from_millis(config.timeout_ms),
        }
    }

    async fn bounded<T>(
        &self,
        send: impl Future<Output = Result<T, PublisherError>>,
    ) -> Result<T, PublisherError> {
        tokio::time::timeout(self.timeout, send)
            .await
            .unwrap_or_else(|_| {
                Err(PublisherError::Retryable(anyhow::anyhow!(
                    "send did not complete within {}ms; it may still land, so a retry can duplicate",
                    self.timeout.as_millis()
                )))
            })
    }
}

#[async_trait]
impl MessagePublisher for TimeoutPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.bounded(self.inner.send(message)).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        self.bounded(self.inner.send_batch(messages)).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
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
    use crate::endpoints::memory::MemoryPublisher;

    struct StuckPublisher;

    #[async_trait]
    impl MessagePublisher for StuckPublisher {
        async fn send_batch(&self, _: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
            std::future::pending().await
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn a_send_that_never_returns_becomes_retryable() {
        let config = TimeoutMiddleware { timeout_ms: 20 };
        let publisher = TimeoutPublisher::new(Box::new(StuckPublisher), &config);

        let error = publisher
            .send_batch(vec![CanonicalMessage::from("stuck")])
            .await
            .unwrap_err();

        assert!(matches!(error, PublisherError::Retryable(_)), "{error}");
    }

    #[tokio::test]
    async fn a_send_within_the_timeout_passes_through() {
        let config = TimeoutMiddleware { timeout_ms: 1000 };
        let inner = MemoryPublisher::new_local("timeout_test_out", 10);
        let publisher = TimeoutPublisher::new(Box::new(inner), &config);

        assert!(publisher.send(CanonicalMessage::from("ok")).await.is_ok());
    }
}
