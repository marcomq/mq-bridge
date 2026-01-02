//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::traits::{
    into_batch_commit_func, BoxFuture, ConsumerError, MessageConsumer, MessagePublisher,
    PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, trace};

/// A sink that writes messages to a file, one per line.
#[derive(Clone)]
pub struct FilePublisher {
    writer: Arc<Mutex<BufWriter<File>>>,
    path: String,
}

impl FilePublisher {
    pub async fn new(path_str: &str) -> anyhow::Result<Self> {
        let path = Path::new(&path_str);
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
            .with_context(|| format!("Failed to open or create file for writing: {}", path_str))?;

        info!(path = %path_str, "File sink opened for appending");
        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
            path: path_str.to_string(),
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

/// A source that reads messages from a file, one line at a time.
pub struct FileConsumer {
    path: String,
    reader: BufReader<File>,
}

impl FileConsumer {
    pub async fn new(path: &str) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .with_context(|| format!("Failed to open file for reading: {}", path))?;

        info!(path = %path, "File source opened for reading");
        Ok(Self {
            reader: BufReader::new(file),
            path: path.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for FileConsumer {
    #[instrument(skip(self), fields(path = %self.path), err(level = "debug"))]
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let mut buffer = Vec::new();

        let bytes_read = self
            .reader
            .read_until(b'\n', &mut buffer)
            .await
            .context("Failed to read from file source")?;
        if bytes_read == 0 {
            debug!("End of file reached, consumer will stop.");
            return Err(ConsumerError::EndOfStream);
        }

        if buffer.ends_with(b"\n") {
            buffer.pop();
        }

        let message = match serde_json::from_slice::<CanonicalMessage>(&buffer) {
            Ok(msg) => msg,
            Err(_) => {
                let mut msg = CanonicalMessage::new(buffer, None);
                msg.metadata
                    .insert("mq_bridge.original_format".to_string(), "raw".to_string());
                msg
            }
        };

        // The commit for a file source is a no-op.
        let commit = Box::new(move |_| Box::pin(async move {}) as BoxFuture<'static, ()>);

        trace!(message_id = %format!("{:032x}", message.message_id), path = %self.path, "Received message from file");
        Ok(ReceivedBatch {
            messages: vec![message],
            commit: into_batch_commit_func(commit),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::traits::ConsumerError;
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
        let sink = FilePublisher::new(&file_path_str).await.unwrap();

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
        let mut source = FileConsumer::new(&file_path_str).await.unwrap();

        // 5. Receive the messages and verify them
        let received1 = source.receive().await.unwrap();
        (received1.commit)(None).await; // Commit is a no-op, but we should call it

        assert_eq!(received1.message.message_id, msg1.message_id);
        assert_eq!(received1.message.payload, msg1.payload);

        let batch = source.receive_batch(1).await.unwrap();
        let (received_msgs, commit2) = (batch.messages, batch.commit);
        let received_msg2 = received_msgs.into_iter().next().unwrap();
        commit2(None).await;
        assert_eq!(received_msg2.message_id, msg2.message_id);
        assert_eq!(received_msg2.payload, msg2.payload);

        // 6. Verify that reading again results in EOF
        let eof_result = source.receive_batch(1).await;
        match eof_result {
            Ok(_) => panic!("Expected an eof error, but got Ok"),
            Err(ConsumerError::EndOfStream) => (),
            Err(e) => panic!("Expected an eof error, but got {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_file_sink_creates_directory() {
        let dir = tempdir().unwrap();
        let nested_dir_path = dir.path().join("nested");
        let file_path = nested_dir_path.join("test.log");

        let sink_result = FilePublisher::new(file_path.to_str().unwrap()).await;

        assert!(sink_result.is_ok());
        assert!(nested_dir_path.exists());
        assert!(file_path.exists());
    }
}
