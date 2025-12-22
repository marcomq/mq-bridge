//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::models::DeduplicationMiddleware;
use crate::traits::{
    into_batch_commit_func, ConsumerError, MessageConsumer, Received, ReceivedBatch,
};
use anyhow::Context;
use async_trait::async_trait;
use sled::Db;
use std::any::Any;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, instrument, warn};

pub struct DeduplicationConsumer {
    inner: Box<dyn MessageConsumer>,
    db: Arc<Db>,
    ttl_seconds: u64,
}

impl DeduplicationConsumer {
    pub fn new(
        inner: Box<dyn MessageConsumer>,
        config: &DeduplicationMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        info!(
            "Deduplication Middleware enabled for route '{}' with TTL {}s",
            route_name, config.ttl_seconds
        );
        let db = sled::open(&config.sled_path)?;
        Ok(Self {
            inner,
            db: Arc::new(db),
            ttl_seconds: config.ttl_seconds,
        })
    }
}

#[async_trait]
impl MessageConsumer for DeduplicationConsumer {
    #[instrument(skip_all)]
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            let received = self.inner.receive().await?;
            let message = received.message;
            let commit = received.commit;
            let key = message.message_id.to_be_bytes().to_vec();

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_secs();
            // Atomically insert only if key doesn't exist
            match self
                .db
                .compare_and_swap(&key, None as Option<&[u8]>, Some(&now.to_be_bytes()[..]))
                .context("Failed to perform compare-and-swap in deduplication DB")?
            {
                Ok(_) => {
                    // Successfully inserted - not a duplicate, proceed
                }
                Err(_) => {
                    // Key already exists - duplicate detected
                    info!("Duplicate message detected and skipped");
                    commit(None).await;
                    continue;
                }
            }

            if rand::random::<u8>() < 5 {
                // ~2% chance
                let db = self.db.clone();
                let ttl = self.ttl_seconds;
                tokio::spawn(async move {
                    let now_duration = match SystemTime::now().duration_since(UNIX_EPOCH) {
                        Ok(duration) => duration,
                        Err(e) => {
                            error!("Failed to get system time duration since UNIX_EPOCH for deduplication cleanup: {}", e);
                            return; // Exit the spawned task if we can't get the current time
                        }
                    };
                    // Use saturating_sub to prevent underflow if ttl is very large, though unlikely for timestamps.
                    let cutoff = now_duration.as_secs().saturating_sub(ttl);

                    for item_result in db.iter() {
                        match item_result {
                            Ok((key, val)) => {
                                if val.as_ref().len() != 8 {
                                    warn!("Deduplication DB entry for key {:?} has invalid timestamp length (expected 8 bytes, got {}). Skipping entry.", key, val.as_ref().len());
                                    continue; // Move to the next item
                                }

                                // After checking the length, `try_into()` from `&[u8]` to `&[u8; 8]` is infallible.
                                // However, using `match` explicitly handles the `Err` case for robustness and clarity.
                                let timestamp_bytes: [u8; 8] = match val.as_ref().try_into() {
                                    Ok(bytes) => bytes,
                                    Err(e) => {
                                        error!("Internal error: Failed to convert DB value to [u8; 8] after length check for key {:?}: {}", key, e);
                                        continue; // Move to the next item
                                    }
                                };
                                let timestamp = u64::from_be_bytes(timestamp_bytes);

                                // If the timestamp is older than the cutoff, remove it.
                                if timestamp < cutoff {
                                    match db.remove(&key) {
                                        Ok(_) => debug!("Removed expired deduplication entry for key: {:?}", key),
                                        Err(e) => error!("Failed to remove expired deduplication entry for key {:?}: {}", key, e),
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error iterating deduplication DB during cleanup: {}", e);
                                continue; // Continue to the next item if iteration itself yields an error
                            }
                        }
                    }
                });
            }

            return Ok(Received { message, commit });
        }
    }

    /// Note: This implementation ignores `_max_messages` and always fetches a single message
    /// to ensure correct deduplication logic per message.
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let received = self.receive().await?;
        let commit = into_batch_commit_func(received.commit);
        Ok(ReceivedBatch {
            messages: vec![received.message],
            commit,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryConsumer;
    use crate::models::{DeduplicationMiddleware, MemoryConfig};
    use crate::CanonicalMessage;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_deduplication_logic() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("dedup_test").to_str().unwrap().to_string();

        let config = DeduplicationMiddleware {
            sled_path: db_path,
            ttl_seconds: 60,
        };

        let mem_cfg = MemoryConfig {
            topic: "dedup_topic".to_string(),
            capacity: Some(10),
        };
        let mem_consumer = MemoryConsumer::new(&mem_cfg).unwrap();
        let channel = mem_consumer.channel();

        // 1. Send a message
        let msg1 = CanonicalMessage::new(b"data1".to_vec(), Some(100));
        channel.send_message(msg1).await.unwrap();

        // 2. Send a duplicate message
        let msg2 = CanonicalMessage::new(b"data1_dup".to_vec(), Some(100));
        channel.send_message(msg2).await.unwrap();

        // 3. Send a new message
        let msg3 = CanonicalMessage::new(b"data2".to_vec(), Some(101));
        channel.send_message(msg3).await.unwrap();

        let mut dedup_consumer =
            DeduplicationConsumer::new(Box::new(mem_consumer), &config, "test_route").unwrap();

        // First receive: Should be msg1 (ID 100)
        let rec1 = dedup_consumer.receive().await.unwrap();
        assert_eq!(rec1.message.message_id, 100);
        (rec1.commit)(None).await;

        // Second receive: Should be msg3 (ID 101). msg2 (ID 100) is skipped internally.
        let rec2 = dedup_consumer.receive().await.unwrap();
        assert_eq!(rec2.message.message_id, 101);
        (rec2.commit)(None).await;
    }
}
