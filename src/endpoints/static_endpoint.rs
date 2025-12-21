//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::traits::MessagePublisher;
use crate::traits::{into_batch_commit_func, BatchCommitFunc};
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
    pub fn new(config: &str) -> anyhow::Result<Self> {
        Ok(Self {
            content: config.to_string(),
        })
    }
}

#[async_trait]
impl MessagePublisher for StaticEndpointPublisher {
    async fn send(&self, _message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        trace!(response = %self.content, "Sending static response");
        let payload = serde_json::to_vec(&Value::String(self.content.clone()))?;
        Ok(Some(CanonicalMessage::new(payload, None)))
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
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
    pub fn new(config: &str) -> anyhow::Result<Self> {
        Ok(Self {
            content: config.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for StaticRequestConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let message = CanonicalMessage::new(self.content.as_bytes().to_vec(), None);
        let commit = Box::new(|_response: Option<CanonicalMessage>| {
            Box::pin(async {}) as BoxFuture<'static, ()>
        });
        Ok((message, commit))
    }

    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let (msg, commit) = self.receive().await?;
        let commit = into_batch_commit_func(commit);
        Ok((vec![msg], commit))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use serde_json::Value;

    #[tokio::test]
    async fn test_static_publisher() {
        let content = "static_response";
        let publisher = StaticEndpointPublisher::new(content).unwrap();
        let msg = CanonicalMessage::new(vec![], None);

        let response = publisher.send(msg).await.unwrap();
        assert!(response.is_some());
        let response_msg = response.unwrap();
        // The publisher serializes the content as a JSON string
        let expected_payload = serde_json::to_vec(&Value::String(content.to_string())).unwrap();
        assert_eq!(response_msg.payload, expected_payload);
    }

    #[tokio::test]
    async fn test_static_consumer() {
        let content = "static_message";
        let mut consumer = StaticRequestConsumer::new(content).unwrap();

        let (msg, _commit) = consumer.receive().await.unwrap();
        assert_eq!(msg.payload, content.as_bytes());
    }

    #[test]
    fn test_static_config_yaml() {
        use crate::models::{Config, EndpointType};

        let yaml = r#"
test_route:
  input:
    static: "static_input_value"
  output:
    static: "static_output_value"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
        let route = config.get("test_route").expect("Route should exist");

        if let EndpointType::Static(val) = &route.input.endpoint_type {
            assert_eq!(val, "static_input_value");
        } else {
            panic!("Input was not static");
        }

        if let EndpointType::Static(val) = &route.output.endpoint_type {
            assert_eq!(val, "static_output_value");
        } else {
            panic!("Output was not static");
        }
    }
}
