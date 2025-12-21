//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::models::DeduplicationMiddleware;
use crate::traits::{into_batch_commit_func, BatchCommitFunc, CommitFunc, MessageConsumer};
use crate::CanonicalMessage;
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
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        loop {
            let (message, commit) = self.inner.receive().await?;
            let key = match message.message_id {
                Some(id) => id.to_be_bytes().to_vec(),
                None => {
                    // If there's no message_id, we can't deduplicate. Pass it through.
                    return Ok((message, commit));
                }
            };

            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            // Atomically insert only if key doesn't exist
            match self.db.compare_and_swap(
                &key,
                None as Option<&[u8]>,
                Some(&now.to_be_bytes()[..]),
            )? {
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

            return Ok((message, commit));
        }
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
