//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::event_store::{EventStore, EventStoreConsumer, RetentionPolicy};
use crate::models::FileConfig;
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{self, AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{info, instrument, trace};

/// A sink that writes messages to a file, one per line.
#[derive(Clone)]
pub struct FilePublisher {
    writer: Arc<Mutex<BufWriter<File>>>,
    path: String,
}

impl FilePublisher {
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        let path = Path::new(&config.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create parent directory for file: {:?}", parent)
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| {
                format!("Failed to open or create file for writing: {}", config.path)
            })?;

        info!(path = %config.path, "File sink opened for appending");
        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
            path: config.path.clone(),
        })
    }
}

#[async_trait]
impl MessagePublisher for FilePublisher {
    #[instrument(skip_all, fields(batch_size = messages.len()), level = "debug")]
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        trace!(count = messages.len(), path = %self.path, message_ids = ?LazyMessageIds(&messages), "Writing batch to file");
        let mut writer = self.writer.lock().await;
        let mut failed_messages = Vec::new();

        // Iterate over messages, consuming them
        for msg in messages {
            let serialized_msg = if msg
                .metadata
                .get("mq_bridge.original_format")
                .map(|s| s.as_str())
                == Some("raw")
            {
                msg.payload.to_vec()
            } else {
                match serde_json::to_vec(&msg) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to serialize message for file sink: {}", e);
                        failed_messages
                            .push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
                        continue;
                    }
                }
            };
            if let Err(e) = writer.write_all(&serialized_msg).await {
                tracing::error!("Failed to write message to file: {}", e);
                failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
            } else if let Err(e) = writer.write_all(b"\n").await {
                tracing::error!("Failed to write newline to file: {}", e);
                // If write fails, add the message to the failed list
                failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
            }
        }

        writer
            .flush()
            .await
            .context("Failed to flush file writer")?;
        if failed_messages.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: None,
                failed: failed_messages,
            })
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.flush().await?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static FILE_EVENT_STORES: Lazy<Mutex<HashMap<String, Weak<EventStore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

struct FileFeedState {
    /// For Consume mode: number of lines currently buffered in EventStore.
    /// We skip this many lines from the start of the file when reading.
    lines_in_memory: usize,
    /// For Subscribe mode: the file offset (bytes) where we stopped reading.
    last_position: u64,
}

async fn create_file_event_store(config: &FileConfig) -> anyhow::Result<Arc<EventStore>> {
    let path = config.path.clone();
    let should_delete = config.delete.unwrap_or(!config.subscribe_mode);

    // Shared state to coordinate the reader and the drop (delete) logic.
    let feed_state = Arc::new(tokio::sync::Mutex::new(FileFeedState {
        lines_in_memory: 0,
        last_position: 0,
    }));

    // Lock to serialize file modification operations
    let file_op_lock = Arc::new(tokio::sync::Mutex::new(()));

    let feed_state_clone = feed_state.clone();
    let path_clone = path.clone();
    let file_op_lock_clone = file_op_lock.clone();

    let retention = RetentionPolicy {
        gc_interval: std::time::Duration::ZERO,
        ..Default::default()
    };
    // Use immediate GC for file stores to ensure files are truncated promptly on ack.

    // 1. Create EventStore with on_drop callback
    let store = Arc::new(
        EventStore::new(retention).with_drop_callback(move |events| {
            if !should_delete {
                // If deletion is disabled, we don't modify the file.
                return;
            }
            let count = events.len();
            if count == 0 {
                return;
            }
            let state = feed_state_clone.clone();
            let path = path_clone.clone();
            let file_op_lock = file_op_lock_clone.clone();

            // Spawn a task to rewrite the file, removing the dropped lines.
            // We use the mutex to ensure we don't race with the reader or other drops.
            tokio::spawn(async move {
                // Serialize file operations to prevent race conditions between multiple GCs
                let _guard = file_op_lock.lock().await;

                if let Err(e) = remove_lines_from_file(&path, count).await {
                    tracing::error!("Failed to remove lines from file {}: {}", path, e);
                } else {
                    let mut state = state.lock().await;
                    state.lines_in_memory = state.lines_in_memory.saturating_sub(count);
                    trace!(
                        "Removed {} lines from {}, remaining in memory: {}",
                        count,
                        path,
                        state.lines_in_memory
                    );
                }
            });
        }),
    );

    // 2. Spawn background reader task
    let store_weak = Arc::downgrade(&store);
    let path_clone = path.clone();
    let feed_state_clone = feed_state.clone();
    let file_op_lock_clone = file_op_lock.clone();

    tokio::spawn(async move {
        loop {
            // Check if the store is still alive
            let store_clone = match store_weak.upgrade() {
                Some(s) => s,
                None => break, // Exit if EventStore is dropped
            };

            // Acquire file op lock first to coordinate with GC
            let _file_guard = file_op_lock_clone.lock().await;

            let mut state = feed_state_clone.lock().await;

            // Open file
            let file_res = OpenOptions::new().read(true).open(&path_clone).await;
            let mut file = match file_res {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open file {}: {}", path_clone, e);
                    drop(state);
                    drop(_file_guard);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Position the reader
            if !should_delete {
                if let Err(e) = file.seek(SeekFrom::Start(state.last_position)).await {
                    tracing::error!("Failed to seek in file {}: {}", path_clone, e);
                }
            } else {
                // In consume mode, we skip lines that are already buffered in memory
                // because they are still in the file (until dropped).
                let mut reader = BufReader::new(file);
                let mut lines_skipped = 0;
                let mut error = false;
                while lines_skipped < state.lines_in_memory {
                    let mut buf = Vec::new();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) => break, // EOF
                        Ok(_) => lines_skipped += 1,
                        Err(e) => {
                            tracing::error!("Error skipping lines in {}: {}", path_clone, e);
                            error = true;
                            break;
                        }
                    }
                }
                if error {
                    drop(state);
                    drop(_file_guard);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                file = reader.into_inner();
            }

            // Read new lines
            let mut reader = BufReader::new(file);
            let mut lines_read = 0;
            loop {
                let mut buffer = Vec::new();
                match reader.read_until(b'\n', &mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if buffer.ends_with(b"\n") {
                            buffer.pop();
                        }
                        let msg = parse_message(buffer);
                        store_clone.append(msg).await;
                        lines_read += 1;

                        if !should_delete {
                            state.last_position += n as u64;
                        } else {
                            state.lines_in_memory += 1;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error reading from {}: {}", path_clone, e);
                        break;
                    }
                }
            }

            drop(state); // Release lock before sleeping
            drop(_file_guard);

            // If we didn't read anything, sleep a bit (polling)
            if lines_read == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    });

    Ok(store)
}

async fn remove_lines_from_file(path: &str, count: usize) -> anyhow::Result<()> {
    let unique_id = fast_uuid_v7::gen_id_str();
    let temp_path = format!("{}.{}.tmp", path, unique_id);

    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let temp_file = File::create(&temp_path).await?;
    let mut writer = BufWriter::new(temp_file);

    let mut lines_skipped = 0;
    while lines_skipped < count {
        let mut buf = Vec::new();
        if reader.read_until(b'\n', &mut buf).await? == 0 {
            break;
        }
        lines_skipped += 1;
    }

    if let Err(e) = io::copy(&mut reader, &mut writer).await {
        let _ = fs::remove_file(&temp_path).await;
        return Err(e.into());
    }

    writer.flush().await?;
    let temp_file = writer.into_inner();
    temp_file.sync_all().await?;
    drop(temp_file); // Close writer handle
    drop(reader); // Close reader handle

    fs::rename(&temp_path, path).await?;

    // Sync parent directory to ensure rename is durable
    if let Some(parent) = Path::new(path).parent() {
        if let Ok(parent_dir) = File::open(parent).await {
            let _ = parent_dir.sync_all().await;
        }
    }

    Ok(())
}

/// A consumer that reads messages from a file and removes them upon commit.
pub struct FileConsumer {
    inner: EventStoreConsumer,
}

impl FileConsumer {
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        let key = config.path.clone();
        let store = {
            let mut stores = FILE_EVENT_STORES.lock().await;
            // Cleanup expired entries
            stores.retain(|_, v| v.strong_count() > 0);

            if let Some(weak) = stores.get(&key) {
                if let Some(store) = weak.upgrade() {
                    store
                } else {
                    let store = create_file_event_store(config).await?;
                    stores.insert(key, Arc::downgrade(&store));
                    store
                }
            } else {
                let store = create_file_event_store(config).await?;
                stores.insert(key, Arc::downgrade(&store));
                store
            }
        };

        let subscriber_id = format!("file-sub-{}", fast_uuid_v7::gen_id_str());
        info!(path = %config.path, mode = ?if config.subscribe_mode { "subscribe" } else { "consume" }, "File consumer connected");

        Ok(Self {
            inner: store.consumer(subscriber_id),
        })
    }
}

#[async_trait]
impl MessageConsumer for FileConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.inner.receive_batch(max_messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn parse_message(buffer: Vec<u8>) -> CanonicalMessage {
    match serde_json::from_slice::<CanonicalMessage>(&buffer) {
        Ok(msg) => msg,
        Err(_) => {
            let mut msg = CanonicalMessage::new(buffer, None);
            msg.metadata
                .insert("mq_bridge.original_format".to_string(), "raw".to_string());
            msg
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::models::FileConfig;
    use crate::traits::MessageConsumer;
    use crate::traits::MessagePublisher;
    use crate::{
        endpoints::file::{FileConsumer, FilePublisher},
        CanonicalMessage,
    };
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_file_sink_and_source_integration() {
        // 1. Setup a temporary directory and file path
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // 2. Create a FileSink
        let config = FileConfig {
            path: file_path_str.clone(),
            subscribe_mode: false,
            delete: None,
        };
        let sink = FilePublisher::new(&config).await.unwrap();

        let msg1 = CanonicalMessage::from_json(json!({"hello": "world"})).unwrap();
        let msg2 = CanonicalMessage::from_json(json!({"foo": "bar"})).unwrap();

        sink.send_batch(vec![msg1.clone(), msg2.clone()])
            .await
            .unwrap();
        // Explicitly flush to ensure data is written before we try to read it.
        sink.flush().await.unwrap();
        // Drop the sink to release the file lock on some OSes before the source tries to open it.
        drop(sink);

        // 4. Create a FileConsumer to read from the same file
        let mut source = FileConsumer::new(&config).await.unwrap();

        // 5. Receive the messages and verify them
        let received1 = source.receive().await.unwrap();
        let _ = (received1.commit)(crate::traits::MessageDisposition::Ack).await; // Commit is a no-op, but we should call it

        assert_eq!(received1.message.message_id, msg1.message_id);
        assert_eq!(received1.message.payload, msg1.payload);

        let batch = source.receive_batch(1).await.unwrap();
        let (received_msgs, commit2) = (batch.messages, batch.commit);
        let len = received_msgs.len();
        let received_msg2 = received_msgs.into_iter().next().unwrap();
        let _ = commit2(vec![crate::traits::MessageDisposition::Ack; len]).await;
        assert_eq!(received_msg2.message_id, msg2.message_id);
        assert_eq!(received_msg2.payload, msg2.payload);

        // 6. Verify that reading again waits for new data (timeout)
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            source.receive_batch(1),
        )
        .await;
        assert!(result.is_err(), "Expected timeout waiting for new data");
    }

    #[tokio::test]
    async fn test_file_sink_creates_directory() {
        let dir = tempdir().unwrap();
        let nested_dir_path = dir.path().join("nested");
        let file_path = nested_dir_path.join("test.log");

        let config = FileConfig {
            path: file_path.to_str().unwrap().to_string(),
            subscribe_mode: false,
            delete: None,
        };
        let sink_result = FilePublisher::new(&config).await;

        assert!(sink_result.is_ok());
        assert!(nested_dir_path.exists());
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn test_file_consumer_consume_mode() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("consume.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write 3 lines
        tokio::fs::write(&file_path, b"line1\nline2\nline3\n")
            .await
            .unwrap();

        let config = FileConfig {
            path: file_path_str,
            subscribe_mode: false,
            delete: None,
        };
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Receive first message
        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.payload.as_ref(), b"line1");

        // Commit first message (should remove line1)
        (received1.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Verify file content - wait for async deletion
        let mut content = String::new();
        for _ in 0..20 {
            content = tokio::fs::read_to_string(&file_path).await.unwrap();
            if content == "line2\nline3\n" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(content, "line2\nline3\n");

        // Receive second message
        let received2 = consumer.receive().await.unwrap();
        assert_eq!(received2.message.payload.as_ref(), b"line2");
        (received2.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Receive third message
        let received3 = consumer.receive().await.unwrap();
        assert_eq!(received3.message.payload.as_ref(), b"line3");
        (received3.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Verify file is empty
        for _ in 0..20 {
            content = tokio::fs::read_to_string(&file_path).await.unwrap();
            if content.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(content, "");
    }

    #[tokio::test]
    async fn test_file_consumer_nack_behavior() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("nack.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write 2 lines
        tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            subscribe_mode: false,
            delete: None,
        };
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // 1. Receive batch (should get msg1)
        let batch1 = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch1.messages.len(), 1);
        assert_eq!(batch1.messages[0].payload.as_ref(), b"msg1");

        // 2. Nack the batch
        (batch1.commit)(vec![crate::traits::MessageDisposition::Nack])
            .await
            .unwrap();

        // 3. Receive again - should get msg1 again because it wasn't removed
        let batch2 = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch2.messages.len(), 1);
        assert_eq!(batch2.messages[0].payload.as_ref(), b"msg1");

        // 4. Ack
        (batch2.commit)(vec![crate::traits::MessageDisposition::Ack])
            .await
            .unwrap();

        // 5. Receive next - should get msg2
        let batch3 = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch3.messages.len(), 1);
        assert_eq!(batch3.messages[0].payload.as_ref(), b"msg2");
    }

    #[tokio::test]
    async fn test_file_consumer_consume_no_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("consume_no_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write 3 lines
        tokio::fs::write(&file_path, b"line1\nline2\nline3\n")
            .await
            .unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            subscribe_mode: false,
            delete: Some(false),
        };
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Receive first message
        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.payload.as_ref(), b"line1");

        // Commit first message (should NOT remove line1)
        (received1.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Give some time for any potential (but unwanted) background deletion to happen
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify file content remains unchanged
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");

        // Receive second message
        let received2 = consumer.receive().await.unwrap();
        assert_eq!(received2.message.payload.as_ref(), b"line2");
    }

    #[tokio::test]
    async fn test_file_consumer_subscribe_mode() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("subscribe.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write initial content
        tokio::fs::write(&file_path, b"line1\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            subscribe_mode: true,
            delete: None,
        };

        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Receive existing line
        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.payload.as_ref(), b"line1");
        (received1.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Append new line
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&file_path)
                .await
                .unwrap();
            file.write_all(b"line2\n").await.unwrap();
        }

        // Receive new line
        let received2 = consumer.receive().await.unwrap();
        assert_eq!(received2.message.payload.as_ref(), b"line2");
        (received2.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Verify file content is unchanged
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line1\nline2\n");
    }
}
