use crate::traits::{MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

pub struct SwitchPublisher {
    metadata_key: String,
    cases: HashMap<String, Arc<dyn MessagePublisher>>,
    default: Option<Arc<dyn MessagePublisher>>,
}

impl SwitchPublisher {
    pub fn new(
        metadata_key: String,
        cases: HashMap<String, Arc<dyn MessagePublisher>>,
        default: Option<Arc<dyn MessagePublisher>>,
    ) -> Self {
        Self {
            metadata_key,
            cases,
            default,
        }
    }

    fn get_publisher(&self, message: &CanonicalMessage) -> Option<&Arc<dyn MessagePublisher>> {
        if let Some(val) = message.metadata.get(&self.metadata_key) {
            if let Some(publisher) = self.cases.get(val) {
                return Some(publisher);
            }
        }
        self.default.as_ref()
    }
}

#[async_trait]
impl MessagePublisher for SwitchPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if let Some(publisher) = self.get_publisher(&message) {
            publisher.send(message).await
        } else {
            warn!(
                "Switch publisher dropped message: metadata key '{}' not found or no matching case/default.",
                self.metadata_key
            );
            Ok(Sent::Ack)
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_switch_publisher_routing() {
        // Setup destinations
        let pub_a = MemoryPublisher::new_local("topic_a", 10);
        let pub_b = MemoryPublisher::new_local("topic_b", 10);
        let pub_default = MemoryPublisher::new_local("topic_default", 10);

        let chan_a = pub_a.channel();
        let chan_b = pub_b.channel();
        let chan_default = pub_default.channel();

        let mut cases = HashMap::new();
        cases.insert(
            "A".to_string(),
            Arc::new(pub_a) as Arc<dyn MessagePublisher>,
        );
        cases.insert(
            "B".to_string(),
            Arc::new(pub_b) as Arc<dyn MessagePublisher>,
        );

        let switch = SwitchPublisher::new(
            "route_key".to_string(),
            cases,
            Some(Arc::new(pub_default) as Arc<dyn MessagePublisher>),
        );

        // Test Case A
        let msg_a = CanonicalMessage::from_str("payload_a").with_metadata_kv("route_key", "A");
        switch.send(msg_a).await.unwrap();
        assert_eq!(chan_a.len(), 1);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 0);
        chan_a.drain_messages();

        // Test Case B
        let msg_b = CanonicalMessage::from_str("payload_b").with_metadata_kv("route_key", "B");
        switch.send(msg_b).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 1);
        assert_eq!(chan_default.len(), 0);
        chan_b.drain_messages();

        // Test Default (Unknown Key)
        let msg_c =
            CanonicalMessage::new(b"payload_c".to_vec(), None).with_metadata_kv("route_key", "C");
        switch.send(msg_c).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 1);
        chan_default.drain_messages();

        // Test Default (Missing Key)
        let msg_d = CanonicalMessage::new(b"payload_d".to_vec(), None);
        switch.send(msg_d).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 1);
    }
}
