//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::SledConfig;
use crate::traits::{
    ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use sled::Transactional;
use sled::{Db, IVec, Tree};
use std::any::Any;
use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Mutex;
use std::time::Duration;
use tracing::trace;

pub struct SledPublisher {
    db: Db,
    tree: Tree,
}

static SLED_DBS: Lazy<Mutex<HashMap<String, Db>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn get_or_open_db(path: &str) -> anyhow::Result<Db> {
    let mut dbs = SLED_DBS
        .lock()
        .map_err(|_| anyhow!("Sled DB registry lock poisoned"))?;
    if let Some(db) = dbs.get(path) {
        return Ok(db.clone());
    }
    let db = sled::open(path)?;
    dbs.insert(path.to_string(), db.clone());
    Ok(db)
}

pub fn close_db(path: &str) -> anyhow::Result<()> {
    let mut dbs = SLED_DBS
        .lock()
        .map_err(|_| anyhow!("Sled DB registry lock poisoned"))?;
    if let Some(db) = dbs.remove(path) {
        db.flush()?;
    }
    Ok(())
}

impl SledPublisher {
    pub fn new(config: &SledConfig) -> anyhow::Result<Self> {
        let db = get_or_open_db(&config.path).context("Failed to open Sled DB")?;
        let tree_name = config.tree.as_deref().unwrap_or("default");
        let tree = db
            .open_tree(tree_name)
            .context("Failed to open Sled tree")?;
        Ok(Self { db, tree })
    }
}

#[async_trait]
impl MessagePublisher for SledPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        // Source/provenance keys are per-hop context, not persisted fields.
        message.strip_source_metadata();
        let id = self
            .db
            .generate_id()
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        let key = id.to_be_bytes();
        let value =
            serde_json::to_vec(&message).map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;

        self.tree
            .insert(key, value)
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;

        self.tree
            .flush_async()
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch to Sled");
        let mut batch = sled::Batch::default();
        for mut message in messages {
            // Source/provenance keys are per-hop context, not persisted fields.
            message.strip_source_metadata();
            let id = self
                .db
                .generate_id()
                .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
            let key = id.to_be_bytes();
            let value = serde_json::to_vec(&message)
                .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
            batch.insert(&key, value);
        }
        self.tree
            .apply_batch(batch)
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        self.tree
            .flush_async()
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;

        Ok(SentBatch::Ack)
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.tree.first() {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("Sled error: {}", e))),
        };
        EndpointStatus {
            healthy,
            target: String::from_utf8_lossy(&self.tree.name()).to_string(),
            error,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct SledConsumer {
    _db: Db,
    tree: Tree,
    inflight_tree: Tree,
    notify_rx: async_channel::Receiver<()>,
    delete_after_read: bool,
    last_key: Option<IVec>,
    /// Drain mode: only then does an idle wait time out into an empty batch.
    exit_on_empty: bool,
}

impl SledConsumer {
    pub fn new(config: &SledConfig) -> anyhow::Result<Self> {
        let db = get_or_open_db(&config.path).context("Failed to open Sled DB")?;
        let tree_name = config.tree.as_deref().unwrap_or("default");
        let tree = db
            .open_tree(tree_name)
            .context("Failed to open Sled tree")?;
        let inflight_tree = db
            .open_tree(format!("{}_inflight", tree_name))
            .context("Failed to open Sled inflight tree")?;

        if config.delete_after_read && !inflight_tree.is_empty() {
            let inflight_items: Vec<_> = inflight_tree.iter().collect::<Result<_, _>>()?;
            if !inflight_items.is_empty() {
                tracing::info!(
                    "Found {} inflight messages from previous Sled session, attempting recovery...",
                    inflight_items.len()
                );
                let (tree_ref, inflight_tree_ref) = (&tree, &inflight_tree);
                let tx_result: Result<(), _> =
                    (tree_ref, inflight_tree_ref).transaction(|(t_main, t_inflight)| {
                        for (key, value) in &inflight_items {
                            t_main.insert(key, value)?;
                            t_inflight.remove(key)?;
                        }
                        Ok::<(), sled::transaction::ConflictableTransactionError<()>>(())
                    });
                if let Err(e) = tx_result {
                    return Err(anyhow!("Failed to recover inflight Sled messages: {:?}", e));
                }
            }
        }

        let subscriber = tree.watch_prefix(vec![]);
        let (tx, rx) = async_channel::bounded(1);

        std::thread::spawn(move || {
            let mut subscriber = subscriber;
            loop {
                match subscriber.next_timeout(Duration::from_millis(100)) {
                    Ok(_event) => {
                        if tx.send_blocking(()).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        if tx.is_closed() {
                            break;
                        }
                    }
                }
                if tx.is_closed() {
                    break;
                }
            }
        });

        let last_key = if !config.read_from_start {
            tree.last().map_err(|e| anyhow!(e))?.map(|(k, _)| k)
        } else {
            None
        };

        Ok(Self {
            _db: db,
            tree,
            inflight_tree,
            notify_rx: rx,
            delete_after_read: config.delete_after_read,
            last_key,
            exit_on_empty: false,
        })
    }
}

#[async_trait]
impl MessageConsumer for SledConsumer {
    // Acking removes each message from the inflight tree by key (or is a no-op when
    // not deleting), so commits are independent and order-free.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            let next_item = if self.delete_after_read {
                // Queue mode: Move from main tree to inflight tree atomically
                loop {
                    if let Some((k, v)) = self
                        .tree
                        .first()
                        .map_err(|e| ConsumerError::Connection(anyhow!(e)))?
                    {
                        let k_clone = k.clone();
                        let v_clone = v.clone();
                        let (tree, inflight) = (&self.tree, &self.inflight_tree);
                        let tx_result: std::result::Result<
                            Option<(IVec, IVec)>,
                            sled::transaction::TransactionError<()>,
                        > = (tree, inflight).transaction(|(t_main, t_inflight)| {
                            if t_main.get(&k_clone)?.is_some() {
                                t_main.remove(&k_clone)?;
                                t_inflight.insert(&k_clone, &v_clone)?;
                                Ok(Some((k_clone.clone(), v_clone.clone())))
                            } else {
                                Ok(None)
                            }
                        });
                        match tx_result {
                            Ok(Some(item)) => break Some(item),
                            Ok(None) => continue,
                            Err(e) => return Err(ConsumerError::Connection(anyhow!("{:?}", e))),
                        }
                    } else {
                        break None;
                    }
                }
            } else {
                // Topic mode: Scan forward
                let start = if let Some(k) = &self.last_key {
                    Bound::Excluded(k)
                } else {
                    Bound::Unbounded
                };
                self.tree
                    .range::<&IVec, _>((start, Bound::Unbounded))
                    .next()
                    .transpose()
                    .map_err(|e| ConsumerError::Connection(anyhow!(e)))?
            };

            if let Some((key, value)) = next_item {
                self.last_key = Some(key.clone());
                let message = serde_json::from_slice(&value)
                    .map_err(|e| ConsumerError::Connection(anyhow!(e)))?;

                let tree = self.tree.clone();
                let inflight_tree = self.inflight_tree.clone();
                let delete = self.delete_after_read;
                let key_clone = key.clone();
                let value_clone = value.to_vec();

                let commit = Box::new(move |disposition: MessageDisposition| {
                    Box::pin(async move {
                        if delete {
                            match disposition {
                                MessageDisposition::Ack | MessageDisposition::Reply(_) => {
                                    if matches!(disposition, MessageDisposition::Reply(_)) {
                                        tracing::warn!("Sled consumer received a Reply/StreamReply, but replying is not supported. Dropping reply.");
                                    }
                                    inflight_tree.remove(key_clone).map_err(|e| anyhow!(e))?;
                                }
                                MessageDisposition::Nack => {
                                    // Re-insert on Nack in Queue mode (move back from inflight to main)
                                    (&tree, &inflight_tree)
                                        .transaction(|(t_main, t_inflight)| {
                                            if t_inflight.remove(&key_clone)?.is_some() {
                                                t_main
                                                    .insert(&key_clone, value_clone.as_slice())?;
                                            }
                                            Ok(())
                                        })
                                        .map_err(|e: sled::transaction::TransactionError<()>| {
                                            anyhow!("{:?}", e)
                                        })?;
                                }
                            }
                        }
                        Ok(())
                    }) as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                });

                return Ok(Received { message, commit });
            }

            // If no message, wait for notification
            if self.notify_rx.recv().await.is_err() {
                return Err(ConsumerError::EndOfStream);
            }
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        let mut messages = Vec::with_capacity(max_messages);
        let mut commits = Vec::with_capacity(max_messages);

        // First message blocks; drain mode times out into an empty batch (recv is cancel-safe).
        let Some(first) = crate::traits::drain_gated(self.exit_on_empty, self.receive()).await
        else {
            return Ok(ReceivedBatch::empty());
        };
        let first = first?;
        messages.push(first.message);
        commits.push(first.commit);

        // Subsequent messages poll with timeout
        for _ in 1..max_messages {
            match tokio::time::timeout(Duration::from_millis(10), self.receive()).await {
                Ok(Ok(received)) => {
                    messages.push(received.message);
                    commits.push(received.commit);
                }
                _ => break,
            }
        }

        Ok(ReceivedBatch {
            messages,
            commit: Box::new(move |dispositions| {
                Box::pin(async move {
                    for (commit, disposition) in commits.into_iter().zip(dispositions) {
                        commit(disposition).await?;
                    }
                    Ok(())
                })
            }),
        })
    }

    async fn status(&self) -> EndpointStatus {
        // Note: Tree::len() is O(n) in sled and can be expensive for large trees.
        let (healthy, error, pending, inflight) = match self.tree.flush() {
            Ok(_) => (
                true,
                None,
                Some(self.tree.len()),
                Some(self.inflight_tree.len()),
            ),
            Err(e) => (false, Some(format!("Sled flush failed: {}", e)), None, None),
        };

        EndpointStatus {
            healthy,
            target: String::from_utf8_lossy(&self.tree.name()).to_string(),
            pending,
            error,
            details: serde_json::json!({
                "inflight": inflight
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_sled_queue_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let config = SledConfig {
            path: path.clone(),
            tree: None,
            read_from_start: true,
            delete_after_read: true,
        };

        let publisher = SledPublisher::new(&config).unwrap();
        let mut consumer = SledConsumer::new(&config).unwrap();

        let msg = CanonicalMessage::new(b"queue_item".to_vec(), None);
        publisher.send(msg.clone()).await.unwrap();

        let received = timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();

        assert_eq!(received.message.payload, msg.payload);

        // Commit (Ack)
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        // Verify DB is empty
        let db = get_or_open_db(&path).unwrap();
        let tree = db.open_tree("default").unwrap();
        assert!(tree.is_empty());
        close_db(&path).unwrap();
    }

    #[tokio::test]
    async fn test_sled_topic_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let config = SledConfig {
            path: path.clone(),
            tree: Some("topic".to_string()),
            read_from_start: true,
            delete_after_read: false,
        };

        let publisher = SledPublisher::new(&config).unwrap();
        let mut consumer = SledConsumer::new(&config).unwrap();

        let msg1 = CanonicalMessage::new(b"msg1".to_vec(), None);
        publisher.send(msg1.clone()).await.unwrap();

        let received1 = timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for msg1")
            .unwrap();
        assert_eq!(received1.message.payload, msg1.payload);

        let msg2 = CanonicalMessage::new(b"msg2".to_vec(), None);
        publisher.send(msg2.clone()).await.unwrap();

        let received2 = timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for msg2")
            .unwrap();
        assert_eq!(received2.message.payload, msg2.payload);
        close_db(&path).unwrap();
    }

    #[tokio::test]
    async fn test_sled_nack_requeue() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let config = SledConfig {
            path: path.clone(),
            tree: None,
            read_from_start: true,
            delete_after_read: true,
        };

        let publisher = SledPublisher::new(&config).unwrap();
        let mut consumer = SledConsumer::new(&config).unwrap();

        let msg = CanonicalMessage::new(b"retry_me".to_vec(), None);
        publisher.send(msg.clone()).await.unwrap();

        // Receive and Nack
        let received = consumer.receive().await.unwrap();
        (received.commit)(MessageDisposition::Nack).await.unwrap();

        // Should be available again
        let received_retry = timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for retry")
            .unwrap();

        assert_eq!(received_retry.message.payload, msg.payload);
        close_db(&path).unwrap();
    }

    #[tokio::test]
    async fn test_sled_status() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let config = SledConfig {
            path: path.clone(),
            tree: Some("status_tree".to_string()),
            ..Default::default()
        };

        let publisher = SledPublisher::new(&config).unwrap();
        let status = publisher.status().await;
        assert!(status.healthy);
        assert_eq!(status.target, "status_tree");
        close_db(&path).unwrap();
    }
}
