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

    /// Registers this publisher globally with a given name.
    pub fn register(&self, name: &str) {
        let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let mut map = registry.write().expect("Publisher registry lock poisoned");
        map.insert(name.to_string(), self.clone());
    }

    /// Retrieves a registered publisher by name.
    pub fn get(name: &str) -> Option<Self> {
        let registry = PUBLISHER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = registry.read().expect("Publisher registry lock poisoned");
        map.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::get_or_create_channel;
    use crate::models::{Endpoint, EndpointType, MemoryConfig, PublisherConfig};
    use crate::CanonicalMessage;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_publisher_config_usage() {
        // 1. Create a PublisherConfig (simulating loading from config)
        let mut publisher_config: PublisherConfig = HashMap::new();

        let mem_cfg = MemoryConfig {
            topic: "pub_test_topic".to_string(),
            capacity: Some(10),
        };
        let endpoint = Endpoint::new(EndpointType::Memory(mem_cfg.clone()));

        publisher_config.insert("my_publisher".to_string(), endpoint);

        // 2. Initialize publishers from config
        let mut publishers = HashMap::new();
        for (name, endpoint) in publisher_config {
            let publisher = Publisher::new(endpoint)
                .await
                .expect("Failed to create publisher");
            publishers.insert(name, publisher);
        }

        // 3. Retrieve and use the publisher
        let publisher = publishers.get("my_publisher").expect("Publisher not found");
        let msg = CanonicalMessage::from("hello world");

        publisher.send(msg).await.expect("Failed to send message");

        // 4. Verify with underlying channel
        let channel = get_or_create_channel(&mem_cfg);
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
}
