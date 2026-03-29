//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::event_store::{EventStore, EventStoreConsumer, RetentionPolicy};
use crate::models::{FileConfig, FileConsumerMode, FileFormat};
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::io::Seek;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{self, AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{info, instrument, trace, warn};

/// A sink that writes messages to a file, one per line.
static FILE_LOCKS: Lazy<StdMutex<HashMap<String, Arc<Mutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

fn get_file_lock(path: &str) -> Arc<Mutex<()>> {
    let mut locks = FILE_LOCKS.lock().unwrap();
    locks.retain(|_, v| Arc::strong_count(v) > 1);
    locks
        .entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn parse_delimiter(delimiter: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let bytes = match delimiter {
        Some(s) if s.starts_with("0x") => {
            let hex = s.trim_start_matches("0x");
            if hex.len() != 2 {
                return Err(anyhow::anyhow!(
                    "Hex delimiter must be 1 byte (2 hex chars)"
                ));
            }
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|e| anyhow::anyhow!("Invalid hex delimiter: {}", e))?
        }
        Some(s) => s.as_bytes().to_vec(),
        None => vec![b'\n'],
    };

    if bytes.is_empty() {
        return Err(anyhow::anyhow!("Delimiter cannot be empty"));
    }

    Ok(bytes)
}

async fn read_until_bytes<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    delimiter: &[u8],
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    if delimiter.len() == 1 {
        return reader.read_until(delimiter[0], buf).await;
    }
    let last_byte = delimiter[delimiter.len() - 1];
    let mut total_read = 0;
    loop {
        let n = reader.read_until(last_byte, buf).await?;
        if n == 0 {
            return Ok(total_read);
        }
        total_read += n;
        if buf.len() >= delimiter.len() && &buf[buf.len() - delimiter.len()..] == delimiter {
            return Ok(total_read);
        }
    }
}

#[derive(Clone)]
pub struct FilePublisher {
    path: String,
    file_lock: Arc<Mutex<()>>,
    delimiter: Vec<u8>,
    format: FileFormat,
}

impl FilePublisher {
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        let path_str = &config.path;
        let path = Path::new(path_str);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create parent directory for file: {:?}", parent)
            })?;
        }

        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("Failed to open or create file for writing: {}", path_str))?;

        let file_lock = get_file_lock(path_str);
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let format = config.format.clone();

        info!(path = %path_str, format = ?format, "File sink opened for appending");
        Ok(Self {
            path: path_str.to_string(),
            file_lock,
            delimiter,
            format,
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
        let _file_guard = self.file_lock.lock().await;

        // We open the file for every batch to ensure we are writing to the current file path.
        // This handles external file rotation/deletion (e.g. by the consumer in delete mode)
        // where the old file handle would point to a deleted inode.
        // While this has a performance cost, it ensures correctness.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .context("Failed to open file for writing batch")?;

        let mut writer = BufWriter::new(file);
        let mut failed_messages = Vec::new();

        // Iterate over messages, consuming them
        for msg in messages {
            let serialized_msg = match self.format {
                FileFormat::Raw => Ok(msg.payload.to_vec()),
                FileFormat::Normal => {
                    if msg
                        .metadata
                        .get("mq_bridge.original_format")
                        .map(|s| s.as_str())
                        == Some("raw")
                    {
                        // If the message was originally raw, pass its payload through directly
                        // to support raw file-to-file copies without re-wrapping.
                        Ok(msg.payload.to_vec())
                    } else {
                        serde_json::to_vec(&msg)
                    }
                }
                FileFormat::Json => {
                    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                    {
                        #[derive(serde::Serialize)]
                        struct JsonWrapper<'a> {
                            message_id: u128,
                            payload: serde_json::Value,
                            metadata: &'a HashMap<String, String>,
                        }
                        serde_json::to_vec(&JsonWrapper {
                            message_id: msg.message_id,
                            payload: json_val,
                            metadata: &msg.metadata,
                        })
                    } else {
                        serde_json::to_vec(&msg)
                    }
                }
                FileFormat::Text => {
                    if let Ok(text) = std::str::from_utf8(&msg.payload) {
                        #[derive(serde::Serialize)]
                        struct TextWrapper<'a> {
                            message_id: u128,
                            payload: &'a str,
                            metadata: &'a HashMap<String, String>,
                        }
                        serde_json::to_vec(&TextWrapper {
                            message_id: msg.message_id,
                            payload: text,
                            metadata: &msg.metadata,
                        })
                    } else {
                        serde_json::to_vec(&msg)
                    }
                }
            };
            let serialized_msg = match serialized_msg {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to serialize message for file sink: {}", e);
                    failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
                    continue;
                }
            };

            if let Err(e) = writer.write_all(&serialized_msg).await {
                tracing::error!("Failed to write message to file: {}", e);
                failed_messages.push((msg, PublisherError::NonRetryable(anyhow::anyhow!(e))));
            } else if let Err(e) = writer.write_all(&self.delimiter).await {
                tracing::error!("Failed to write delimiter to file: {}", e);
                return Err(PublisherError::NonRetryable(anyhow::anyhow!(e)));
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
    lines_in_memory: usize,
}

/// Creates an EventStore backed by a file.
/// The EventStore acts as an in-memory buffer for the file content, allowing unified handling of Consume and Subscribe modes.
async fn create_file_event_store(
    path: &str,
    delimiter: Vec<u8>,
    format: FileFormat,
) -> anyhow::Result<Arc<EventStore>> {
    let path = path.to_string();
    // Shared state to coordinate the reader and the drop (delete) logic.
    let feed_state = Arc::new(Mutex::new(FileFeedState { lines_in_memory: 0 }));

    // Lock to serialize file modification operations
    let file_op_lock = get_file_lock(&path);

    let feed_state_clone = feed_state.clone();
    let path_clone = path.clone();
    let file_op_lock_clone = file_op_lock.clone();
    let delimiter_clone = delimiter.clone();

    let retention = RetentionPolicy {
        gc_interval: std::time::Duration::ZERO,
        ..Default::default()
    };
    // Use immediate GC for file stores to ensure files are truncated promptly on ack.

    // 1. Create EventStore with on_drop callback
    let store = Arc::new(
        EventStore::new(retention).with_drop_callback(move |events| {
            // In EventStore mode (Subscribe + Delete), we always delete.
            let count = events.len();
            if count == 0 {
                return;
            }
            let state = feed_state_clone.clone();
            let path = path_clone.clone();
            let file_op_lock = file_op_lock_clone.clone();
            let delimiter = delimiter_clone.clone();

            tokio::spawn(async move {
                // Serialize file operations to prevent race conditions between multiple GCs
                let _guard = file_op_lock.lock().await;

                {
                    let mut s = state.lock().await;
                    s.lines_in_memory = s.lines_in_memory.saturating_sub(count);
                }

                if let Err(e) = remove_lines_from_file(&path, count, &delimiter).await {
                    tracing::error!("Failed to remove lines from file {}: {}", path, e);
                    // Note: In this simplified model, if deletion fails, lines_in_memory
                    // might become out of sync, leading to reprocessing on restart.
                } else {
                    trace!("Removed {} lines from {}", count, path);
                }
            });
        }),
    );

    // 2. Spawn background reader task
    let store_weak = Arc::downgrade(&store);
    let path_clone = path.clone();
    let feed_state_clone = feed_state.clone();
    let file_op_lock_clone = file_op_lock.clone();
    let format_clone = format;

    tokio::spawn(async move {
        let mut current_sleep = std::time::Duration::from_millis(1);
        const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);

        loop {
            // Check if the store is still alive
            let store_clone = match store_weak.upgrade() {
                Some(s) => s,
                None => break, // Exit if EventStore is dropped
            };

            // Acquire file op lock first to coordinate with GC
            let file_guard = Some(file_op_lock_clone.lock().await);

            let mut state = feed_state_clone.lock().await;

            // Open file
            let file_res = OpenOptions::new().read(true).open(&path_clone).await;
            let mut file = match file_res {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open file {}: {}", path_clone, e);
                    drop(state);
                    drop(file_guard);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Position the reader
            // In consume mode, we skip lines that are already buffered in memory
            // because they are still in the file (until dropped).
            let mut reader = BufReader::new(file);
            let mut lines_skipped = 0;
            let mut error = false;
            let lines_to_skip = state.lines_in_memory;
            while lines_skipped < lines_to_skip {
                let mut buf = Vec::new();
                match read_until_bytes(&mut reader, &delimiter, &mut buf).await {
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
                drop(file_guard);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            file = reader.into_inner();

            // Release file op lock to allow publisher to write while we read
            drop(file_guard);

            // Read new lines
            let mut reader = BufReader::new(file);
            let mut lines_read = 0;
            let mut batch = Vec::with_capacity(128);

            loop {
                let mut buffer = Vec::new();
                match read_until_bytes(&mut reader, &delimiter, &mut buffer).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if buffer.ends_with(&delimiter) {
                            buffer.truncate(buffer.len() - delimiter.len());
                        }
                        if delimiter.len() == 1 && delimiter[0] == b'\n' && buffer.ends_with(b"\r")
                        {
                            buffer.pop();
                        }
                        let msg = parse_message(&buffer, &format_clone);
                        batch.push(msg);
                        lines_read += 1;

                        state.lines_in_memory += 1;

                        if batch.len() >= 128 {
                            store_clone.append_batch(std::mem::take(&mut batch)).await;
                            batch.reserve(128);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error reading from {}: {}", path_clone, e);
                        break;
                    }
                }
            }

            if !batch.is_empty() {
                store_clone.append_batch(batch).await;
            }

            drop(state); // Release lock before sleeping

            // If we didn't read anything, sleep a bit (polling)
            if lines_read == 0 {
                tokio::time::sleep(current_sleep).await;
                current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
            } else {
                current_sleep = std::time::Duration::from_millis(1);
            }
        }
    });

    Ok(store)
}

async fn remove_lines_from_file(path: &str, count: usize, delimiter: &[u8]) -> anyhow::Result<()> {
    let unique_id = fast_uuid_v7::gen_id_str();
    let temp_path = format!("{}.{}.tmp", path, unique_id);

    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let temp_file = File::create(&temp_path).await?;
    let mut writer = BufWriter::new(temp_file);

    let mut lines_skipped = 0;
    while lines_skipped < count {
        let mut buf = Vec::new();
        if read_until_bytes(&mut reader, delimiter, &mut buf).await? == 0 {
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

struct FileTailConsumer {
    msg_rx: async_channel::Receiver<Vec<CanonicalMessage>>,
    buffer: Vec<CanonicalMessage>,
    offset_file: Option<Arc<Mutex<tokio::fs::File>>>,
}

fn read_until_bytes_sync<R: std::io::BufRead>(
    reader: &mut R,
    delimiter: &[u8],
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    if delimiter.len() == 1 {
        return reader.read_until(delimiter[0], buf);
    }
    let last_byte = delimiter[delimiter.len() - 1];
    let mut total_read = 0;
    loop {
        let n = reader.read_until(last_byte, buf)?;
        if n == 0 {
            return Ok(total_read);
        }
        total_read += n;
        if buf.len() >= delimiter.len() && &buf[buf.len() - delimiter.len()..] == delimiter {
            return Ok(total_read);
        }
    }
}

fn run_file_tail_task_sync(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    initial_offset: u64,
    group_id: Option<String>,
    delimiter: Vec<u8>,
    format: FileFormat,
) {
    let mut last_position: u64 = initial_offset;
    let mut reader: Option<std::io::BufReader<std::fs::File>> = None;
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(50);
    const BATCH_SIZE: usize = 1024;
    let mut buf = Vec::with_capacity(1024);

    loop {
        if reader.is_none() {
            let mut file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            if let Ok(metadata) = file.metadata() {
                if metadata.len() < last_position {
                    tracing::warn!("File {} was truncated. Resetting position to 0.", path);
                    last_position = 0;
                }
            }

            if let Err(e) = file.seek(std::io::SeekFrom::Start(last_position)) {
                tracing::error!("Failed to seek in {}: {}", path, e);
                last_position = 0; // Reset on seek failure
                if let Err(e) = file.seek(std::io::SeekFrom::Start(0)) {
                    tracing::error!("Failed to reset seek to 0 in {}: {}", path, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }

            reader = Some(std::io::BufReader::with_capacity(128 * BATCH_SIZE, file));
        }

        let mut batch = Vec::with_capacity(BATCH_SIZE);
        let mut lines_read_in_batch = 0;

        if let Some(r) = reader.as_mut() {
            for _ in 0..BATCH_SIZE {
                buf.clear();
                match read_until_bytes_sync(r, &delimiter, &mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        last_position += n as u64;
                        if buf.ends_with(&delimiter) {
                            buf.truncate(buf.len() - delimiter.len());
                        }
                        if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r") {
                            buf.pop();
                        }
                        let mut msg = parse_message(&buf, &format);
                        if group_id.is_some() {
                            msg.metadata
                                .insert("file_offset".to_string(), last_position.to_string());
                        }
                        batch.push(msg);
                        lines_read_in_batch += 1;
                    }
                    Err(e) => {
                        tracing::error!("Error reading {}: {}", path, e);
                        reader = None; // Force reopen on next loop
                        break;
                    }
                }
            }
        }

        if !batch.is_empty() {
            if msg_tx.send_blocking(batch).is_err() {
                break; // Consumer dropped, exit thread
            }
            current_sleep = std::time::Duration::from_millis(1);
        }

        if lines_read_in_batch == 0 {
            // EOF reached, sleep
            std::thread::sleep(current_sleep);
            current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
            // Invalidate reader to check for file changes (like rotation) on next poll
            reader = None;
        }
    }
}

struct FileQueueConsumer {
    msg_rx: async_channel::Receiver<Vec<CanonicalMessage>>,
    lines_in_memory: Arc<AtomicUsize>,
    path: String,
    file_lock: Arc<Mutex<()>>,
    buffer: Arc<Mutex<Vec<CanonicalMessage>>>,
    delimiter: Vec<u8>,
}

fn run_file_queue_task(
    path: String,
    msg_tx: async_channel::Sender<Vec<CanonicalMessage>>,
    lines_in_memory: Arc<AtomicUsize>,
    file_lock: Arc<Mutex<()>>,
    runtime_handle: tokio::runtime::Handle,
    delimiter: Vec<u8>,
    format: FileFormat,
) {
    let mut current_sleep = std::time::Duration::from_millis(1);
    const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
    let mut buf = Vec::new();

    loop {
        buf.clear();
        let mut batch = Vec::with_capacity(128);
        let mut lines_read = 0;

        {
            let _guard = runtime_handle.block_on(file_lock.lock());
            let skip_count = lines_in_memory.load(Ordering::SeqCst);

            let file = match std::fs::OpenOptions::new().read(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", path, e);
                    drop(_guard);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let mut reader = std::io::BufReader::new(file);
            let mut skipped = 0;
            let mut error = false;

            while skipped < skip_count {
                buf.clear();
                match read_until_bytes_sync(&mut reader, &delimiter, &mut buf) {
                    Ok(0) => break,
                    Ok(_) => skipped += 1,
                    Err(e) => {
                        tracing::error!("Error skipping lines in {}: {}", path, e);
                        error = true;
                        break;
                    }
                }
            }

            if !error {
                for _ in 0..128 {
                    buf.clear();
                    match read_until_bytes_sync(&mut reader, &delimiter, &mut buf) {
                        Ok(0) => break,
                        Ok(_) => {
                            if buf.ends_with(&delimiter) {
                                buf.truncate(buf.len() - delimiter.len());
                            }
                            if delimiter.len() == 1 && delimiter[0] == b'\n' && buf.ends_with(b"\r")
                            {
                                buf.pop();
                            }
                            batch.push(parse_message(&buf, &format));
                            lines_read += 1;
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        if lines_read > 0 {
            lines_in_memory.fetch_add(lines_read, Ordering::SeqCst);
            if msg_tx.send_blocking(batch).is_err() {
                break;
            }
            current_sleep = std::time::Duration::from_millis(1);
        } else {
            std::thread::sleep(current_sleep);
            current_sleep = std::cmp::min(current_sleep * 2, MAX_SLEEP);
        }
    }
}

enum ConsumerBackend {
    EventStore(EventStoreConsumer),
    Tail(FileTailConsumer),
    Queue(FileQueueConsumer),
}

/// A consumer that reads messages from a file and removes them upon commit.
pub struct FileConsumer {
    backend: ConsumerBackend,
}

impl FileConsumer {
    pub async fn new(config: &FileConfig) -> anyhow::Result<Self> {
        let delimiter = parse_delimiter(config.delimiter.as_deref())?;
        let format = config.format.clone();
        match &config.mode {
            None | Some(FileConsumerMode::Consume { delete: false }) => {
                Self::new_tail(&config.path, false, None, delimiter.clone(), format).await
            }
            Some(FileConsumerMode::Subscribe { delete: false }) => {
                Self::new_tail(&config.path, true, None, delimiter.clone(), format).await
            }
            Some(FileConsumerMode::GroupSubscribe {
                group_id,
                read_from_tail,
            }) => {
                let start_at_end = *read_from_tail;
                Self::new_tail(
                    &config.path,
                    start_at_end,
                    Some(group_id.clone()),
                    delimiter.clone(),
                    format,
                )
                .await
            }
            Some(FileConsumerMode::Consume { delete: true }) => {
                let (msg_tx, msg_rx) = async_channel::bounded(100);
                let file_lock = get_file_lock(&config.path);
                let lines_in_memory = Arc::new(AtomicUsize::new(0));
                let lines_clone = lines_in_memory.clone();
                let lock_clone = file_lock.clone();
                let runtime = tokio::runtime::Handle::current();
                let path_clone = config.path.clone();

                let delimiter_clone = delimiter.clone();
                let format_clone = format.clone();
                std::thread::spawn(move || {
                    run_file_queue_task(
                        path_clone,
                        msg_tx,
                        lines_clone,
                        lock_clone,
                        runtime,
                        delimiter_clone,
                        format_clone,
                    );
                });

                info!(path = %config.path, mode = "queue (delete, optimized)", "File consumer connected");
                Ok(Self {
                    backend: ConsumerBackend::Queue(FileQueueConsumer {
                        msg_rx,
                        lines_in_memory,
                        path: config.path.clone(),
                        file_lock,
                        buffer: Arc::new(Mutex::new(Vec::new())),
                        delimiter,
                    }),
                })
            }
            Some(FileConsumerMode::Subscribe { delete: true }) => {
                let key = format!(
                    "{}|subscribe|delete|{:?}|{:?}",
                    config.path, format, delimiter
                );

                let store = if let Some(store) = {
                    let mut stores = FILE_EVENT_STORES.lock().await;
                    stores.retain(|_, v| v.strong_count() > 0);
                    stores.get(&key).and_then(|w| w.upgrade())
                } {
                    store
                } else {
                    let created =
                        create_file_event_store(&config.path, delimiter.clone(), format).await?;
                    let mut stores = FILE_EVENT_STORES.lock().await;
                    let store = stores
                        .get(&key)
                        .and_then(|w| w.upgrade())
                        .unwrap_or_else(|| {
                            stores.insert(key.clone(), Arc::downgrade(&created));
                            created
                        });
                    store
                };

                let subscriber_id = format!("file-sub-{}", fast_uuid_v7::gen_id_str());
                info!(path = %config.path, mode = "subscribe (delete)", subscriber_id = %subscriber_id, "File consumer connected");

                Ok(Self {
                    backend: ConsumerBackend::EventStore(store.consumer(subscriber_id)),
                })
            }
        }
    }

    async fn new_tail(
        path: &str,
        start_at_end: bool,
        group_id: Option<String>,
        delimiter: Vec<u8>,
        format: FileFormat,
    ) -> anyhow::Result<Self> {
        let (msg_tx, msg_rx) = async_channel::bounded(100);
        let mut initial_offset = 0;
        let mut offset_file = None;

        if let Some(gid) = &group_id {
            let offset_path = format!("{}.{}.offset", path, gid);
            if let Ok(content) = tokio::fs::read_to_string(&offset_path).await {
                if let Ok(pos) = content.trim().parse::<u64>() {
                    initial_offset = pos;
                    info!(
                        "Restored offset {} for group {} from {}",
                        pos, gid, offset_path
                    );
                }
            }
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&offset_path)
                .await?;
            offset_file = Some(Arc::new(Mutex::new(file)));
        }

        if initial_offset == 0 && start_at_end {
            if let Ok(metadata) = tokio::fs::metadata(path).await {
                initial_offset = metadata.len();
            }
        }

        let path_clone = path.to_string();
        let format_clone = format;
        std::thread::spawn(move || {
            run_file_tail_task_sync(
                path_clone,
                msg_tx,
                initial_offset,
                group_id,
                delimiter,
                format_clone,
            );
        });

        info!(path = %path, mode = "tail (no-delete, optimized)", "File consumer connected");

        Ok(Self {
            backend: ConsumerBackend::Tail(FileTailConsumer {
                msg_rx,
                buffer: Vec::new(),
                offset_file,
            }),
        })
    }
}

#[async_trait]
impl MessageConsumer for FileConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        match &mut self.backend {
            ConsumerBackend::EventStore(c) => c.receive_batch(max_messages).await,
            ConsumerBackend::Tail(c) => {
                if c.buffer.is_empty() {
                    match c.msg_rx.recv().await {
                        Ok(batch) => c.buffer = batch,
                        Err(_) => return Err(ConsumerError::EndOfStream),
                    }
                }

                // Greedily fill buffer from channel if more messages are available
                while c.buffer.len() < max_messages {
                    match c.msg_rx.try_recv() {
                        Ok(mut next_batch) => c.buffer.append(&mut next_batch),
                        Err(_) => break, // Channel is empty or disconnected
                    }
                }

                let count = std::cmp::min(c.buffer.len(), max_messages);
                let messages: Vec<_> = c.buffer.drain(0..count).collect();

                let commit: crate::traits::BatchCommitFunc = if let Some(offset_file) =
                    &c.offset_file
                {
                    let offset_file = offset_file.clone();
                    let offsets: Vec<Option<u64>> = messages
                        .iter()
                        .map(|m| {
                            m.metadata
                                .get("file_offset")
                                .and_then(|s| s.parse::<u64>().ok())
                        })
                        .collect();

                    Box::new(
                        move |dispositions: Vec<crate::traits::MessageDisposition>| {
                            let offset_file = offset_file.clone();
                            let offsets = offsets.clone();
                            Box::pin(async move {
                                let max_offset = dispositions
                                    .iter()
                                    .zip(offsets.iter())
                                    .filter_map(|(d, offset)| match d {
                                        crate::traits::MessageDisposition::Ack
                                        | crate::traits::MessageDisposition::Reply(_) => *offset,
                                        _ => None,
                                    })
                                    .max();

                                if let Some(offset) = max_offset {
                                    let mut file = offset_file.lock().await;
                                    if let Err(e) = file.rewind().await {
                                        tracing::error!("Failed to rewind offset file: {}", e);
                                    } else if let Err(e) = file.set_len(0).await {
                                        tracing::error!("Failed to truncate offset file: {}", e);
                                    } else if let Err(e) =
                                        file.write_all(offset.to_string().as_bytes()).await
                                    {
                                        tracing::error!("Failed to write offset file: {}", e);
                                    } else if let Err(e) = file.flush().await {
                                        tracing::error!("Failed to flush offset file: {}", e);
                                    }
                                }
                                Ok(())
                            })
                                as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                        },
                    )
                } else {
                    // No-op commit since we are not deleting and no group_id to track
                    Box::new(|_dispositions: Vec<crate::traits::MessageDisposition>| {
                        Box::pin(async move { Ok(()) })
                            as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                    })
                };

                Ok(ReceivedBatch { messages, commit })
            }
            ConsumerBackend::Queue(c) => {
                {
                    let buffer = c.buffer.lock().await;
                    if buffer.is_empty() {
                        drop(buffer);
                        match c.msg_rx.recv().await {
                            Ok(b) => c.buffer.lock().await.extend(b),
                            Err(_) => return Err(ConsumerError::EndOfStream),
                        }
                    }
                }
                let mut buffer = c.buffer.lock().await;

                while buffer.len() < max_messages {
                    match c.msg_rx.try_recv() {
                        Ok(mut b) => buffer.append(&mut b),
                        Err(_) => break,
                    }
                }

                let count = std::cmp::min(buffer.len(), max_messages);
                let batch: Vec<_> = buffer.drain(0..count).collect();
                drop(buffer);

                let path = c.path.clone();
                let lock = c.file_lock.clone();
                let buffer_clone = c.buffer.clone();
                let lines_mem = c.lines_in_memory.clone();
                let nack_buffer = Arc::new(Mutex::new(Some(batch.clone())));
                let delimiter = c.delimiter.clone();

                let commit = Box::new(
                    move |dispositions: Vec<crate::traits::MessageDisposition>| {
                        let nack_buffer = nack_buffer.clone();
                        let path = path.clone();
                        let lock = lock.clone();
                        let buffer_clone = buffer_clone.clone();
                        let lines_mem = lines_mem.clone();
                        let delimiter = delimiter.clone();
                        Box::pin(async move {
                            let mut leading_acks = 0;
                            let mut nacked_msgs = Vec::new();
                            let mut encountered_nack = false;

                            if let Some(original_batch) = nack_buffer.lock().await.take() {
                                for (d, msg) in
                                    dispositions.into_iter().zip(original_batch.into_iter())
                                {
                                    if encountered_nack {
                                        nacked_msgs.push(msg);
                                        continue;
                                    }
                                    match d {
                                        crate::traits::MessageDisposition::Ack
                                        | crate::traits::MessageDisposition::Reply(_) => {
                                            leading_acks += 1;
                                        }
                                        crate::traits::MessageDisposition::Nack => {
                                            encountered_nack = true;
                                            nacked_msgs.push(msg);
                                        }
                                    }
                                }
                            }

                            if !nacked_msgs.is_empty() {
                                let mut buf = buffer_clone.lock().await;
                                let old_buf = std::mem::take(&mut *buf);
                                let mut new_buf = nacked_msgs;
                                new_buf.extend(old_buf);
                                *buf = new_buf;
                            }

                            if leading_acks > 0 {
                                let _guard = lock.lock().await;
                                if let Err(e) =
                                    remove_lines_from_file(&path, leading_acks, &delimiter).await
                                {
                                    tracing::error!("Failed to remove lines from {}: {}", path, e);
                                }
                                lines_mem.fetch_sub(leading_acks, Ordering::SeqCst);
                            }
                            Ok(())
                        })
                            as crate::traits::BoxFuture<'static, anyhow::Result<()>>
                    },
                );

                Ok(ReceivedBatch {
                    messages: batch,
                    commit,
                })
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn parse_message(buffer: &[u8], format: &FileFormat) -> CanonicalMessage {
    match format {
        FileFormat::Raw => {
            let mut msg = CanonicalMessage::new(buffer.to_vec(), None);
            msg.metadata
                .insert("mq_bridge.original_format".to_string(), "raw".to_string());
            msg
        }
        FileFormat::Normal | FileFormat::Json | FileFormat::Text => {
            #[derive(serde::Deserialize)]
            struct AnyPayloadMessage {
                message_id: u128,
                payload: serde_json::Value,
                #[serde(default)]
                metadata: HashMap<String, String>,
            }

            match serde_json::from_slice::<AnyPayloadMessage>(buffer) {
                Ok(wrapper) => {
                    let payload_bytes = if matches!(format, FileFormat::Json) {
                        serde_json::to_vec(&wrapper.payload).unwrap_or_default()
                    } else {
                        match wrapper.payload {
                            serde_json::Value::String(s) => s.into_bytes(),
                            serde_json::Value::Array(arr) => {
                                if let Ok(bytes) = serde_json::from_value::<Vec<u8>>(
                                    serde_json::Value::Array(arr.clone()),
                                ) {
                                    bytes
                                } else {
                                    serde_json::to_vec(&serde_json::Value::Array(arr))
                                        .unwrap_or_default()
                                }
                            }
                            other => serde_json::to_vec(&other).unwrap_or_default(),
                        }
                    };
                    CanonicalMessage {
                        message_id: wrapper.message_id,
                        payload: payload_bytes.into(),
                        metadata: wrapper.metadata,
                    }
                }
                Err(e) => {
                    warn!(error = %e, content_length = buffer.len(), "Failed to parse file line as JSON, treating as raw.");
                    let mut msg = CanonicalMessage::new(buffer.to_vec(), None);
                    msg.metadata
                        .insert("mq_bridge.original_format".to_string(), "raw".to_string());
                    msg
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::endpoints::file::{FileConsumer, FilePublisher};
    use crate::models::{FileConfig, FileConsumerMode, FileFormat};
    use crate::msg;
    use crate::traits::MessageConsumer;
    use crate::traits::MessagePublisher;
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
            ..Default::default()
        };
        let sink = FilePublisher::new(&config).await.unwrap();

        let msg1 = msg!(json!({"hello": "world"}));
        let msg2 = msg!(json!({"foo": "bar"}));

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
            ..Default::default()
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
            mode: Some(FileConsumerMode::Consume { delete: true }),
            ..Default::default()
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
            mode: Some(FileConsumerMode::Consume { delete: true }),
            ..Default::default()
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
            ..Default::default()
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
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        };

        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Give the background tailer a moment to initialize and find its starting position.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

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
        let received2 = consumer.receive_batch(2).await.unwrap();
        assert_eq!(received2.messages.len(), 1);
        assert_eq!(received2.messages[0].payload.as_ref(), b"line2");
        (received2.commit)(vec![crate::traits::MessageDisposition::Ack])
            .await
            .unwrap();

        // Verify file content is unchanged
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[tokio::test]
    async fn test_file_consumer_consume_explicit_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("consume_explicit_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        tokio::fs::write(&file_path, b"line1\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Consume { delete: true }),
            ..Default::default()
        };
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        let received = consumer.receive().await.unwrap();
        assert_eq!(received.message.payload.as_ref(), b"line1");

        (received.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // Verify file becomes empty
        let mut content = String::new();
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
    async fn test_file_consumer_subscribe_with_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("subscribe_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        tokio::fs::write(&file_path, b"line1\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: true }),
            ..Default::default()
        };

        let mut sub1 = FileConsumer::new(&config).await.unwrap();
        let mut sub2 = FileConsumer::new(&config).await.unwrap();

        let msg1 = sub1.receive().await.unwrap();
        assert_eq!(msg1.message.payload.as_ref(), b"line1");

        let msg2 = sub2.receive().await.unwrap();
        assert_eq!(msg2.message.payload.as_ref(), b"line1");

        // Sub1 acks. File should NOT be deleted yet.
        (msg1.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line1\n");

        // Sub2 acks. File should be deleted.
        (msg2.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        let mut content = String::new();
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
    async fn test_file_consumer_subscribe_explicit_no_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("subscribe_no_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        tokio::fs::write(&file_path, b"line1\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        };

        let mut consumer = FileConsumer::new(&config).await.unwrap();
        // Give the background tailer a moment to initialize and find its starting position.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&file_path)
                .await
                .unwrap();
            file.write_all(b"line2\n").await.unwrap();
        }

        let received = consumer.receive().await.unwrap();
        assert_eq!(received.message.payload.as_ref(), b"line2");

        (received.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    use crate::models::{Endpoint, EndpointType, Route};

    #[tokio::test]
    async fn test_route_file_consume_explicit_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("route_consume_explicit_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();
        tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

        let input = Endpoint::new(EndpointType::File(FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Consume { delete: true }),
            ..Default::default()
        }));
        let output = Endpoint::new_memory("out_consume_explicit_delete", 10);
        let route = Route::new(input, output.clone());

        let handle = route
            .run("test_route_consume_explicit_delete")
            .await
            .unwrap();

        let channel = output.channel().unwrap();
        // Wait for message
        let mut received = Vec::new();
        for _ in 0..20 {
            if !channel.is_empty() {
                received = channel.drain_messages();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(received.len(), 1);
        assert_eq!(&received[0].payload.to_vec(), b"msg1");

        // Verify deletion
        let mut content = String::new();
        for _ in 0..20 {
            content = tokio::fs::read_to_string(&file_path).await.unwrap();
            if content.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(content, "");

        handle.stop().await;
    }

    #[tokio::test]
    async fn test_route_file_subscribe_with_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("route_subscribe_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();
        tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

        let input = Endpoint::new(EndpointType::File(FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: true }),
            ..Default::default()
        }));
        let output = Endpoint::new_memory("out_subscribe_delete", 10);
        let route = Route::new(input, output.clone());

        let handle = route.run("test_route_subscribe_delete").await.unwrap();

        let channel = output.channel().unwrap();
        let mut received = Vec::new();
        for _ in 0..20 {
            if !channel.is_empty() {
                received = channel.drain_messages();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(received.len(), 1);

        // Verify deletion
        let mut content = String::new();
        for _ in 0..20 {
            content = tokio::fs::read_to_string(&file_path).await.unwrap();
            if content.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(content, "");

        handle.stop().await;
    }

    #[tokio::test]
    async fn test_route_file_subscribe_explicit_no_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("route_subscribe_no_delete.log");
        let file_path_str = file_path.to_str().unwrap().to_string();
        tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

        let input = Endpoint::new(EndpointType::File(FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        }));
        let output = Endpoint::new_memory("out_subscribe_no_delete", 10);
        let route = Route::new(input, output.clone());

        let handle = route.run("test_route_subscribe_no_delete").await.unwrap();

        let channel = output.channel().unwrap();
        let mut received = Vec::new();
        for _ in 0..20 {
            if !channel.is_empty() {
                received = channel.drain_messages();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(received.len(), 0);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "msg1\n");

        handle.stop().await;
    }

    #[tokio::test]
    async fn test_route_file_consume_all_lines() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("consume_all.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write 10 lines
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("msg{}\n", i));
        }
        tokio::fs::write(&file_path, content).await.unwrap();

        let input = Endpoint::new(EndpointType::File(FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Consume { delete: true }),
            ..Default::default()
        }));
        let output = Endpoint::new_memory("out_consume_all", 100);
        let route = Route::new(input, output.clone());

        let handle = route.run("test_route_consume_all").await.unwrap();

        let channel = output.channel().unwrap();
        // Wait for messages
        let mut received_count = 0;
        for _ in 0..100 {
            received_count += channel.drain_messages().len();
            if received_count >= 10 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(received_count, 10);

        // Verify file is empty
        let mut content = String::new();
        for _ in 0..40 {
            content = tokio::fs::read_to_string(&file_path).await.unwrap();
            if content.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(content, "");

        handle.stop().await;
    }

    #[tokio::test]
    async fn test_file_consumer_group_id_persistence() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("group_id.log");
        let file_path_str = file_path.to_str().unwrap().to_string();
        let offset_path = dir.path().join("group_id.log.my_group.offset");

        // Write initial content
        tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::GroupSubscribe {
                group_id: "my_group".to_string(),
                read_from_tail: false,
            }),
            ..Default::default()
        };

        // 1. First consumer reads msg1
        let mut consumer1 = FileConsumer::new(&config).await.unwrap();
        // Allow thread to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let batch1 = consumer1.receive_batch(1).await.unwrap();
        assert_eq!(batch1.messages[0].payload.as_ref(), b"msg1");

        // Commit msg1 -> should write offset
        (batch1.commit)(vec![crate::traits::MessageDisposition::Ack])
            .await
            .unwrap();

        // Verify offset file exists and contains correct offset (length of "msg1\n" is 5)
        let offset_content = tokio::fs::read_to_string(&offset_path).await.unwrap();
        assert_eq!(offset_content, "5");

        drop(consumer1);

        // 2. Second consumer (simulating restart) should start from offset 5 (msg2)
        let mut consumer2 = FileConsumer::new(&config).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let batch2 = consumer2.receive_batch(1).await.unwrap();
        assert_eq!(batch2.messages[0].payload.as_ref(), b"msg2");

        (batch2.commit)(vec![crate::traits::MessageDisposition::Ack])
            .await
            .unwrap();

        // Verify offset updated (5 + length of "msg2\n" (5) = 10)
        let offset_content = tokio::fs::read_to_string(&offset_path).await.unwrap();
        assert_eq!(offset_content, "10");
    }

    #[tokio::test]
    async fn test_file_consumer_group_id_init_from_start() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("group_id_start.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Write initial content
        tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::GroupSubscribe {
                group_id: "my_group_start".to_string(),
                read_from_tail: false,
            }),
            ..Default::default()
        };

        // Consumer should start from beginning (msg1)
        let mut consumer = FileConsumer::new(&config).await.unwrap();
        // Allow thread to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let batch = consumer.receive_batch(2).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].payload.as_ref(), b"msg1");
        assert_eq!(batch.messages[1].payload.as_ref(), b"msg2");
    }

    #[tokio::test]
    async fn test_file_tail_concurrent_publish_and_consume() {
        // This test verifies that the tail reader can work concurrently with the publisher
        // writing to the file. This is critical for Windows compatibility where file locking
        // semantics may prevent concurrent access if not handled correctly.
        // Note: Even though this runs in the same process, Windows file sharing modes are
        // enforced per-handle. Since the consumer does not participate in the `FILE_LOCKS`
        // mutex used by the publisher, this effectively tests that the OS allows the
        // publisher to open/write while the consumer has the file open for reading.
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("concurrent.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Create file with initial message
        tokio::fs::write(&file_path, b"msg0\n").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        };

        // Start the tail consumer
        let mut consumer = FileConsumer::new(&config).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Spawn a task that continuously publishes messages while the consumer is reading
        let publisher_path = file_path_str.clone();
        let publish_handle = tokio::spawn(async move {
            let pub_config = FileConfig {
                path: publisher_path,
                mode: Some(FileConsumerMode::Subscribe { delete: false }),
                ..Default::default()
            };
            let publisher = FilePublisher::new(&pub_config).await.unwrap();

            // Send enough messages to ensure overlap between reading and writing
            for i in 1..=100 {
                let msg = msg!(json!({"id": i, "data": format!("message_{}", i)}));
                publisher.send_batch(vec![msg]).await.unwrap();
                // Small delay to allow consumer to catch up and potentially open the file
                if i % 10 == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        });

        // Consumer should be able to read messages while publisher is writing
        let mut received_count = 0;
        let mut message_ids = Vec::new();

        // We expect 100 published messages (initial msg0 is skipped in Subscribe mode)
        let expected_count = 100;
        let start = std::time::Instant::now();

        while received_count < expected_count {
            if start.elapsed() > std::time::Duration::from_secs(10) {
                break;
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                consumer.receive_batch(10),
            )
            .await
            {
                Ok(Ok(batch)) => {
                    for msg in &batch.messages {
                        received_count += 1;
                        if let Ok(json_msg) =
                            serde_json::from_slice::<serde_json::Value>(&msg.payload)
                        {
                            if let Some(id) = json_msg.get("id").and_then(|v| v.as_i64()) {
                                message_ids.push(id);
                            }
                        }
                    }
                    (batch.commit)(vec![
                        crate::traits::MessageDisposition::Ack;
                        batch.messages.len()
                    ])
                    .await
                    .unwrap();
                }
                Ok(Err(_)) => break, // Stream ended
                Err(_) => continue,  // Timeout waiting for message
            }
        }

        publish_handle.await.unwrap();

        // Verify we received at least some messages from the publisher
        // We should receive the messages from the concurrent publisher
        assert_eq!(
            received_count, expected_count,
            "Expected {} messages, got {}. This may indicate file locking issues on this platform.",
            expected_count, received_count
        );

        // Verify the file still exists and can be read (not locked/deleted)
        let final_content = tokio::fs::read_to_string(&file_path)
            .await
            .expect("File should still be readable after concurrent access");
        assert!(
            !final_content.is_empty(),
            "File should contain messages after concurrent access"
        );
    }

    /// Simulates an external process (like a Python script or log writer) appending to the file.
    /// Unlike `test_file_tail_concurrent_publish_and_consume`, this test:
    /// 1. Does not use `FilePublisher` (bypassing internal `FILE_LOCKS`).
    /// 2. Keeps the file handle open across multiple writes (simulating a long-running writer),
    ///    which stresses file locking/sharing semantics on OSs like Windows.
    #[tokio::test]
    async fn test_file_subscribe_concurrent_external_write() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("external_write.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        // Create empty file
        tokio::fs::write(&file_path, b"").await.unwrap();

        let config = FileConfig {
            path: file_path_str.clone(),
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        };

        let mut consumer = FileConsumer::new(&config).await.unwrap();

        // Give the background tailer a moment to initialize
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let file_path_clone = file_path.clone();
        let write_task = tokio::spawn(async move {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&file_path_clone)
                .await
                .unwrap();

            for i in 0..5 {
                let line = format!("message {}\n", i);
                file.write_all(line.as_bytes()).await.unwrap();
                file.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });

        for i in 0..5 {
            let received =
                tokio::time::timeout(std::time::Duration::from_secs(5), consumer.receive())
                    .await
                    .expect("Timed out waiting for message")
                    .unwrap();

            let expected_payload = format!("message {}", i);
            assert_eq!(received.message.get_payload_str().trim(), expected_payload);
            (received.commit)(crate::traits::MessageDisposition::Ack)
                .await
                .unwrap();
        }

        write_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_file_custom_delimiter() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("custom_delim.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        let config = FileConfig {
            path: file_path_str.clone(),
            delimiter: Some("|".to_string()),
            mode: Some(FileConsumerMode::Consume { delete: false }),
            ..Default::default()
        };

        let publisher = FilePublisher::new(&config).await.unwrap();
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        let msg1 = crate::CanonicalMessage::from("msg1").with_raw_format();
        let msg2 = crate::CanonicalMessage::from("msg2").with_raw_format();

        publisher.send_batch(vec![msg1, msg2]).await.unwrap();
        publisher.flush().await.unwrap();
        drop(publisher); // Release lock

        // Verify file content has pipes
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "msg1|msg2|");

        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.get_payload_str(), "msg1");

        let received2 = consumer.receive().await.unwrap();
        assert_eq!(received2.message.get_payload_str(), "msg2");
    }

    #[tokio::test]
    async fn test_file_xml_delimiter() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("xml_delim.log");
        let file_path_str = file_path.to_str().unwrap().to_string();

        let config = FileConfig {
            path: file_path_str.clone(),
            delimiter: Some("</message>".to_string()),
            mode: Some(FileConsumerMode::Consume { delete: false }),
            ..Default::default()
        };

        let publisher = FilePublisher::new(&config).await.unwrap();
        let mut consumer = FileConsumer::new(&config).await.unwrap();

        let msg1 = crate::CanonicalMessage::from("<xml>content1").with_raw_format();
        let msg2 = crate::CanonicalMessage::from("<xml>content2").with_raw_format();

        publisher.send_batch(vec![msg1, msg2]).await.unwrap();
        publisher.flush().await.unwrap();
        drop(publisher); // Release lock

        // Verify file content has tags
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "<xml>content1</message><xml>content2</message>");

        let received1 = consumer.receive().await.unwrap();
        assert_eq!(received1.message.get_payload_str(), "<xml>content1");

        let received2 = consumer.receive().await.unwrap();
        assert_eq!(received2.message.get_payload_str(), "<xml>content2");
    }

    #[tokio::test]
    async fn test_file_formats_and_fallbacks() {
        let dir = tempdir().unwrap();

        // 1. Test JSON Format Roundtrip
        let json_path = dir.path().join("json.log");
        let json_config = FileConfig {
            path: json_path.to_str().unwrap().to_string(),
            format: FileFormat::Json,
            ..Default::default()
        };

        let json_publisher = FilePublisher::new(&json_config).await.unwrap();
        let mut json_consumer = FileConsumer::new(&json_config).await.unwrap();

        let json_payload = json!({"key": "value", "num": 123});
        let msg = msg!(json_payload.clone());

        json_publisher.send_batch(vec![msg.clone()]).await.unwrap();
        json_publisher.flush().await.unwrap();
        drop(json_publisher); // Release lock

        let received = json_consumer.receive().await.unwrap();
        let received_json: serde_json::Value =
            serde_json::from_slice(&received.message.payload).unwrap();
        assert_eq!(received_json, json_payload);
        (received.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // 2. Test Text Format Roundtrip
        let text_path = dir.path().join("text.log");
        let text_config = FileConfig {
            path: text_path.to_str().unwrap().to_string(),
            format: FileFormat::Text,
            ..Default::default()
        };

        let text_publisher = FilePublisher::new(&text_config).await.unwrap();
        let mut text_consumer = FileConsumer::new(&text_config).await.unwrap();

        let text_payload = "Hello World";
        let msg = crate::CanonicalMessage::from(text_payload);

        text_publisher.send_batch(vec![msg.clone()]).await.unwrap();
        text_publisher.flush().await.unwrap();
        drop(text_publisher);

        let received = text_consumer.receive().await.unwrap();
        assert_eq!(received.message.get_payload_str(), text_payload);
        (received.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();

        // 3. Test Fallback (Corrupted/Raw line in Json format)
        // We append a raw line that isn't the expected JSON wrapper structure
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&json_path)
                .await
                .unwrap();
            file.write_all(b"Not a JSON wrapper\n").await.unwrap();
        }

        let received_fallback = json_consumer.receive().await.unwrap();
        // Should be treated as raw
        assert_eq!(
            received_fallback.message.get_payload_str(),
            "Not a JSON wrapper"
        );
        assert_eq!(
            received_fallback
                .message
                .metadata
                .get("mq_bridge.original_format")
                .map(|s| s.as_str()),
            Some("raw")
        );
    }
}
