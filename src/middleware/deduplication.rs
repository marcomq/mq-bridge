//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
use super::deferred_commit::{run_all, DeferredCommits};
use crate::models::DeduplicationMiddleware;
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, Received, ReceivedBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use sled::Db;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, instrument, trace, warn};

/// Short TTL for a reservation held while a message is in flight. Kept small so a crash between
/// reserve and commit frees the key quickly for at-least-once redelivery.
pub(crate) const PENDING_TTL_SECS: u64 = 5;

/// A pluggable deduplication backend: a keyed store with an atomic reserve and a TTL.
///
/// `reserve` is the ordering point — it atomically claims a key, returning whether a *live*
/// entry already existed. `mark_processed` promotes a claimed key to the full TTL once the
/// message is committed. Local (`sled`) and shared (`mongodb`) backends implement this.
#[async_trait]
pub(crate) trait DedupStore: Send + Sync {
    /// Atomically reserve `key`. `Ok(true)` = a live entry already exists (duplicate);
    /// `Ok(false)` = freshly reserved (caller should process the message).
    async fn reserve(&self, key: &[u8], now: u64) -> Result<bool, ConsumerError>;

    /// Reserve a whole batch at once, returning one flag per key in order. Backends that can
    /// amortise per-call work — a lock, a network round trip — override this; the default
    /// reserves one key at a time.
    async fn reserve_many(&self, keys: &[Vec<u8>], now: u64) -> Result<Vec<bool>, ConsumerError> {
        let mut claimed = Vec::with_capacity(keys.len());
        for key in keys {
            claimed.push(self.reserve(key, now).await?);
        }
        Ok(claimed)
    }

    /// Promote a reserved key to the processed state with the full TTL. Best-effort: a failure
    /// here only risks a later redelivery being reprocessed (at-least-once), so it logs rather
    /// than propagating.
    async fn mark_processed(&self, key: &[u8], now: u64);

    /// Promote a whole batch of reserved keys. See [`DedupStore::reserve_many`].
    async fn mark_processed_many(&self, keys: &[Vec<u8>], now: u64) {
        for key in keys {
            self.mark_processed(key, now).await;
        }
    }

    /// Best-effort periodic GC of expired keys. Default no-op for backends with native TTL.
    fn maybe_cleanup(&self, _now: u64) {}

    /// Flush buffered writes on disconnect. Default no-op.
    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A parsed deduplication `store:` destination.
pub(crate) enum DedupBackend {
    /// Local single-instance Sled directory.
    Sled { path: String },
    /// Shared MongoDB collection (`mongodb://host/db[/collection]`).
    #[cfg(feature = "mongodb")]
    Mongo {
        url: String,
        database: String,
        collection: Option<String>,
    },
    /// Shared SQL table (`postgres|mysql|mariadb|sqlite://…[/table]`).
    #[cfg(feature = "sqlx")]
    Sqlx { url: String, table: Option<String> },
}

/// Parse a `store:` value. `sled:`/bare paths select the local store; other schemes are handed to
/// the checkpoint URL parser and only MongoDB is accepted (SQL/Redis land in later slices).
pub(crate) fn parse_dedup_store(spec: &str) -> anyhow::Result<DedupBackend> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("deduplication store is empty"));
    }
    let scheme = spec
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    // `C:\path` / `C:/path` is a Windows drive letter, not a URL scheme.
    if scheme.len() == 1 && scheme.chars().all(|c| c.is_ascii_alphabetic()) {
        return Ok(DedupBackend::Sled {
            path: spec.to_string(),
        });
    }
    match scheme.as_str() {
        "sled" => {
            let path = spec
                .strip_prefix("sled://")
                .or_else(|| spec.strip_prefix("sled:"))
                .unwrap_or(spec);
            Ok(DedupBackend::Sled {
                path: path.to_string(),
            })
        }
        // Schemeless (a bare filesystem path) -> local sled.
        "" => Ok(DedupBackend::Sled {
            path: spec.to_string(),
        }),
        _ => parse_networked_dedup_store(spec),
    }
}

/// Parse a non-sled `store:` scheme by delegating to the checkpoint URL parser, then accepting
/// only the backends deduplication supports in this build.
#[cfg(any(feature = "mongodb", feature = "sqlx"))]
fn parse_networked_dedup_store(spec: &str) -> anyhow::Result<DedupBackend> {
    match crate::checkpoint::parse_checkpoint_store(spec)? {
        #[cfg(feature = "mongodb")]
        crate::checkpoint::CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => Ok(DedupBackend::Mongo {
            url,
            database,
            collection,
        }),
        #[cfg(feature = "sqlx")]
        crate::checkpoint::CheckpointBackend::Sqlx { url, table } => {
            Ok(DedupBackend::Sqlx { url, table })
        }
        other => Err(anyhow!(
            "deduplication store '{spec}' is not supported (only sled, mongodb, and SQL); got {other:?}"
        )),
    }
}

#[cfg(not(any(feature = "mongodb", feature = "sqlx")))]
fn parse_networked_dedup_store(spec: &str) -> anyhow::Result<DedupBackend> {
    Err(anyhow!(
        "deduplication store '{spec}' requires a networked backend feature (mongodb or sqlx); this build supports only local sled paths"
    ))
}

async fn build_store(
    backend: DedupBackend,
    ttl_seconds: u64,
    route_name: &str,
) -> anyhow::Result<Arc<dyn DedupStore>> {
    #[cfg(not(any(feature = "mongodb", feature = "sqlx")))]
    let _ = route_name;
    match backend {
        DedupBackend::Sled { path } => Ok(Arc::new(SledDedupStore::new(&path, ttl_seconds)?)),
        #[cfg(feature = "mongodb")]
        DedupBackend::Mongo {
            url,
            database,
            collection,
        } => {
            crate::endpoints::mongodb::build_mongo_dedup_store(
                &url,
                &database,
                collection,
                ttl_seconds,
                route_name,
            )
            .await
        }
        #[cfg(feature = "sqlx")]
        DedupBackend::Sqlx { url, table } => {
            crate::endpoints::sqlx::build_sql_dedup_store(&url, table, ttl_seconds, route_name)
                .await
        }
    }
}

/// Keys claimed by a batch that has not committed yet, and when they were claimed.
type Claims = HashMap<Vec<u8>, u64>;

const STATE_PENDING: u8 = 0;
const STATE_PROCESSED: u8 = 1;

/// The stored form of a committed key.
fn processed_value(now: u64) -> [u8; 9] {
    let mut value = [0u8; 9];
    value[0] = STATE_PROCESSED;
    value[1..9].copy_from_slice(&now.to_be_bytes());
    value
}

/// A poisoned lock is recovered rather than propagated: the map is still consistent, and
/// failing every later message would be far worse than the panic that poisoned it.
fn lock_claims(claims: &Mutex<Claims>) -> std::sync::MutexGuard<'_, Claims> {
    claims.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a stored value is still within its TTL. Values written by older versions are 8 bytes
/// (a bare timestamp) or carry the pending state, and are read on their own terms.
fn is_live(value: &[u8], now: u64, ttl_seconds: u64) -> bool {
    let (timestamp, ttl) = match value.len() {
        9 => {
            let ttl = if value[0] == STATE_PENDING {
                PENDING_TTL_SECS
            } else {
                ttl_seconds
            };
            match value[1..9].try_into() {
                Ok(bytes) => (u64::from_be_bytes(bytes), ttl),
                Err(_) => return false,
            }
        }
        8 => match value.try_into() {
            Ok(bytes) => (u64::from_be_bytes(bytes), ttl_seconds),
            Err(_) => return false,
        },
        _ => return false,
    };
    now.saturating_sub(timestamp) < ttl
}

/// Claim `key` against an already-held claim map. `Ok(true)` = a live entry already exists.
fn claim_key(
    db: &Db,
    claims: &mut Claims,
    key: &[u8],
    now: u64,
    ttl_seconds: u64,
) -> Result<bool, ConsumerError> {
    if claims
        .get(key)
        .is_some_and(|at| now.saturating_sub(*at) < PENDING_TTL_SECS)
    {
        return Ok(true);
    }
    let stored = db
        .get(key)
        .map_err(|e| ConsumerError::Connection(anyhow!("Deduplication DB error: {e}")))?;
    if stored
        .as_deref()
        .is_some_and(|v| is_live(v, now, ttl_seconds))
    {
        return Ok(true);
    }
    claims.insert(key.to_vec(), now);
    Ok(false)
}

/// Claim every key under one acquisition of the claim map.
fn claim_all(
    db: &Db,
    claims: &Mutex<Claims>,
    keys: &[Vec<u8>],
    now: u64,
    ttl_seconds: u64,
) -> Result<Vec<bool>, ConsumerError> {
    let mut held = lock_claims(claims);
    keys.iter()
        .map(|key| claim_key(db, &mut held, key, now, ttl_seconds))
        .collect()
}

fn commit_key(db: &Db, key: &[u8], now: u64) {
    if let Err(e) = db.insert(key, &processed_value(now)[..]) {
        error!(
            "Failed to update key {} as processed in deduplication DB: {}",
            hex_key(key),
            e
        );
    } else {
        trace!("Updated message as processed in deduplication DB");
    }
}

/// Persist every key, then release its claim. The claim outlives the write on purpose, so a
/// concurrent `reserve` never sees a key that is in neither the map nor the store.
fn commit_all(db: &Db, claims: &Mutex<Claims>, keys: &[Vec<u8>], now: u64) {
    for key in keys {
        commit_key(db, key, now);
    }
    let mut held = lock_claims(claims);
    for key in keys {
        held.remove(key.as_slice());
    }
}

/// Local, single-instance deduplication store backed by a Sled database.
///
/// Committed keys are stored as `[state, be_u64_timestamp]`, expire on read, and are swept
/// lazily by `maybe_cleanup`. Reservations — keys claimed by a batch that has not committed
/// yet — are held in memory rather than written to disk: sled takes an exclusive file lock, so
/// a reservation only ever has to be visible to this process, and a sled write costs about six
/// times a read. A crash drops every reservation, which is exactly what one is for — the
/// uncommitted messages are redelivered and reprocessed.
///
/// Sled's calls are synchronous and made inline: handing a batch to a blocking thread measured
/// slower than paying them where they are. They are not this middleware's main cost — rendering
/// the dedup key is, because a `${payload:...}` key parses the whole payload.
struct SledDedupStore {
    db: Arc<Db>,
    ttl_seconds: u64,
    in_flight: Arc<Mutex<Claims>>,
    last_cleanup: AtomicU64,
}

impl SledDedupStore {
    fn new(path: &str, ttl_seconds: u64) -> anyhow::Result<Self> {
        Ok(Self {
            db: Arc::new(sled::open(path)?),
            ttl_seconds,
            in_flight: Arc::new(Mutex::new(Claims::new())),
            last_cleanup: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl DedupStore for SledDedupStore {
    async fn reserve(&self, key: &[u8], now: u64) -> Result<bool, ConsumerError> {
        let mut held = lock_claims(&self.in_flight);
        claim_key(&self.db, &mut held, key, now, self.ttl_seconds)
    }

    async fn reserve_many(&self, keys: &[Vec<u8>], now: u64) -> Result<Vec<bool>, ConsumerError> {
        claim_all(&self.db, &self.in_flight, keys, now, self.ttl_seconds)
    }

    async fn mark_processed(&self, key: &[u8], now: u64) {
        commit_key(&self.db, key, now);
        lock_claims(&self.in_flight).remove(key);
    }

    async fn mark_processed_many(&self, keys: &[Vec<u8>], now: u64) {
        commit_all(&self.db, &self.in_flight, keys, now);
    }

    fn maybe_cleanup(&self, now: u64) {
        const CLEANUP_INTERVAL_SECS: u64 = 30;
        let last = self.last_cleanup.load(Ordering::Acquire);
        // The first call only starts the clock. Sweeping straight away would scan the whole
        // store before a single message had been handled.
        if last == 0 {
            let _ = self
                .last_cleanup
                .compare_exchange(0, now, Ordering::SeqCst, Ordering::Acquire);
            return;
        }
        if now.saturating_sub(last) <= CLEANUP_INTERVAL_SECS
            || self
                .last_cleanup
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        lock_claims(&self.in_flight).retain(|_, at| now.saturating_sub(*at) < PENDING_TTL_SECS);

        let db = self.db.clone();
        let ttl = self.ttl_seconds;
        tokio::task::spawn_blocking(move || {
            let cutoff = now.saturating_sub(ttl);
            for (key, value) in db.iter().flatten() {
                let offset = match value.len() {
                    9 => 1,
                    8 => 0,
                    _ => continue,
                };
                if let Ok(bytes) = value[offset..offset + 8].try_into() {
                    if u64::from_be_bytes(bytes) < cutoff {
                        let _ = db.compare_and_swap(&key, Some(value), None::<&[u8]>);
                    }
                }
            }
        });
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.db.flush_async().await?;
        Ok(())
    }
}

/// Hex-encode a dedup key for logging and for string-keyed backends.
pub(crate) fn hex_key(key: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(key.len() * 2);
    for b in key {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub struct DeduplicationConsumer {
    inner: Box<dyn MessageConsumer>,
    store: Arc<dyn DedupStore>,
    /// Compiled `key` template. `None` keys on `message_id`, which most sources
    /// regenerate per read — so a re-read of the same source dedupes nothing.
    key_template: Option<CompiledTemplate>,
    /// Commits for batches that were entirely duplicates. See
    /// [`crate::middleware::deferred_commit`].
    deferred: DeferredCommits,
}

/// The dedup key for a message: the rendered `key` template, or the raw `message_id`.
///
/// An unresolved selector renders empty, and an empty key would be shared by every message
/// missing that field — silently dropping all but the first. Such a message falls back to
/// `message_id` instead, so it is passed through rather than swallowed.
fn dedup_key(template: Option<&CompiledTemplate>, msg: &CanonicalMessage) -> Vec<u8> {
    match template {
        Some(t) => match t.render(Some(msg)) {
            key if key.is_empty() => {
                warn!(
                    message_id = %msg.message_id,
                    "Deduplication `key` template resolved to nothing; keying on message_id for this message"
                );
                msg.message_id.to_be_bytes().to_vec()
            }
            key => key,
        },
        None => msg.message_id.to_be_bytes().to_vec(),
    }
}

impl DeduplicationConsumer {
    pub async fn new(
        inner: Box<dyn MessageConsumer>,
        config: &DeduplicationMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        info!(
            "Deduplication Middleware enabled for route '{}' with TTL {}s",
            route_name, config.ttl_seconds
        );
        let backend = match (&config.store, &config.sled_path) {
            (Some(store), _) => parse_dedup_store(store)?,
            // Legacy `sled_path` is always a local path (never scheme-parsed).
            (None, Some(path)) => DedupBackend::Sled { path: path.clone() },
            (None, None) => {
                return Err(anyhow!(
                    "deduplication requires either `store` or `sled_path`"
                ))
            }
        };
        let store = build_store(backend, config.ttl_seconds, route_name).await?;
        let key_template = config
            .key
            .as_deref()
            .map(|k| CompiledTemplate::compile(k, None))
            .transpose()
            .context("invalid deduplication `key` template")?;
        Ok(Self {
            inner,
            store,
            key_template,
            deferred: DeferredCommits::new(),
        })
    }

    /// Test seam: wrap `inner` around a store the caller already holds, so a test can inspect
    /// the store's state directly. Reopening the same sled path instead would deadlock on its
    /// file lock.
    #[cfg(test)]
    pub(crate) fn with_store(
        inner: Box<dyn MessageConsumer>,
        store: Arc<dyn DedupStore>,
        key: Option<&str>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            store,
            key_template: key
                .map(|k| CompiledTemplate::compile(k, None))
                .transpose()?,
            deferred: DeferredCommits::new(),
        })
    }
}

#[async_trait]
impl MessageConsumer for DeduplicationConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        let inner_hook = self.inner.on_disconnect_hook();
        let store = self.store.clone();
        let held = self.deferred.take_shared();

        Some(Box::pin(async move {
            let mut first_error = None;
            if let Err(err) = run_all(held).await {
                first_error = Some(err);
            }
            if let Some(hook) = inner_hook {
                if let Err(err) = hook.await {
                    first_error.get_or_insert(err);
                }
            }
            if let Err(err) = store.flush().await {
                first_error.get_or_insert(err);
            }
            match first_error {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }))
    }

    #[instrument(skip_all)]
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            let received = self.inner.receive().await?;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_secs();

            self.store.maybe_cleanup(now);

            let key = dedup_key(self.key_template.as_ref(), &received.message);
            if self.store.reserve(&key, now).await? {
                info!(message_id = %format!("{:032x}", received.message.message_id), "Duplicate message detected and skipped");
                if let Err(e) = (received.commit)(MessageDisposition::Ack).await {
                    warn!("Failed to commit skipped duplicate message: {}", e);
                }
                continue;
            }

            let store = self.store.clone();
            let original_commit = received.commit;

            // Wrap commit to promote the reservation to "processed" state.
            let commit = Box::new(move |disposition: MessageDisposition| {
                Box::pin(async move {
                    let is_ack = matches!(
                        disposition,
                        MessageDisposition::Ack | MessageDisposition::Reply(_)
                    );
                    original_commit(disposition).await?;
                    if is_ack {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        store.mark_processed(&key, now).await;
                    }
                    Ok(())
                }) as crate::traits::BoxFuture<'static, anyhow::Result<()>>
            });

            return Ok(Received {
                message: received.message,
                commit,
            });
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let ReceivedBatch {
                messages,
                commit: inner_commit,
            } = self.inner.receive_batch(max_messages).await?;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| ConsumerError::Connection(anyhow::anyhow!(e)))?
                .as_secs();

            self.store.maybe_cleanup(now);

            // An empty inner batch is how a drained source signals `exit_on_empty`
            // (see `traits::drain_gated`). Looping on it here would spin forever on
            // an already-drained source, so pass it through untouched.
            if messages.is_empty() {
                run_all(self.deferred.take()).await.map_err(|error| {
                    ConsumerError::Connection(error.context(
                        "failed to flush deferred deduplication acknowledgements on drain",
                    ))
                })?;
                return Ok(ReceivedBatch {
                    messages,
                    commit: inner_commit,
                });
            }

            let total_len = messages.len();
            let keys: Vec<Vec<u8>> = messages
                .iter()
                .map(|msg| dedup_key(self.key_template.as_ref(), msg))
                .collect();
            let duplicates = self.store.reserve_many(&keys, now).await?;

            let mut filtered_messages = Vec::with_capacity(total_len);
            let mut kept_indices = Vec::with_capacity(total_len);
            let mut kept_keys: Vec<Vec<u8>> = Vec::with_capacity(total_len);

            for ((idx, msg), (key, duplicate)) in messages
                .into_iter()
                .enumerate()
                .zip(keys.into_iter().zip(duplicates))
            {
                if duplicate {
                    info!(message_id = %format!("{:032x}", msg.message_id), "Duplicate message detected and skipped");
                } else {
                    filtered_messages.push(msg);
                    kept_indices.push(idx);
                    kept_keys.push(key);
                }
            }

            if filtered_messages.is_empty() {
                let ordered = self.inner.commit_requires_order();
                self.deferred
                    .ack_emptied(ordered, inner_commit, total_len)
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }

            let held = self.deferred.take();
            let store = self.store.clone();

            let commit: crate::traits::BatchCommitFunc = Box::new(move |dispositions| {
                Box::pin(async move {
                    let mut full_dispositions = vec![MessageDisposition::Ack; total_len];
                    let mut acked = Vec::with_capacity(kept_keys.len());
                    for ((key, disposition), slot) in
                        kept_keys.into_iter().zip(dispositions).zip(kept_indices)
                    {
                        if matches!(
                            disposition,
                            MessageDisposition::Ack | MessageDisposition::Reply(_)
                        ) {
                            acked.push(key);
                        }
                        full_dispositions[slot] = disposition;
                    }

                    run_all(held).await?;
                    inner_commit(full_dispositions).await?;

                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    store.mark_processed_many(&acked, now).await;
                    Ok(())
                }) as crate::traits::BoxFuture<'static, anyhow::Result<()>>
            });

            return Ok(ReceivedBatch {
                messages: filtered_messages,
                commit,
            });
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
    use crate::models::DeduplicationMiddleware;
    use crate::CanonicalMessage;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_deduplication_logic() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("dedup_test").to_str().unwrap().to_string();

        let config = DeduplicationMiddleware {
            store: None,
            sled_path: Some(db_path),
            ttl_seconds: 60,
            key: None,
        };

        let mem_consumer = MemoryConsumer::new_local("dedup_topic", 10);
        let channel = mem_consumer.channel();

        let msg1 = CanonicalMessage::new(b"data1".to_vec(), Some(100));
        channel.send_message(msg1).await.unwrap();

        let msg2 = CanonicalMessage::new(b"data1_dup".to_vec(), Some(100));
        channel.send_message(msg2).await.unwrap();

        let msg3 = CanonicalMessage::new(b"data2".to_vec(), Some(101));
        channel.send_message(msg3).await.unwrap();

        let mut dedup_consumer =
            DeduplicationConsumer::new(Box::new(mem_consumer), &config, "test_route")
                .await
                .unwrap();

        // First receive: Should be msg1 (ID 100)
        let rec1 = dedup_consumer.receive().await.unwrap();
        assert_eq!(rec1.message.message_id, 100);
        let _ = (rec1.commit)(crate::traits::MessageDisposition::Ack).await;

        // Second receive: Should be msg3 (ID 101). msg2 (ID 100) is skipped internally.
        let rec2 = dedup_consumer.receive().await.unwrap();
        assert_eq!(rec2.message.message_id, 101);
        let _ = (rec2.commit)(crate::traits::MessageDisposition::Ack).await;
    }

    /// Regression: an empty inner batch is how a drained source signals `exit_on_empty`.
    /// The wrapper used to loop on it, so a route with `deduplication` on the input moved
    /// everything and then hung `healthy: true` forever instead of completing.
    #[tokio::test]
    async fn empty_inner_batch_is_passed_through_for_drain() {
        let dir = tempdir().unwrap();
        let config = DeduplicationMiddleware {
            store: None,
            sled_path: Some(dir.path().join("dedup_drain").to_str().unwrap().to_string()),
            ttl_seconds: 60,
            key: None,
        };

        let mut mem_consumer = MemoryConsumer::new_local("dedup_drain_topic", 10);
        mem_consumer.set_exit_on_empty(true);

        let mut dedup_consumer =
            DeduplicationConsumer::new(Box::new(mem_consumer), &config, "test_route")
                .await
                .unwrap();

        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            dedup_consumer.receive_batch(16),
        )
        .await
        .expect("receive_batch must return on a drained source, not loop forever")
        .unwrap();

        assert!(batch.messages.is_empty());
    }

    /// With a `key` template, dedup follows the payload, not `message_id` — which most
    /// sources regenerate per read, so a re-read of the same data would dedupe nothing.
    #[tokio::test]
    async fn key_template_dedupes_on_payload_not_message_id() {
        let dir = tempdir().unwrap();
        let config = DeduplicationMiddleware {
            store: None,
            sled_path: Some(dir.path().join("dedup_key").to_str().unwrap().to_string()),
            ttl_seconds: 60,
            key: Some("${payload:order_id}".to_string()),
        };

        let mem_consumer = MemoryConsumer::new_local("dedup_key_topic", 10);
        let channel = mem_consumer.channel();

        // Same order_id, different message_ids: the second must be suppressed.
        for id in [1u128, 2, 3] {
            let body = if id == 3 {
                br#"{"order_id":"B"}"#
            } else {
                br#"{"order_id":"A"}"#
            };
            channel
                .send_message(CanonicalMessage::new(body.to_vec(), Some(id)))
                .await
                .unwrap();
        }

        let mut dedup_consumer =
            DeduplicationConsumer::new(Box::new(mem_consumer), &config, "test_route")
                .await
                .unwrap();

        // Timed out rather than awaited bare: a key that fails to resolve makes every
        // message share one key, so the second `receive` would block forever.
        macro_rules! recv {
            () => {
                tokio::time::timeout(Duration::from_secs(10), dedup_consumer.receive())
                    .await
                    .expect("receive timed out — the key template resolved to a constant")
                    .unwrap()
            };
        }

        let first = recv!();
        assert_eq!(first.message.message_id, 1);
        let _ = (first.commit)(crate::traits::MessageDisposition::Ack).await;

        // message_id 2 carries order_id "A" again and is skipped internally.
        let second = recv!();
        assert_eq!(
            second.message.message_id, 3,
            "the second copy of order_id A must be suppressed by the key template"
        );
    }

    /// A message missing the keyed field renders an empty key. Keying on that would make
    /// every such message collide, so all but the first would vanish; they fall back to
    /// `message_id` and are passed through instead.
    #[tokio::test]
    async fn messages_missing_the_keyed_field_are_not_collapsed() {
        let dir = tempdir().unwrap();
        let config = DeduplicationMiddleware {
            store: None,
            sled_path: Some(dir.path().join("dedup_miss").to_str().unwrap().to_string()),
            ttl_seconds: 60,
            key: Some("${payload:order_id}".to_string()),
        };

        let mem_consumer = MemoryConsumer::new_local("dedup_miss_topic", 10);
        let channel = mem_consumer.channel();
        for id in [1u128, 2, 3] {
            channel
                .send_message(CanonicalMessage::new(br#"{"other":1}"#.to_vec(), Some(id)))
                .await
                .unwrap();
        }

        let mut dedup_consumer =
            DeduplicationConsumer::new(Box::new(mem_consumer), &config, "test_route")
                .await
                .unwrap();
        dedup_consumer.set_exit_on_empty(true);

        let batch = tokio::time::timeout(Duration::from_secs(10), dedup_consumer.receive_batch(16))
            .await
            .expect("receive_batch timed out")
            .unwrap();
        assert_eq!(
            batch.messages.len(),
            3,
            "an unresolvable key must not collapse distinct messages into one"
        );
    }

    // --- Store-level semantics ---
    //
    // `reserve` takes `now` explicitly, so expiry is tested by moving the clock rather than
    // sleeping. These pin the reserve/promote/expire contract the consumer relies on.

    fn sled_store(dir: &tempfile::TempDir, name: &str, ttl_seconds: u64) -> SledDedupStore {
        SledDedupStore::new(dir.path().join(name).to_str().unwrap(), ttl_seconds).unwrap()
    }

    #[tokio::test]
    async fn reserve_claims_a_key_once() {
        let dir = tempdir().unwrap();
        let store = sled_store(&dir, "once", 60);
        assert!(!store.reserve(b"k", 1000).await.unwrap(), "first is fresh");
        assert!(store.reserve(b"k", 1000).await.unwrap(), "second is a dup");
    }

    #[tokio::test]
    async fn distinct_keys_do_not_collide() {
        let dir = tempdir().unwrap();
        let store = sled_store(&dir, "distinct", 60);
        assert!(!store.reserve(b"a", 1000).await.unwrap());
        assert!(!store.reserve(b"b", 1000).await.unwrap());
        assert!(!store.reserve(b"", 1000).await.unwrap());
    }

    /// A reservation that is never committed frees itself, so a crash between reserve and
    /// commit leaves the message redeliverable instead of permanently swallowed.
    #[tokio::test]
    async fn an_uncommitted_reservation_expires_quickly() {
        let dir = tempdir().unwrap();
        let store = sled_store(&dir, "pending", 3600);
        assert!(!store.reserve(b"k", 1000).await.unwrap());
        assert!(
            store
                .reserve(b"k", 1000 + PENDING_TTL_SECS - 1)
                .await
                .unwrap(),
            "still held while the reservation is live"
        );
        assert!(
            !store
                .reserve(b"k", 1000 + PENDING_TTL_SECS + 1)
                .await
                .unwrap(),
            "a reservation outlived by its short TTL must be reclaimable"
        );
    }

    /// Committing promotes the key to the configured TTL, which is what makes it a duplicate
    /// long after the short reservation window has passed.
    #[tokio::test]
    async fn mark_processed_promotes_to_the_full_ttl() {
        let dir = tempdir().unwrap();
        let store = sled_store(&dir, "promote", 3600);
        assert!(!store.reserve(b"k", 1000).await.unwrap());
        store.mark_processed(b"k", 1000).await;
        assert!(
            store
                .reserve(b"k", 1000 + PENDING_TTL_SECS + 1)
                .await
                .unwrap(),
            "a committed key must outlive the reservation TTL"
        );
        assert!(store.reserve(b"k", 1000 + 3599).await.unwrap());
        assert!(
            !store.reserve(b"k", 1000 + 3601).await.unwrap(),
            "past its TTL the key is reclaimable again"
        );
    }

    // --- Batch path ---

    fn label(disposition: &MessageDisposition) -> &'static str {
        match disposition {
            MessageDisposition::Ack => "ack",
            MessageDisposition::Nack => "nack",
            MessageDisposition::Reply(_) => "reply",
        }
    }

    type Committed = Arc<std::sync::Mutex<Vec<Vec<&'static str>>>>;

    /// Hands out prepared batches and records what its commit was called with, so the
    /// dispositions the wrapper passes *inward* can be asserted on.
    struct RecordingConsumer {
        batches: std::collections::VecDeque<Vec<CanonicalMessage>>,
        committed: Committed,
    }

    #[async_trait]
    impl MessageConsumer for RecordingConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            let messages = self.batches.pop_front().unwrap_or_default();
            let committed = self.committed.clone();
            let commit: crate::traits::BatchCommitFunc = Box::new(move |dispositions| {
                committed
                    .lock()
                    .unwrap()
                    .push(dispositions.iter().map(label).collect());
                Box::pin(async { Ok(()) }) as crate::traits::BoxFuture<'static, anyhow::Result<()>>
            });
            Ok(ReceivedBatch { messages, commit })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// A message whose `id` field drives the `${payload:id}` key template.
    fn keyed(id: &str, message_id: u128) -> CanonicalMessage {
        CanonicalMessage::new(
            format!(r#"{{"id":"{id}","n":{message_id}}}"#).into_bytes(),
            Some(message_id),
        )
    }

    async fn dedup_over(
        dir: &tempfile::TempDir,
        name: &str,
        batches: Vec<Vec<CanonicalMessage>>,
    ) -> (DeduplicationConsumer, Committed) {
        let config = DeduplicationMiddleware {
            store: None,
            sled_path: Some(dir.path().join(name).to_str().unwrap().to_string()),
            ttl_seconds: 3600,
            key: Some("${payload:id}".to_string()),
        };
        let committed: Committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = RecordingConsumer {
            batches: batches.into(),
            committed: committed.clone(),
        };
        let consumer = DeduplicationConsumer::new(Box::new(inner), &config, "test_route")
            .await
            .unwrap();
        (consumer, committed)
    }

    #[tokio::test]
    async fn duplicates_within_one_batch_are_suppressed() {
        let dir = tempdir().unwrap();
        let (mut consumer, _) = dedup_over(
            &dir,
            "in_batch",
            vec![vec![
                keyed("A", 1),
                keyed("A", 2),
                keyed("B", 3),
                keyed("A", 4),
            ]],
        )
        .await;

        let batch = consumer.receive_batch(16).await.unwrap();
        let ids: Vec<u128> = batch.messages.iter().map(|m| m.message_id).collect();
        assert_eq!(
            ids,
            vec![1, 3],
            "only the first of each key survives, in order"
        );
        assert_eq!(
            batch.messages[0].payload.as_ref(),
            keyed("A", 1).payload.as_ref(),
            "the surviving message keeps its own payload"
        );
    }

    /// The wrapper hands the inner consumer a full-width disposition vector: the caller's
    /// choices at the slots they came from, and `Ack` for the duplicates it dropped — which
    /// is what stops a suppressed duplicate from being redelivered forever.
    #[tokio::test]
    async fn dispositions_are_remapped_to_the_inner_batch_slots() {
        let dir = tempdir().unwrap();
        let (mut consumer, committed) = dedup_over(
            &dir,
            "remap",
            vec![vec![keyed("A", 1), keyed("A", 2), keyed("B", 3)]],
        )
        .await;

        let batch = consumer.receive_batch(16).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        (batch.commit)(vec![MessageDisposition::Nack, MessageDisposition::Ack])
            .await
            .unwrap();

        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [vec!["nack", "ack", "ack"]],
            "kept slots carry the caller's disposition; the dropped duplicate is acked"
        );
    }

    /// A batch of nothing but duplicates must not surface as an empty batch — that is the
    /// drain signal — so the wrapper skips it and fetches again. On an ordered source its
    /// ack is held back and released just before the next retained batch commits.
    #[tokio::test]
    async fn an_all_duplicate_batch_is_acked_and_retried() {
        let dir = tempdir().unwrap();
        let (mut consumer, committed) = dedup_over(
            &dir,
            "all_dup",
            vec![
                vec![keyed("A", 1)],
                vec![keyed("A", 2), keyed("A", 3)],
                vec![keyed("B", 4)],
            ],
        )
        .await;

        let first = consumer.receive_batch(16).await.unwrap();
        assert_eq!(first.messages.len(), 1);
        (first.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        let second = tokio::time::timeout(Duration::from_secs(10), consumer.receive_batch(16))
            .await
            .expect("an all-duplicate batch must not stall the consumer")
            .unwrap();
        assert_eq!(
            second
                .messages
                .iter()
                .map(|m| m.message_id)
                .collect::<Vec<_>>(),
            vec![4],
            "the all-duplicate batch is skipped and the next one delivered"
        );
        assert_eq!(
            committed.lock().unwrap().len(),
            1,
            "the skipped batch must not ack ahead of the ordered sequencer"
        );

        (second.commit)(vec![MessageDisposition::Ack])
            .await
            .unwrap();
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [vec!["ack"], vec!["ack", "ack"], vec!["ack"]],
            "every message of the skipped batch is acked, in front of the batch that followed it"
        );
    }

    /// An ordered source may drain immediately after an all-duplicate batch. Its deferred
    /// acknowledgement must be flushed even though no retained batch follows it.
    #[tokio::test]
    async fn a_final_all_duplicate_batch_is_acked_when_the_source_drains() {
        let dir = tempdir().unwrap();
        let (mut consumer, committed) = dedup_over(
            &dir,
            "final_all_dup",
            vec![vec![keyed("A", 1)], vec![keyed("A", 2)]],
        )
        .await;

        let first = consumer.receive_batch(16).await.unwrap();
        (first.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        let drained = consumer.receive_batch(16).await.unwrap();
        assert!(drained.messages.is_empty());
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [vec!["ack"], vec!["ack"]],
            "the duplicate-only final batch is acknowledged before drain returns"
        );
    }

    /// Only an ack promotes the reservation. A nacked message leaves its key on the short
    /// reservation TTL, so a redelivery is reprocessed rather than silently dropped.
    #[tokio::test]
    async fn a_nacked_message_is_not_marked_processed() {
        let dir = tempdir().unwrap();
        let store: Arc<dyn DedupStore> = Arc::new(sled_store(&dir, "nack_state", 3600));
        let committed: Committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = RecordingConsumer {
            batches: vec![vec![keyed("A", 1), keyed("B", 2)]].into(),
            committed,
        };
        let mut consumer = DeduplicationConsumer::with_store(
            Box::new(inner),
            store.clone(),
            Some("${payload:id}"),
        )
        .unwrap();

        let batch = consumer.receive_batch(16).await.unwrap();
        (batch.commit)(vec![MessageDisposition::Nack, MessageDisposition::Ack])
            .await
            .unwrap();

        // Look past the reservation window: the acked key was promoted and still blocks, the
        // nacked one has released.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + PENDING_TTL_SECS
            + 1;
        assert!(
            !store.reserve(b"A", now).await.unwrap(),
            "a nacked key must be reclaimable"
        );
        assert!(
            store.reserve(b"B", now).await.unwrap(),
            "an acked key stays claimed for the configured TTL"
        );
    }

    #[test]
    fn parse_sled_and_bare_paths() {
        assert!(matches!(
            parse_dedup_store("sled:///var/lib/dedup").unwrap(),
            DedupBackend::Sled { path } if path == "/var/lib/dedup"
        ));
        assert!(matches!(
            parse_dedup_store("/var/lib/dedup").unwrap(),
            DedupBackend::Sled { path } if path == "/var/lib/dedup"
        ));
    }
}
