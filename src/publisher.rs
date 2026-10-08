use crate::endpoints;
use crate::models;
use crate::traits;
use crate::CanonicalMessage;
use crate::Sent;
use crate::SentBatch;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// A simple wrapper around a publisher to send messages to a specific endpoint.
#[derive(Clone)]
pub struct Publisher {
    publisher: std::sync::Arc<dyn traits::MessagePublisher>,
}

static PUBLISHER_REGISTRY: OnceLock<RwLock<HashMap<String, Publisher>>> = OnceLock::new();

impl Publisher {
    /// Creates a new publisher for the given endpoint configuration.
    pub async fn new(endpoint: models::Endpoint) -> anyhow::Result<Self> {
        let publisher = endpoints::create_publisher_from_route("publisher", &endpoint).await?;
        Ok(Self { publisher })
    }

    /// Creates a publisher from a JSON endpoint configuration.
    ///
    /// Convenience over [`Publisher::new`] for callers that hold a
    /// `serde_json::Value` (e.g. loaded from a config file or built at runtime),
    /// mirroring the `from_config` constructor exposed by the language bindings.
    /// The value is the endpoint body keyed by type, e.g. `{"kafka": { ... }}`.
    pub async fn from_config(config: serde_json::Value) -> anyhow::Result<Self> {
        let endpoint: models::Endpoint = serde_json::from_value(config)?;
        Self::new(endpoint).await
    }

    /// Sends a message and expects a response message from the endpoint.
    /// Returns an error if the endpoint does not support responses (e.g. returns a simple Ack).
    pub async fn request(&self, message: CanonicalMessage) -> anyhow::Result<CanonicalMessage> {
        match self.publisher.send(message).await? {
            Sent::Response(resp) => Ok(resp),
            Sent::Ack => Err(anyhow::anyhow!("Expected a response from the endpoint, but received only an acknowledgment (Ack). Ensure the endpoint and route are correctly configured for request-reply.")),
        }
    }

    /// Sends a batch of messages and expects a response message for each from the endpoint.
    /// Returns an error if any message fails or if the endpoint does not support responses (e.g. returns a simple Ack).
    pub async fn request_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<CanonicalMessage>> {
        let count = messages.len();
        if count == 0 {
            return Ok(Vec::new());
        }
        match self.publisher.send_batch(messages).await? {
            SentBatch::Partial { responses: Some(resps), failed } if failed.is_empty() && resps.len() == count => Ok(resps),
            SentBatch::Ack => Err(anyhow::anyhow!("Expected responses from the endpoint, but received only acknowledgments (Ack). Ensure the endpoint and route are correctly configured for request-reply.")),
            _ => Err(anyhow::anyhow!("Request batch failed to return the expected responses. Ensure the endpoint and route are correctly configured for request-reply.")),
        }
    }

    /// Sends a message to the configured endpoint.
    pub async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Sent> {
        self.publisher
            .send(message)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Sends a batch of messages to the configured endpoint.
    pub async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> anyhow::Result<SentBatch> {
        self.publisher
            .send_batch(messages)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub fn inner(&self) -> std::sync::Arc<dyn traits::MessagePublisher> {
        self.publisher.clone()
    }

    /// Attempt to borrow the underlying concrete publisher as `T`.
    /// Returns `Some(&T)` if the underlying publisher is of type `T`.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.publisher.as_ref().as_any().downcast_ref::<T>()
    }

    /// Registers this publisher globally with a given name.
    pub fn register(&self, name: &str) -> Option<Self> {
        let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let mut map = registry.write().expect("Publisher registry lock poisoned");
        map.insert(name.to_string(), self.clone())
    }

    /// Retrieves a registered publisher by name.
    pub fn get(name: &str) -> Option<Self> {
        let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = registry.read().expect("Publisher registry lock poisoned");
        map.get(name).cloned()
    }

    /// Removes a registered publisher by name.
    pub fn unregister(name: &str) -> Option<Self> {
        let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let mut map = registry.write().expect("Publisher registry lock poisoned");
        map.remove(name)
    }
}

// Generic conversion from any concrete publisher into the generic `Publisher` wrapper.
impl<T> From<T> for Publisher
where
    T: traits::MessagePublisher + 'static,
{
    fn from(p: T) -> Self {
        Self {
            publisher: std::sync::Arc::new(p),
        }
    }
}

pub fn get_publisher(name: &str) -> Option<Publisher> {
    Publisher::get(name)
}

pub fn list_publishers() -> Vec<String> {
    let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    registry
        .read()
        .expect("Publisher registry lock poisoned")
        .keys()
        .cloned()
        .collect()
}

pub fn register_publisher(name: &str, publisher: Publisher) -> Option<Publisher> {
    publisher.register(name)
}

pub fn unregister_publisher(name: &str) -> Option<Publisher> {
    Publisher::unregister(name)
}

pub use crate::middleware::apply_middlewares_to_publisher as apply_middlewares;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, PublisherConfig};
    use crate::CanonicalMessage;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_publisher_config_usage() {
        // Create a PublisherConfig (simulating loading from config)
        let mut publisher_config: PublisherConfig = HashMap::new();
        let endpoint = Endpoint::new_memory("pub_test_topic", 10);
        let channel = endpoint.channel().unwrap();
        publisher_config.insert("my_publisher".to_string(), endpoint);

        let mut publishers = HashMap::new();
        for (name, endpoint) in publisher_config {
            let publisher = Publisher::new(endpoint)
                .await
                .expect("Failed to create publisher");
            publishers.insert(name, publisher);
        }

        let publisher = publishers.get("my_publisher").expect("Publisher not found");
        let msg = CanonicalMessage::from("hello world");

        publisher.send(msg).await.expect("Failed to send message");

        let received = channel.drain_messages();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].get_payload_str(), "hello world");
    }

    #[tokio::test]
    async fn test_publisher_registry() {
        let endpoint = Endpoint::new_memory("registry_test", 10);
        let publisher = Publisher::new(endpoint)
            .await
            .expect("Failed to create publisher");

        publisher.register("static_pub");

        let retrieved = Publisher::get("static_pub").expect("Failed to get publisher");
        assert!(Arc::ptr_eq(&publisher.publisher, &retrieved.publisher));
    }

    #[tokio::test]
    async fn test_publisher_request_batch() {
        use crate::traits::{MessagePublisher, PublisherError, SentBatch};
        use async_trait::async_trait;
        use std::any::Any;

        struct MockRR;
        #[async_trait]
        impl MessagePublisher for MockRR {
            async fn send_batch(
                &self,
                messages: Vec<CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Ok(SentBatch::Partial {
                    responses: Some(messages),
                    failed: vec![],
                })
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let publisher: Publisher = MockRR.into();
        let msgs = vec![CanonicalMessage::from("1"), CanonicalMessage::from("2")];
        let res = publisher.request_batch(msgs).await.unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].get_payload_str(), "1");
    }

    /// Replies to all but the last message, or acks when `reply` is off.
    struct MockReplies {
        reply: bool,
    }

    #[async_trait::async_trait]
    impl traits::MessagePublisher for MockReplies {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, traits::PublisherError> {
            if self.reply {
                Ok(Sent::Response(message))
            } else {
                Ok(Sent::Ack)
            }
        }
        async fn send_batch(
            &self,
            mut messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, traits::PublisherError> {
            if !self.reply {
                return Ok(SentBatch::Ack);
            }
            messages.pop();
            Ok(SentBatch::Partial {
                responses: Some(messages),
                failed: vec![],
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn request_returns_the_response_and_rejects_a_plain_ack() {
        let replying: Publisher = MockReplies { reply: true }.into();
        let response = replying.request(CanonicalMessage::from("ping")).await;
        assert_eq!(response.unwrap().get_payload_str(), "ping");

        let acking: Publisher = MockReplies { reply: false }.into();
        let error = acking
            .request(CanonicalMessage::from("ping"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("only an acknowledgment"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn request_batch_rejects_acks_and_missing_responses() {
        let batch = || vec![CanonicalMessage::from("1"), CanonicalMessage::from("2")];

        let acking: Publisher = MockReplies { reply: false }.into();
        let error = acking.request_batch(batch()).await.unwrap_err();
        assert!(
            error.to_string().contains("only acknowledgments"),
            "{error}"
        );

        let short: Publisher = MockReplies { reply: true }.into();
        let error = short.request_batch(batch()).await.unwrap_err();
        assert!(error.to_string().contains("expected responses"), "{error}");

        assert!(short.request_batch(Vec::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn registry_lists_and_unregisters_by_name() {
        let publisher: Publisher = MockReplies { reply: false }.into();
        assert!(register_publisher("registry_lifecycle", publisher).is_none());

        assert!(list_publishers().contains(&"registry_lifecycle".to_string()));
        assert!(get_publisher("registry_lifecycle").is_some());

        assert!(unregister_publisher("registry_lifecycle").is_some());
        assert!(get_publisher("registry_lifecycle").is_none());
        assert!(!list_publishers().contains(&"registry_lifecycle".to_string()));
    }

    #[tokio::test]
    async fn from_config_builds_the_publisher_a_json_endpoint_describes() {
        let topic = format!("pub_json_{}", fast_uuid_v7::gen_id_str());
        let publisher = Publisher::from_config(serde_json::json!({ "memory": { "topic": topic } }))
            .await
            .unwrap();
        assert!(matches!(
            publisher.send("x".into()).await.unwrap(),
            Sent::Ack
        ));
        let error = Publisher::from_config(serde_json::json!({ "no_such_endpoint": {} }))
            .await
            .err()
            .expect("an unknown endpoint type is refused");
        assert!(error.to_string().contains("no_such_endpoint"), "{error}");
    }
}
