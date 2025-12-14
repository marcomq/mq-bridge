//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use crate::traits::MessagePublisher;
use crate::traits::{BoxFuture, CommitFunc, MessageConsumer};
use crate::CanonicalMessage;
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use tracing::trace;

/// A sink that responds with a static, pre-configured message.
#[derive(Clone)]
pub struct StaticEndpointPublisher {
    content: String,
}

impl StaticEndpointPublisher {
    pub fn new(content: &str) -> anyhow::Result<Self> {
        Ok(Self {
            content: content.to_owned(),
        })
    }
}

#[async_trait]
impl MessagePublisher for StaticEndpointPublisher {
    async fn send(&self, _message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        trace!(response = %self.content, "Sending static response");
        let payload = serde_json::to_vec(&Value::String(self.content.clone()))?;
        Ok(Some(CanonicalMessage::new(payload)))
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(()) // Nothing to flush for a static response
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A source that always produces the same static message.
#[derive(Clone)]
pub struct StaticRequestConsumer {
    content: String,
}

impl StaticRequestConsumer {
    pub fn new(content: &str) -> anyhow::Result<Self> {
        Ok(Self {
            content: content.to_owned(),
        })
    }
}

#[async_trait]
impl MessageConsumer for StaticRequestConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let payload = self.content.as_bytes().to_vec();
        let message = CanonicalMessage::new(payload);
        let commit = Box::new(|_response: Option<CanonicalMessage>| {
            Box::pin(async {}) as BoxFuture<'static, ()>
        });
        Ok((message, commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
