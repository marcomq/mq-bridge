use crate::models::LimiterMiddleware;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

const MAX_DELAY: Duration = Duration::from_secs(3600);

#[derive(Debug)]
struct RateState {
    next_allowed_at: Instant,
}

impl RateState {
    fn new() -> Self {
        Self {
            next_allowed_at: Instant::now(),
        }
    }

    /// Returns how long to wait before releasing `count` messages. The wait covers the whole
    /// window those messages occupy, not just the queue ahead of them: releasing a batch the
    /// moment its slot opens let a single `send_batch` of N pass instantly, however large N
    /// was, and only charged the next batch for it.
    fn reserve(&mut self, count: usize, per_message: Duration) -> Duration {
        if count == 0 {
            return Duration::ZERO;
        }

        let now = Instant::now();
        let start_at = self.next_allowed_at.max(now);
        let additional = Duration::try_from_secs_f64(per_message.as_secs_f64() * count as f64)
            .map_or(MAX_DELAY, |d| d.min(MAX_DELAY));
        self.next_allowed_at = start_at
            .checked_add(additional)
            .unwrap_or_else(|| start_at + MAX_DELAY);
        self.next_allowed_at.saturating_duration_since(now)
    }
}

/// Clamped so a tiny rate cannot overflow `Duration` and panic.
fn per_message(messages_per_second: f64) -> Duration {
    Duration::try_from_secs_f64(1.0 / messages_per_second).map_or(MAX_DELAY, |d| d.min(MAX_DELAY))
}

pub struct LimiterConsumer {
    inner: Box<dyn MessageConsumer>,
    per_message: Duration,
    state: RateState,
}

impl LimiterConsumer {
    pub fn new(
        inner: Box<dyn MessageConsumer>,
        config: &LimiterMiddleware,
    ) -> anyhow::Result<Self> {
        if !(config.messages_per_second.is_finite() && config.messages_per_second > 0.0) {
            return Err(anyhow::anyhow!(
                "Limiter messages_per_second must be a finite value greater than zero"
            ));
        }
        Ok(Self {
            inner,
            per_message: per_message(config.messages_per_second),
            state: RateState::new(),
        })
    }

    async fn wait_for(&mut self, count: usize) {
        let delay = self.state.reserve(count, self.per_message);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

#[async_trait]
impl MessageConsumer for LimiterConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let received = self.inner.receive().await?;
        self.wait_for(1).await;
        Ok(received)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let batch = self.inner.receive_batch(max_messages).await?;
        self.wait_for(batch.messages.len()).await;
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct LimiterPublisher {
    inner: Box<dyn MessagePublisher>,
    per_message: Duration,
    state: Mutex<RateState>,
}

impl LimiterPublisher {
    pub fn new(
        inner: Box<dyn MessagePublisher>,
        config: &LimiterMiddleware,
    ) -> anyhow::Result<Self> {
        if !(config.messages_per_second.is_finite() && config.messages_per_second > 0.0) {
            return Err(anyhow::anyhow!(
                "Limiter messages_per_second must be a finite value greater than zero"
            ));
        }
        Ok(Self {
            inner,
            per_message: per_message(config.messages_per_second),
            state: Mutex::new(RateState::new()),
        })
    }

    async fn wait_for(&self, count: usize) {
        let delay = self
            .state
            .lock()
            .expect("Limiter mutex poisoned")
            .reserve(count, self.per_message);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

#[async_trait]
impl MessagePublisher for LimiterPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.wait_for(1).await;
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        self.wait_for(messages.len()).await;
        self.inner.send_batch(messages).await
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
    use crate::traits::MessagePublisher;
    use crate::CanonicalMessage;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    struct MockConsumer {
        batches: VecDeque<Vec<CanonicalMessage>>,
    }

    #[async_trait]
    impl MessageConsumer for MockConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            Ok(ReceivedBatch {
                messages: self.batches.pop_front().expect("batch already consumed"),
                commit: ack_commit(),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Clone)]
    struct MockPublisher {
        sent: Arc<StdMutex<Vec<CanonicalMessage>>>,
    }

    #[async_trait]
    impl MessagePublisher for MockPublisher {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            self.sent.lock().unwrap().push(message);
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.sent.lock().unwrap().extend(messages);
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn ack_commit() -> crate::traits::BatchCommitFunc {
        Box::new(|_| Box::pin(async { Ok(()) }))
    }

    #[tokio::test]
    async fn test_limiter_consumer_delays_batch_by_message_count() {
        let config = LimiterMiddleware {
            messages_per_second: 20.0,
        };
        let mut consumer = LimiterConsumer::new(
            Box::new(MockConsumer {
                batches: VecDeque::from([
                    vec![CanonicalMessage::from("one"), CanonicalMessage::from("two")],
                    vec![
                        CanonicalMessage::from("three"),
                        CanonicalMessage::from("four"),
                    ],
                ]),
            }),
            &config,
        )
        .unwrap();

        let first = consumer.receive_batch(10).await.unwrap();
        let start = Instant::now();
        let second = consumer.receive_batch(10).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(first.messages.len(), 2);
        assert_eq!(second.messages.len(), 2);
        assert!(elapsed >= Duration::from_millis(90));
    }

    #[tokio::test]
    async fn test_limiter_publisher_delays_consecutive_sends() {
        let config = LimiterMiddleware {
            messages_per_second: 20.0,
        };
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let publisher =
            LimiterPublisher::new(Box::new(MockPublisher { sent: sent.clone() }), &config).unwrap();

        let start = Instant::now();
        publisher.send(CanonicalMessage::from("one")).await.unwrap();
        publisher.send(CanonicalMessage::from("two")).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(sent.lock().unwrap().len(), 2);
        assert!(elapsed >= Duration::from_millis(45));
    }

    /// A single `send_batch` must be paced by its own message count. It used to pass straight
    /// through and only charge the *next* call, so a route that read everything in one poll
    /// ignored the limit entirely.
    #[tokio::test]
    async fn test_limiter_publisher_paces_a_single_large_batch() {
        let config = LimiterMiddleware {
            messages_per_second: 20.0,
        };
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let publisher =
            LimiterPublisher::new(Box::new(MockPublisher { sent: sent.clone() }), &config).unwrap();

        let messages: Vec<CanonicalMessage> = (0..10)
            .map(|i| CanonicalMessage::from(&*i.to_string()))
            .collect();
        let start = Instant::now();
        publisher.send_batch(messages).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(sent.lock().unwrap().len(), 10);
        assert!(
            elapsed >= Duration::from_millis(450),
            "10 messages at 20/s should take ~500ms, took {elapsed:?}"
        );
    }
}
