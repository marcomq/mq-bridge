use crate::models::RandomPanicMiddleware;
use crate::traits::{BatchCommitFunc, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use async_trait::async_trait;
use rand::Rng;
use std::any::Any;

pub struct RandomPanicConsumer {
    inner: Box<dyn MessageConsumer>,
    probability: f64,
}

impl RandomPanicConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &RandomPanicMiddleware) -> Self {
        if !(0.0..=1.0).contains(&config.probability) {
            panic!(
                "RandomPanicMiddleware: probability must be between 0.0 and 1.0, got {}",
                config.probability
            );
        }
        Self {
            inner,
            probability: config.probability,
        }
    }

    fn maybe_panic(&self) {
        if rand::rng().random_bool(self.probability) {
            panic!("RandomPanicMiddleware: Consumer panic triggered!");
        }
    }
}

#[async_trait]
impl MessageConsumer for RandomPanicConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        self.maybe_panic();
        self.inner.receive().await
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        self.maybe_panic();
        self.inner.receive_batch(max_messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct RandomPanicPublisher {
    inner: Box<dyn MessagePublisher>,
    probability: f64,
}

impl RandomPanicPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &RandomPanicMiddleware) -> Self {
        if !(0.0..=1.0).contains(&config.probability) {
            panic!(
                "RandomPanicMiddleware: probability must be between 0.0 and 1.0, got {}",
                config.probability
            );
        }
        Self {
            inner,
            probability: config.probability,
        }
    }

    fn maybe_panic(&self) {
        if rand::rng().random_bool(self.probability) {
            panic!("RandomPanicMiddleware: Publisher panic triggered!");
        }
    }
}

#[async_trait]
impl MessagePublisher for RandomPanicPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        self.maybe_panic();
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        self.maybe_panic();
        self.inner.send_batch(messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
