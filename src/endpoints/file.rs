//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::FileConfig;
use crate::traits::{
    into_batch_commit_func, BatchCommitFunc, CommitFunc, ConsumerError, MessageConsumer, MessagePublisher,
    PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{self, AsyncBufReadExt, BufReader};
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

struct FileConsumerState {
    in_flight_lines: usize,
    next_batch_seq: u64,
    next_commit_seq: u64,
    pending_commits: std::collections::BTreeMap<u64, usize>,
}

enum ConsumerMode {
    Consume(Arc<Mutex<FileConsumerState>>),
    Subscribe(BufReader<File>),
}

/// A consumer that reads messages from a file and removes them upon commit.
pub struct FileConsumer {
    path: String,
    mode: ConsumerMode,
}

impl FileConsumer {
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        if config.subscribe_mode {
            let file = OpenOptions::new()
                .read(true)
                .open(&config.path)
                .await
                .with_context(|| format!("Failed to open file for reading: {}", config.path))?;

            info!(path = %config.path, "File consumer opened in subscribe (tail) mode");
            Ok(Self {
                path: config.path.clone(),
                mode: ConsumerMode::Subscribe(BufReader::new(file)),
            })
        } else {
            info!(path = %config.path, "File consumer opened in consume (delete) mode");
            Ok(Self {
                path: config.path.clone(),
                mode: ConsumerMode::Consume(Arc::new(Mutex::new(FileConsumerState {
                    in_flight_lines: 0,
                    next_batch_seq: 0,
                    next_commit_seq: 0,
                    pending_commits: std::collections::BTreeMap::new(),
                }))),
            })
        }
    }
}

#[async_trait]
impl MessageConsumer for FileConsumer {
    #[instrument(skip(self), fields(path = %self.path), err(level = "debug"))]
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        match &mut self.mode {
            ConsumerMode::Subscribe(reader) => {
                let mut messages = Vec::new();

                // 1. Wait for the first message
                loop {
                    let mut buffer = Vec::new();
                    let bytes_read = reader
                        .read_until(b'\n', &mut buffer)
                        .await
                        .context("Failed to read from file source")?;

                    if bytes_read > 0 {
                        if buffer.ends_with(b"\n") {
                            buffer.pop();
                        }
                        messages.push(parse_message(buffer));
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                // 2. Try to read more messages to fill the batch
                while messages.len() < max_messages {
                    let mut buffer = Vec::new();
                    let bytes_read = reader
                        .read_until(b'\n', &mut buffer)
                        .await
                        .context("Failed to read from file source")?;

                    if bytes_read == 0 {
                        break;
                    }
                    if buffer.ends_with(b"\n") {
                        buffer.pop();
                    }
                    messages.push(parse_message(buffer));
                }

                let commit: CommitFunc = Box::new(move |_| Box::pin(async move { Ok(()) }));

                Ok(ReceivedBatch {
                    messages,
                    commit: into_batch_commit_func(commit),
                })
            }
            ConsumerMode::Consume(state) => {
                let path = self.path.clone();
                let state = state.clone();
                Self::receive_batch_consume(path, state, max_messages).await
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileConsumer {
    async fn receive_batch_consume(
        // Changed from &self to associated function
        path: String,
        state: Arc<Mutex<FileConsumerState>>,
        max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = Vec::new();
        let mut lines_read = 0;
        let batch_seq;

        // 1. Wait for the first message
        loop {
            let mut state = state.lock().await;
            let file = OpenOptions::new()
                .read(true)
                .open(&path)
                .await
                .context("Failed to open file for reading")?;
            let mut reader = BufReader::new(file);
            let mut current_line = 0;

            // Skip lines that are currently being processed
            while current_line < state.in_flight_lines {
                let mut skip_buf = Vec::new();
                if reader
                    .read_until(b'\n', &mut skip_buf)
                    .await
                    .context("Failed to skip in-flight line")?
                    == 0
                {
                    break;
                }
                current_line += 1;
            }

            // Try to read the first message
            let mut buffer = Vec::new();
            let n = reader
                .read_until(b'\n', &mut buffer)
                .await
                .context("Failed to read next line")?;

            if n > 0 {
                lines_read += 1;
                if buffer.ends_with(b"\n") {
                    buffer.pop();
                }
                messages.push(parse_message(buffer));

                // 2. Try to read more messages to fill the batch
                while messages.len() < max_messages {
                    let mut buffer = Vec::new();
                    let n = reader
                        .read_until(b'\n', &mut buffer)
                        .await
                        .context("Failed to read next line")?;
                    if n == 0 {
                        break;
                    }
                    lines_read += 1;
                    if buffer.ends_with(b"\n") {
                        buffer.pop();
                    }
                    messages.push(parse_message(buffer));
                }

                batch_seq = state.next_batch_seq;
                state.next_batch_seq += 1;
                state.in_flight_lines += lines_read;
                drop(state);
                break;
            }

            // Keep lock held while sleeping to prevent concurrent readers
            // from reading overlapping lines
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // In Consume mode, we remove the processed lines from the file.
        let commit_path = path.clone();
        let lines_to_remove = lines_read;

        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<crate::traits::MessageDisposition>| {
            Box::pin(async move {
                if dispositions
                    .iter()
                    .any(|d| matches!(d, crate::traits::MessageDisposition::Nack))
                {
                    let mut state = state.lock().await;
                    state.in_flight_lines =
                        state.in_flight_lines.saturating_sub(lines_to_remove);
                    return Ok(());
                }

                let mut state = state.lock().await;
                state.pending_commits.insert(batch_seq, lines_to_remove);

                // Check if we can process any commits in order
                let mut total_lines_to_remove = 0;
                let mut batches_to_process: u64 = 0;
                let mut current_seq = state.next_commit_seq;

                while let Some(&lines) = state.pending_commits.get(&current_seq) {
                    total_lines_to_remove += lines;
                    batches_to_process += 1;
                    current_seq += 1;
                }

                if batches_to_process == 0 {
                    return Ok(());
                }

                // Read the whole file
                let file = File::open(&commit_path)
                    .await
                    .context("Failed to read file for commit")?;
                let mut reader = BufReader::new(file);

                let temp_path = format!("{}.tmp", commit_path);
                let result = async {
                    let temp_file = File::create(&temp_path)
                        .await
                        .context("Failed to create temp file")?;
                    let mut writer = BufWriter::new(temp_file);

                    let mut lines_skipped = 0;

                    // Skip the first N lines (where N is the batch size we just processed)
                    // Note: This simple implementation assumes ordered commits.
                    while lines_skipped < total_lines_to_remove {
                        let mut skip_buf = Vec::new();
                        if reader.read_until(b'\n', &mut skip_buf).await? == 0 {
                            break;
                        }
                        lines_skipped += 1;
                    }

                    // Write the rest using streaming copy
                    io::copy(&mut reader, &mut writer).await?;
                    writer.flush().await?;

                    // Rewrite the file
                    fs::rename(&temp_path, &commit_path)
                        .await
                        .context("Failed to replace file for commit")?;
                    Ok(())
                }
                .await;

                if let Err(e) = result {
                    let _ = fs::remove_file(&temp_path).await;
                    return Err(e);
                }

                // Update state
                state.in_flight_lines = state.in_flight_lines.saturating_sub(total_lines_to_remove);
                let start_seq = state.next_commit_seq;
                state.next_commit_seq += batches_to_process;
                for i in 0..batches_to_process {
                    state.pending_commits.remove(&(start_seq + i));
                }
                Ok(())
            })
        });

        if let Some(first) = messages.first() {
            trace!(message_id = %format!("{:032x}", first.message_id), path = %path, "Received message from file");
        }
        Ok(ReceivedBatch {
            messages,
            commit,
        })
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
        };
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Receive first message
        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.payload.as_ref(), b"line1");

        // Commit first message (should remove line1)
        (received1.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Verify file content
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
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
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "");
    }

    #[tokio::test]
    async fn test_file_consumer_nack_behavior() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("nack.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write 2 lines
        tokio::fs::write(&file_path, b"msg1\nmsg2\n")
            .await
            .unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            subscribe_mode: false,
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
}
