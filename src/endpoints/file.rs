//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue
use crate::traits::{into_batch_commit_func, BatchCommitFunc};
use crate::traits::{BoxFuture, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{info, instrument};

/// A sink that writes messages to a file, one per line.
#[derive(Clone)]
pub struct FilePublisher {
    writer: Arc<Mutex<BufWriter<File>>>,
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
        })
    }
}

#[async_trait]
impl MessagePublisher for FilePublisher {
    #[instrument(skip_all, fields(message_id = ?message.message_id))]
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let mut writer = self.writer.lock().await;
        // Since Bytes is immutable, we write the payload and the newline separately.
        // The BufWriter will handle this efficiently.
        writer.write_all(&message.payload).await?;
        writer.write_all(b"\n").await?;
        Ok(None)
    }

    // just using normal send for simplicity - it is fast enough
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
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
    #[instrument(skip(self), fields(path = %self.path), err(level = "info"))]
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let mut buffer = Vec::new();

        let bytes_read = self.reader.read_until(b'\n', &mut buffer).await?;
        if bytes_read == 0 {
            info!("End of file reached, consumer will stop.");
            return Err(anyhow!("End of file reached: {}", self.path));
        }

        // Trim the newline character that read_until includes
        if buffer.ends_with(b"\n") {
            buffer.pop();
        }
        let message = CanonicalMessage::new(buffer);

        // The commit for a file source is a no-op.
        let commit = Box::new(move |_| Box::pin(async move {}) as BoxFuture<'static, ()>);

        Ok((vec![message], into_batch_commit_func(commit)))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
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
        let (received_msg1, commit1) = source.receive().await.unwrap();
        commit1(None).await; // Commit is a no-op, but we should call it
        assert_eq!(received_msg1.message_id, msg1.message_id);
        assert_eq!(received_msg1.payload, msg1.payload);

        let (received_msgs, commit2) = source.receive_batch(1).await.unwrap();
        let received_msg2 = received_msgs.into_iter().next().unwrap();
        commit2(None).await;
        assert_eq!(received_msg2.message_id, msg2.message_id);
        assert_eq!(received_msg2.payload, msg2.payload);

        // 6. Verify that reading again results in EOF
        let eof_result = source.receive_batch(1).await;
        match eof_result {
            Ok(_) => panic!("Expected an error, but got Ok"),
            Err(e) => assert!(e.to_string().contains("End of file reached")),
        }
    }

    #[tokio::test]
    async fn test_file_sink_creates_directory() {
        let dir = tempdir().unwrap();
        let nested_dir_path = dir.path().join("nested");
        let file_path = nested_dir_path.join("test.log");

        let sink_result = FilePublisher::new(&file_path.to_str().unwrap()).await;

        assert!(sink_result.is_ok());
        assert!(nested_dir_path.exists());
        assert!(file_path.exists());
    }
}
