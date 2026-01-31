use crate::models::WeakJoinMiddleware;
use crate::traits::{ConsumerError, MessageConsumer, MessageDisposition, ReceivedBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct JoinState {
    // Key -> (CreationTime, Messages)
    pending: HashMap<String, (Instant, Vec<CanonicalMessage>)>,
    ready_buffer: Vec<CanonicalMessage>,
}

pub struct WeakJoinConsumer {
    inner: Box<dyn MessageConsumer>,
    config: WeakJoinMiddleware,
    state: Arc<Mutex<JoinState>>,
}

impl WeakJoinConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &WeakJoinMiddleware) -> Self {
        Self {
            inner,
            config: config.clone(),
            state: Arc::new(Mutex::new(JoinState {
                pending: HashMap::new(),
                ready_buffer: Vec::new(),
            })),
        }
    }

    fn try_join(&self, key: &str, messages: Vec<CanonicalMessage>) -> CanonicalMessage {
        let payloads: Vec<Value> = messages
            .iter()
            .map(|m| match serde_json::from_slice(&m.payload) {
                Ok(v) => v,
                Err(_) => Value::String(String::from_utf8_lossy(&m.payload).to_string()),
            })
            .collect();

        let merged_payload = serde_json::to_vec(&payloads).unwrap_or_default();
        let mut new_msg = CanonicalMessage::new(merged_payload, Some(fast_uuid_v7::gen_id()));

        if let Some(first) = messages.first() {
            new_msg.metadata = first.metadata.clone();
        }
        new_msg
            .metadata
            .insert(self.config.group_by.clone(), key.to_string());
        new_msg
    }

    fn check_timeouts(&self, state: &mut JoinState, ready_messages: &mut Vec<CanonicalMessage>) {
        let now = Instant::now();
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let mut timed_out_keys = Vec::new();

        for (key, (start_time, _)) in state.pending.iter() {
            if now.duration_since(*start_time) >= timeout {
                timed_out_keys.push(key.clone());
            }
        }

        for key in timed_out_keys {
            if let Some((_, msgs)) = state.pending.remove(&key) {
                ready_messages.push(self.try_join(&key, msgs));
            }
        }
    }
}

#[async_trait]
impl MessageConsumer for WeakJoinConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut state = self.state.lock().await;

        if !state.ready_buffer.is_empty() {
            let count = std::cmp::min(state.ready_buffer.len(), max_messages);
            let messages: Vec<_> = state.ready_buffer.drain(0..count).collect();
            return Ok(ReceivedBatch {
                messages,
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        let now = Instant::now();
        let timeout_duration = Duration::from_millis(self.config.timeout_ms);
        let next_timeout = state
            .pending
            .values()
            .map(|(start, _)| *start + timeout_duration)
            .min()
            .unwrap_or(now + Duration::from_secs(3600));

        let sleep_duration = next_timeout.saturating_duration_since(now);
        drop(state);

        let batch_future = self.inner.receive_batch(max_messages);
        let timeout_future = tokio::time::sleep(sleep_duration);

        tokio::select! {
            res = batch_future => {
                match res {
                    Ok(batch) => {
                        // Weak join: Ack immediately to avoid complex disposition mapping
                        let count = batch.messages.len();
                        if count > 0 {
                            if let Err(e) = (batch.commit)(vec![MessageDisposition::Ack; count]).await {
                                return Err(ConsumerError::Connection(e));
                            }
                        }

                        let mut state = self.state.lock().await;
                        let now = Instant::now();
                        let mut ready_messages = Vec::new();

                        for msg in batch.messages {
                            let key = msg
                                .metadata
                                .get(&self.config.group_by)
                                .cloned()
                                .unwrap_or_else(|| "default".to_string());
                            let entry = state
                                .pending
                                .entry(key.clone())
                                .or_insert_with(|| (now, Vec::new()));
                            entry.1.push(msg);

                            if entry.1.len() >= self.config.expected_count {
                                let (_, msgs) = state.pending.remove(&key).unwrap();
                                ready_messages.push(self.try_join(&key, msgs));
                            }
                        }
                        self.check_timeouts(&mut state, &mut ready_messages);

                        if ready_messages.len() > max_messages {
                            let overflow = ready_messages.split_off(max_messages);
                            state.ready_buffer.extend(overflow);
                        }

                        Ok(ReceivedBatch {
                            messages: ready_messages,
                            commit: Box::new(|_| Box::pin(async { Ok(()) })),
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            _ = timeout_future => {
                let mut state = self.state.lock().await;
                let mut ready_messages = Vec::new();
                self.check_timeouts(&mut state, &mut ready_messages);

                if ready_messages.len() > max_messages {
                    let overflow = ready_messages.split_off(max_messages);
                    state.ready_buffer.extend(overflow);
                }

                Ok(ReceivedBatch {
                    messages: ready_messages,
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                })
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryConsumer;
    use crate::CanonicalMessage;
    use serde_json::json;

    #[tokio::test]
    async fn test_weak_join_grouping() {
        let config = WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 2,
            timeout_ms: 1000,
        };

        let mem_consumer = MemoryConsumer::new_local("join_test", 10);
        let channel = mem_consumer.channel();

        // Send 2 messages with group_id "A"
        let msg1 = CanonicalMessage::from_json(json!({"val": 1}))
            .unwrap()
            .with_metadata_kv("group_id", "A");
        let msg2 = CanonicalMessage::from_json(json!({"val": 2}))
            .unwrap()
            .with_metadata_kv("group_id", "A");

        channel.send_message(msg1).await.unwrap();
        channel.send_message(msg2).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        let batch = join_consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);

        let joined = &batch.messages[0];
        let payload: Vec<Value> = serde_json::from_slice(&joined.payload).unwrap();
        assert_eq!(payload.len(), 2);
        assert_eq!(payload[0]["val"], 1);
        assert_eq!(payload[1]["val"], 2);
        assert_eq!(
            joined.metadata.get("group_id").map(|s| s.as_str()),
            Some("A")
        );
    }

    #[tokio::test]
    async fn test_weak_join_timeout() {
        let config = WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 3,
            timeout_ms: 100,
        };

        let mem_consumer = MemoryConsumer::new_local("join_timeout_test", 10);
        let channel = mem_consumer.channel();

        // Send 1 message with group_id "B" (less than expected 3)
        let msg1 = CanonicalMessage::from_json(json!({"val": 1}))
            .unwrap()
            .with_metadata_kv("group_id", "B");

        channel.send_message(msg1).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        // First call will pick up the message but return empty batch because count < expected
        let batch1 = join_consumer.receive_batch(10).await.unwrap();
        assert!(batch1.messages.is_empty());

        // Wait for timeout to expire
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Second call should trigger timeout logic and return the partial batch
        let batch2 = join_consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch2.messages.len(), 1);

        let joined = &batch2.messages[0];
        let payload: Vec<Value> = serde_json::from_slice(&joined.payload).unwrap();
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0]["val"], 1);
    }
}
