//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Redis Streams endpoint.
//!
//! Publishers `XADD` each message to a stream key. Consumers read through a
//! consumer group (`XREADGROUP` + per-message `XACK`) by default, or ephemerally
//! via `XREAD` from new messages when `subscriber_mode` is set.
//!
//! The payload is stored under the `payload` field and the mq-bridge message id
//! under `mqb_message_id`; every remaining metadata entry becomes its own field,
//! so messages stay readable by any other Redis client.

use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::RedisStreamsConfig;
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamReadOptions, StreamReadReply,
};
use redis::{AsyncCommands, IntoConnectionInfo};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{trace, warn};

/// The stream field used to carry the raw message payload.
const PAYLOAD_FIELD: &str = "payload";
/// The stream field used to carry the mq-bridge message id (hex).
const MESSAGE_ID_FIELD: &str = "mqb_message_id";

const DEFAULT_BLOCK_MS: u64 = 5000;
const DEFAULT_BUFFER: usize = 128;
/// Default idle time before a pending entry is reclaimed and redelivered.
const DEFAULT_REDELIVERY_MS: u64 = 60_000;

fn open_client(config: &RedisStreamsConfig) -> anyhow::Result<redis::Client> {
    // redis 1.3's ConnectionInfo has no public auth setter, so credentials are
    // injected into the URL's userinfo when provided as separate config fields.
    let url = url_with_credentials(config);
    let info = url
        .as_str()
        .into_connection_info()
        .map_err(|e| anyhow!("Invalid Redis URL: {}", e))?;
    redis::Client::open(info).map_err(|e| anyhow!("Failed to open Redis client: {}", e))
}

/// Percent-encodes a userinfo component per RFC 3986, escaping every character
/// outside the unreserved set so reserved characters (`@`, `:`, `/`, ...) in
/// credentials can't corrupt URL parsing.
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Injects `config.username`/`config.password` into the URL's userinfo. If the
/// URL already carries userinfo, or no credentials are configured, it is
/// returned unchanged. Credentials are percent-encoded so reserved characters
/// don't break URL parsing.
fn url_with_credentials(config: &RedisStreamsConfig) -> String {
    if config.username.is_none() && config.password.is_none() {
        return config.url.clone();
    }
    let Some(scheme_end) = config.url.find("://") else {
        return config.url.clone();
    };
    let (scheme, rest) = config.url.split_at(scheme_end + 3);
    let authority_end = rest.find('/').unwrap_or(rest.len());
    if rest[..authority_end].contains('@') {
        // Userinfo already present in the URL; leave it as the source of truth.
        return config.url.clone();
    }
    let user = config
        .username
        .as_deref()
        .map(encode_userinfo)
        .unwrap_or_default();
    let userinfo = match &config.password {
        Some(password) => format!("{}:{}@", user, encode_userinfo(password)),
        None => format!("{}@", user),
    };
    format!("{}{}{}", scheme, userinfo, rest)
}

// --- Publisher ---

pub struct RedisStreamsPublisher {
    conn: ConnectionManager,
    stream: String,
    maxlen: Option<usize>,
    approx_trim: bool,
}

impl RedisStreamsPublisher {
    pub async fn new(config: &RedisStreamsConfig) -> anyhow::Result<Self> {
        let stream = config
            .stream
            .clone()
            .ok_or_else(|| anyhow!("Stream key is required for Redis Streams publisher"))?;
        let client = open_client(config)?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| anyhow!("Failed to connect to Redis: {}", e))?;
        Ok(Self {
            conn,
            stream,
            maxlen: config.maxlen,
            approx_trim: config.approx_trim.unwrap_or(true),
        })
    }
}

#[async_trait]
impl MessagePublisher for RedisStreamsPublisher {
    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(stream = %self.stream, count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch of Redis Streams messages");
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        // XADD each message in one pipeline round trip.
        let mut pipe = redis::pipe();
        for message in &mut messages {
            // Source/provenance keys are per-hop context and must not be forwarded.
            message.strip_source_metadata();
            pipe.cmd("XADD").arg(&self.stream);
            if let Some(maxlen) = self.maxlen {
                pipe.arg("MAXLEN");
                if self.approx_trim {
                    pipe.arg("~");
                }
                pipe.arg(maxlen);
            }
            pipe.arg("*")
                .arg(PAYLOAD_FIELD)
                .arg(message.payload.as_ref())
                .arg(MESSAGE_ID_FIELD)
                .arg(format!("{:032x}", message.message_id));
            for (key, value) in &message.metadata {
                // Never let a metadata key shadow the reserved fields; a duplicate
                // XADD field would overwrite the payload/id on read (last wins).
                if key == PAYLOAD_FIELD || key == MESSAGE_ID_FIELD {
                    continue;
                }
                pipe.arg(key).arg(value);
            }
        }

        let mut conn = self.conn.clone();
        pipe.query_async::<()>(&mut conn)
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!("Redis XADD failed: {}", e)))?;
        Ok(SentBatch::Ack)
    }

    async fn status(&self) -> EndpointStatus {
        let mut conn = self.conn.clone();
        let healthy = redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok();
        EndpointStatus {
            healthy,
            target: self.stream.clone(),
            error: if healthy {
                None
            } else {
                Some("Redis PING failed".to_string())
            },
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// --- Consumer ---

struct StreamEntry {
    /// The native Redis stream entry id, used for `XACK` in group mode.
    id: String,
    msg: CanonicalMessage,
}

pub struct RedisStreamsConsumer {
    rx: Receiver<Result<StreamEntry, ConsumerError>>,
    ack_conn: ConnectionManager,
    stream: String,
    /// `Some` in consumer-group mode (entries are acked); `None` in subscriber mode.
    group: Option<String>,
    buffer: VecDeque<StreamEntry>,
    /// Set by the route when `exit_on_empty`/`--drain` is active. Only then does an
    /// idle read time out into an empty batch; otherwise it blocks indefinitely
    /// (event-driven, no added latency) as a streaming source should.
    exit_on_empty: bool,
    /// Every consumer name this instance reads under, for the own-PEL health probe.
    consumer_names: Vec<String>,
    redelivery_ms: u64,
    /// Entries handed to the route, and entries the route has since settled
    /// (acked or nacked). Equal means nothing is in flight downstream.
    delivered: Arc<AtomicU64>,
    settled: Arc<AtomicU64>,
    /// When the "own PEL non-empty with nothing in flight" state was first seen.
    stuck_since: std::sync::Mutex<Option<Instant>>,
}

impl RedisStreamsConsumer {
    pub async fn new(config: &RedisStreamsConfig) -> anyhow::Result<Self> {
        let stream = config
            .stream
            .clone()
            .ok_or_else(|| anyhow!("Stream key is required for Redis Streams consumer"))?;
        let client = open_client(config)?;

        // A dedicated connection for the (blocking) reads, plus a separate one for
        // acks so an in-flight XREAD BLOCK never stalls XACK. Extra reader
        // connections (see `reader_connections`) are opened below from `client`.
        let mut read_conn = ConnectionManager::new(client.clone())
            .await
            .map_err(|e| anyhow!("Failed to connect to Redis: {}", e))?;
        let ack_conn = ConnectionManager::new(client.clone())
            .await
            .map_err(|e| anyhow!("Failed to connect to Redis: {}", e))?;

        let block_ms = config.block_ms.unwrap_or(DEFAULT_BLOCK_MS) as usize;
        let count = config.internal_buffer_size.unwrap_or(DEFAULT_BUFFER).max(1);
        // 0 disables reclaiming; otherwise redeliver entries idle past this long.
        let redelivery_ms = config
            .redelivery_timeout_ms
            .unwrap_or(DEFAULT_REDELIVERY_MS);

        let group = if config.subscriber_mode {
            None
        } else {
            let group = config
                .group
                .clone()
                .unwrap_or_else(|| format!("{}-{}", APP_NAME, stream));
            // "$" reads only new messages; "0" replays the whole stream. Only takes
            // effect when the group is first created (BUSYGROUP is ignored below).
            let start_id = if config.read_from_start { "0" } else { "$" };
            let created: redis::RedisResult<()> = read_conn
                .xgroup_create_mkstream(&stream, &group, start_id)
                .await;
            if let Err(e) = created {
                // BUSYGROUP: the group already exists, which is expected on restart.
                if e.code() != Some("BUSYGROUP") {
                    return Err(anyhow!("Failed to create Redis consumer group: {}", e));
                }
            }
            Some(group)
        };

        // A unique per-instance name by default. Entries stranded under an old
        // name (e.g. after a restart) are recovered by the XAUTOCLAIM pass below,
        // which reclaims idle pending entries across the whole group.
        let consumer_name = config.consumer_name.clone().unwrap_or_else(|| {
            format!("{}-{:032x}", APP_NAME, fast_uuid_v7::gen_id_with_sub_ms_4())
        });

        // Consumer-group mode can fan the reads out across several connections in
        // the same group: Redis load-balances `>` deliveries across the distinct
        // per-connection consumer names, and per-entry XACK stays order-independent.
        // Subscriber mode reads via XREAD from a shared cursor, which every
        // connection would see identically, so it stays single-connection.
        let readers = if group.is_some() {
            config.reader_connections.unwrap_or(1).max(1)
        } else {
            1
        };
        // Give each reader room to stage a full COUNT batch without stalling on a
        // slow drain, so the connections actually make progress in parallel.
        let (tx, rx) =
            bounded::<Result<StreamEntry, ConsumerError>>(count.saturating_mul(readers).max(count));

        let mut consumer_names = Vec::with_capacity(readers);
        for i in 0..readers {
            // Reader 0 reuses the connection the group was created on; the rest get
            // their own so one reader's blocking XREAD never stalls a sibling.
            let read_conn = if i == 0 {
                read_conn.clone()
            } else {
                ConnectionManager::new(client.clone())
                    .await
                    .map_err(|e| anyhow!("Failed to connect to Redis: {}", e))?
            };
            // Distinct names per connection so the group load-balances across them.
            let consumer_name = if readers == 1 {
                consumer_name.clone()
            } else {
                format!("{}-{}", consumer_name, i)
            };
            consumer_names.push(consumer_name.clone());
            // Only one reader runs the reclaim pass, to avoid N-way XAUTOCLAIM contention.
            // It gets its own connection: XAUTOCLAIM must not share the multiplexed
            // connection that is parked in `XREAD BLOCK`, or the claim can succeed
            // server-side while the reply times out client-side, stranding the entries
            // in this consumer's PEL with their idle time reset on every retry.
            let reclaim_conn = if i == 0 && group.is_some() && redelivery_ms > 0 {
                Some(
                    ConnectionManager::new(client.clone())
                        .await
                        .map_err(|e| anyhow!("Failed to connect to Redis: {}", e))?,
                )
            } else {
                None
            };
            spawn_stream_reader(ReaderCtx {
                read_conn,
                reclaim_conn,
                stream: stream.clone(),
                group: group.clone(),
                consumer_name,
                count,
                block_ms,
                redelivery_ms,
                tx: tx.clone(),
            });
        }
        drop(tx);

        Ok(Self {
            rx,
            ack_conn,
            stream,
            group,
            buffer: VecDeque::new(),
            exit_on_empty: false,
            consumer_names,
            redelivery_ms,
            delivered: Arc::new(AtomicU64::new(0)),
            settled: Arc::new(AtomicU64::new(0)),
            stuck_since: std::sync::Mutex::new(None),
        })
    }
}

impl RedisStreamsConsumer {
    /// Detects entries stuck in *this* consumer's own PEL: claimed from the group but
    /// never delivered onward. That state is unreachable in normal operation — every
    /// entry we own is either buffered here, in flight downstream, or waiting to be
    /// reclaimed — so once it persists it means the reader has wedged. It used to be
    /// invisible, with the route reporting a healthy idle consumer forever.
    ///
    /// Returns the error message when stalled. Requires reclaiming to be enabled;
    /// with `redelivery_timeout_ms: 0` a permanently pending entry is the configured
    /// behaviour, not a fault.
    async fn stalled_own_pending(&self, conn: &mut ConnectionManager) -> Option<String> {
        let clear = || *self.stuck_since.lock().unwrap() = None;
        let Some(group) = &self.group else {
            return None;
        };
        // Anything queued locally, or still unsettled downstream, is progress.
        if self.redelivery_ms == 0
            || !self.buffer.is_empty()
            || !self.rx.is_empty()
            || self.delivered.load(Ordering::Relaxed) != self.settled.load(Ordering::Relaxed)
        {
            clear();
            return None;
        }

        let mut pending = 0usize;
        for name in &self.consumer_names {
            let reply: redis::RedisResult<redis::streams::StreamPendingCountReply> = conn
                .xpending_consumer_count(&self.stream, group, "-", "+", 1, name)
                .await;
            match reply {
                Ok(reply) => pending += reply.ids.len(),
                // Can't tell; leave the verdict to the next poll.
                Err(_) => return None,
            }
        }
        if pending == 0 {
            clear();
            return None;
        }

        // Grace period: a nacked entry legitimately sits in our PEL until the
        // reclaim pass redelivers it, which takes up to redelivery_ms.
        let grace = Duration::from_millis(self.redelivery_ms.saturating_mul(2)).max(TEN_SECONDS);
        let mut stuck_since = self.stuck_since.lock().unwrap();
        let since = stuck_since.get_or_insert_with(Instant::now);
        if since.elapsed() < grace {
            return None;
        }
        Some(format!(
            "Redis consumer stalled: entries pending under this consumer's own name in group '{}' \
             with nothing in flight for {:?}",
            group,
            since.elapsed()
        ))
    }
}

const TEN_SECONDS: Duration = Duration::from_secs(10);

/// Parameters for a single background stream-reader task.
struct ReaderCtx {
    read_conn: ConnectionManager,
    /// Dedicated connection for the XAUTOCLAIM reclaim pass; `Some` on the one
    /// reader that runs it. Never shared with the blocking read connection.
    reclaim_conn: Option<ConnectionManager>,
    stream: String,
    /// `Some` in consumer-group mode; `None` in subscriber mode.
    group: Option<String>,
    consumer_name: String,
    count: usize,
    block_ms: usize,
    redelivery_ms: u64,
    tx: Sender<Result<StreamEntry, ConsumerError>>,
}

/// Spawns a background task that reads stream entries (via `XREADGROUP` in group
/// mode, or `XREAD` in subscriber mode) and forwards them to the consumer channel.
fn spawn_stream_reader(ctx: ReaderCtx) {
    let ReaderCtx {
        mut read_conn,
        mut reclaim_conn,
        stream: task_stream,
        group: task_group,
        consumer_name,
        count,
        block_ms,
        redelivery_ms,
        tx,
    } = ctx;
    tokio::spawn(async move {
        // In subscriber mode we track the last delivered id ("$" = new only).
        let mut last_id = String::from("$");
        // Check for reclaimable entries on the read cadence; eligibility is
        // still gated by redelivery_ms of idleness on the server side.
        let reclaim_interval = Duration::from_millis(redelivery_ms.min(block_ms as u64).max(1000));
        let mut last_reclaim: Option<Instant> = None;
        // XAUTOCLAIM cursor, carried across passes so we don't rescan the
        // same pending entries each time; "0-0" restarts a full scan.
        let mut reclaim_cursor = String::from("0-0");
        loop {
            // Redeliver entries left pending past redelivery_ms (Nacked, or
            // orphaned by a crashed/renamed consumer) via XAUTOCLAIM.
            if last_reclaim.is_none_or(|t| t.elapsed() >= reclaim_interval) {
                last_reclaim = Some(Instant::now());
                if let (Some(group), Some(conn)) = (&task_group, reclaim_conn.as_mut()) {
                    let claim_opts = StreamAutoClaimOptions::default().count(count);
                    let claimed: redis::RedisResult<StreamAutoClaimReply> = conn
                        .xautoclaim_options(
                            &task_stream,
                            group,
                            &consumer_name,
                            redelivery_ms as usize,
                            &reclaim_cursor,
                            claim_opts,
                        )
                        .await;
                    match claimed {
                        Ok(reply) => {
                            // Advance the cursor; the server returns "0-0"
                            // once the pending set has been fully scanned.
                            reclaim_cursor = reply.next_stream_id;
                            for entry in reply.claimed {
                                let stream_entry = parse_entry(entry.id, entry.map);
                                if tx.send(Ok(stream_entry)).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                        }
                        // Transient; a hard error surfaces on the read below. Warn
                        // anyway: a claim that fails after the server applied it
                        // leaves entries pending under this consumer, and silence
                        // makes that indistinguishable from an idle stream.
                        Err(e) => {
                            warn!(stream = %task_stream, error = %e, "Redis XAUTOCLAIM failed")
                        }
                    }
                }
            }

            let mut opts = StreamReadOptions::default().count(count).block(block_ms);
            let read_ids: Vec<&str> = match &task_group {
                Some(group) => {
                    opts = opts.group(group, &consumer_name);
                    vec![">"]
                }
                None => vec![last_id.as_str()],
            };

            let reply: redis::RedisResult<Option<StreamReadReply>> = read_conn
                .xread_options(&[&task_stream], read_ids.as_slice(), &opts)
                .await;

            match reply {
                Ok(Some(reply)) => {
                    for key in reply.keys {
                        for entry in key.ids {
                            if task_group.is_none() {
                                last_id = entry.id.clone();
                            }
                            let stream_entry = parse_entry(entry.id, entry.map);
                            if tx.send(Ok(stream_entry)).await.is_err() {
                                return; // consumer dropped
                            }
                        }
                    }
                }
                // BLOCK timeout with no data — just poll again.
                Ok(None) => {}
                // Client-side read timeout: same "no data" condition as `Ok(None)`,
                // it just fired before the server's empty BLOCK reply arrived.
                Err(e) if e.is_timeout() => {
                    trace!(
                        stream = %task_stream,
                        error = %e,
                        "Redis XREAD client-side read timeout; polling again"
                    );
                }
                Err(e) => {
                    if tx
                        .send(Err(ConsumerError::Connection(anyhow!(
                            "Redis XREAD failed: {}",
                            e
                        ))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    // Avoid a hot error loop while disconnected.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    });
}

/// Reconstructs a [`CanonicalMessage`] from a Redis stream entry's field map.
fn parse_entry(id: String, map: HashMap<String, redis::Value>) -> StreamEntry {
    let mut payload = Vec::new();
    let mut message_id = None;
    let mut metadata = HashMap::new();

    for (key, value) in map {
        if key == PAYLOAD_FIELD {
            payload = redis::from_redis_value::<Vec<u8>>(value).unwrap_or_default();
        } else if key == MESSAGE_ID_FIELD {
            if let Ok(s) = redis::from_redis_value::<String>(value) {
                if let Ok(n) = u128::from_str_radix(&s, 16) {
                    message_id = Some(n);
                }
            }
        } else {
            // A stored field must never spoof a reserved `mqb.src.*` cursor key;
            // the authoritative cursor is injected below.
            if crate::canonical_message::is_source_metadata_key(&key) {
                continue;
            }
            if let Ok(s) = redis::from_redis_value::<String>(value) {
                metadata.insert(key, s);
            }
        }
    }

    let mut msg = CanonicalMessage::new(payload, message_id);
    msg.metadata = metadata;
    // Opt-in via the MQB_SOURCE_METADATA env var; off by default.
    if crate::canonical_message::source_metadata_enabled() {
        msg.metadata
            .insert("mqb.src.redis_stream_id".to_string(), id.clone());
    }
    StreamEntry { id, msg }
}

#[async_trait]
impl MessageConsumer for RedisStreamsConsumer {
    // Redis acks each entry individually via XACK, so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        if self.buffer.is_empty() {
            // Drain mode times out into an empty batch (async_channel recv is cancel-safe,
            // so no entry is lost); streaming blocks indefinitely for the first entry.
            let Some(res) = crate::traits::drain_gated(self.exit_on_empty, self.rx.recv()).await
            else {
                return Ok(ReceivedBatch::empty());
            };
            let entry = res.map_err(|_| ConsumerError::EndOfStream)??;
            self.buffer.push_back(entry);
        }

        // Greedily drain whatever else is already buffered, up to max_messages.
        while self.buffer.len() < max_messages {
            match self.rx.try_recv() {
                Ok(Ok(entry)) => self.buffer.push_back(entry),
                // A connection error surfaces on the next blocking recv; stop here.
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let mut messages = Vec::with_capacity(max_messages);
        let mut ids = Vec::with_capacity(max_messages);
        while messages.len() < max_messages {
            if let Some(entry) = self.buffer.pop_front() {
                ids.push(entry.id);
                messages.push(entry.msg);
            } else {
                break;
            }
        }

        trace!(stream = %self.stream, count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Received batch of Redis Streams messages");

        self.delivered
            .fetch_add(messages.len() as u64, Ordering::Relaxed);

        let group = self.group.clone();
        let stream = self.stream.clone();
        let mut ack_conn = self.ack_conn.clone();
        let settled = self.settled.clone();
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                settled.fetch_add(dispositions.len() as u64, Ordering::Relaxed);
                // Subscriber mode has no group and therefore nothing to ack.
                let Some(group) = group else {
                    return Ok(());
                };
                // Ack successfully handled entries; Nacked ones stay pending so the
                // group can redeliver them (e.g. via XAUTOCLAIM) later.
                let ack_ids: Vec<String> = ids
                    .into_iter()
                    .zip(dispositions)
                    .filter_map(|(id, disposition)| match disposition {
                        MessageDisposition::Ack | MessageDisposition::Reply(_) => Some(id),
                        MessageDisposition::Nack => None,
                    })
                    .collect();
                if !ack_ids.is_empty() {
                    ack_conn
                        .xack::<_, _, _, ()>(&stream, &group, &ack_ids)
                        .await
                        .map_err(|e| anyhow!("Redis XACK failed: {}", e))?;
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let mut conn = self.ack_conn.clone();
        let healthy = redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok();
        if !healthy {
            return EndpointStatus {
                healthy,
                target: self.stream.clone(),
                pending: Some(self.buffer.len() + self.rx.len()),
                error: Some("Redis PING failed".to_string()),
                ..Default::default()
            };
        }
        let stalled = self.stalled_own_pending(&mut conn).await;
        EndpointStatus {
            healthy: stalled.is_none(),
            target: self.stream.clone(),
            pending: Some(self.buffer.len() + self.rx.len()),
            error: stalled,
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

    fn bulk(s: &str) -> redis::Value {
        redis::Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn parse_entry_reads_payload_and_message_id() {
        let mut map = HashMap::new();
        map.insert(PAYLOAD_FIELD.to_string(), bulk("hello"));
        map.insert(
            MESSAGE_ID_FIELD.to_string(),
            bulk("0000000000000000000000000000002a"),
        );
        map.insert("user_key".to_string(), bulk("kept"));

        let entry = parse_entry("1-0".to_string(), map);
        assert_eq!(entry.msg.payload.as_ref(), b"hello");
        assert_eq!(entry.msg.message_id, 0x2a);
        assert_eq!(
            entry.msg.metadata.get("user_key").map(String::as_str),
            Some("kept")
        );
        // Reserved fields never leak into user metadata.
        assert!(!entry.msg.metadata.contains_key(PAYLOAD_FIELD));
        assert!(!entry.msg.metadata.contains_key(MESSAGE_ID_FIELD));
    }

    #[test]
    fn parse_entry_strips_spoofed_source_metadata() {
        let mut map = HashMap::new();
        map.insert(PAYLOAD_FIELD.to_string(), bulk("body"));
        map.insert("mqb.src.kafka_offset".to_string(), bulk("999"));
        map.insert("user_key".to_string(), bulk("kept"));

        let entry = parse_entry("2-0".to_string(), map);
        assert!(!entry.msg.metadata.contains_key("mqb.src.kafka_offset"));
        assert_eq!(
            entry.msg.metadata.get("user_key").map(String::as_str),
            Some("kept")
        );
    }

    #[test]
    fn parse_entry_without_message_id_gets_generated_id() {
        let mut map = HashMap::new();
        map.insert(PAYLOAD_FIELD.to_string(), bulk("body"));
        let entry = parse_entry("3-0".to_string(), map);
        // CanonicalMessage::new assigns a uuid v7 when none is provided.
        assert_ne!(entry.msg.message_id, 0);
    }
}
