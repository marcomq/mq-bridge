//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! In-process deduplication store, selected by a `memory://[name][?max_keys=N]` `store:` URL.

use super::{DedupStore, Reservation, PENDING_TTL_SECS};
use crate::traits::ConsumerError;
use async_trait::async_trait;
use hashbrown::{DefaultHashBuilder, HashTable};
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tracing::warn;

const SHARDS: usize = 16;

/// Keys kept when `max_keys` is not given.
pub(crate) const DEFAULT_MAX_KEYS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Processed,
    /// Released, superseded or expired; its queue slot waits to reach the front.
    Dropped,
}

struct Entry {
    key: Box<[u8]>,
    at: u64,
    state: State,
}

impl Entry {
    fn live(&self, now: u64, ttl: u64) -> bool {
        let age = now.saturating_sub(self.at);
        match self.state {
            State::Pending => age < PENDING_TTL_SECS,
            State::Processed => age < ttl,
            State::Dropped => false,
        }
    }
}

fn hash_key(hasher: &DefaultHashBuilder, key: &[u8]) -> u64 {
    hasher.hash_one(key)
}

/// Entries in claim order, indexed by key. The hash only picks the bucket: a match is always
/// confirmed on the full key bytes. The TTL is the same for every key, so expired entries
/// collect at the front of the queue and are dropped from there without a scan.
#[derive(Default)]
struct Shard {
    entries: VecDeque<Entry>,
    /// Sequence number of `entries[0]`.
    head: u64,
    /// Sequence numbers of the entries not yet dropped.
    index: HashTable<u64>,
    /// Stored replies for `replay_response`, by sequence number.
    responses: HashMap<u64, Box<[u8]>>,
}

impl Shard {
    fn entry_mut(&mut self, seq: u64) -> &mut Entry {
        let slot = (seq - self.head) as usize;
        &mut self.entries[slot]
    }

    fn find(&self, hash: u64, key: &[u8]) -> Option<u64> {
        let (entries, head) = (&self.entries, self.head);
        self.index
            .find(hash, |&seq| &*entries[(seq - head) as usize].key == key)
            .copied()
    }

    fn unindex(&mut self, hash: u64, seq: u64) {
        if let Ok(found) = self.index.find_entry(hash, |&s| s == seq) {
            found.remove();
        }
        self.responses.remove(&seq);
    }

    fn drop_entry(&mut self, hash: u64, seq: u64) {
        self.unindex(hash, seq);
        self.entry_mut(seq).state = State::Dropped;
    }

    fn push(
        &mut self,
        hasher: &DefaultHashBuilder,
        hash: u64,
        key: &[u8],
        now: u64,
        state: State,
    ) -> u64 {
        let seq = self.head + self.entries.len() as u64;
        self.entries.push_back(Entry {
            key: key.into(),
            at: now,
            state,
        });
        let (entries, head) = (&self.entries, self.head);
        self.index.insert_unique(hash, seq, |&s| {
            hash_key(hasher, &entries[(s - head) as usize].key)
        });
        seq
    }

    /// Drops expired entries from the front, and live ones while more than `max` keys are
    /// indexed. Dropped slots count only past `2 * max`, which bounds memory under heavy
    /// release/re-claim churn. Returns whether a live key was evicted.
    fn collect(&mut self, hasher: &DefaultHashBuilder, now: u64, ttl: u64, max: usize) -> bool {
        let mut evicted = false;
        while let Some(front) = self.entries.front() {
            let live = front.live(now, ttl);
            if live && self.index.len() <= max && self.entries.len() <= 2 * max {
                break;
            }
            evicted |= live;
            if front.state != State::Dropped {
                let hash = hash_key(hasher, &front.key);
                self.unindex(hash, self.head);
            }
            self.entries.pop_front();
            self.head += 1;
        }
        evicted
    }
}

/// A deduplication store held in process memory, shared by name across reconnects and
/// redeploys. Keys are compared exactly. A restart forgets every key, so redeliveries after a
/// restart are processed again; within a process it keeps the same claim/commit states as the
/// persistent stores.
pub(crate) struct MemoryDedupStore {
    shards: Box<[Mutex<Shard>]>,
    hasher: DefaultHashBuilder,
    ttl_seconds: AtomicU64,
    max_keys: AtomicUsize,
    warned_eviction: AtomicBool,
}

impl MemoryDedupStore {
    fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            hasher: DefaultHashBuilder::default(),
            ttl_seconds: AtomicU64::new(0),
            max_keys: AtomicUsize::new(DEFAULT_MAX_KEYS),
            warned_eviction: AtomicBool::new(false),
        }
    }

    /// Locks the key's shard after dropping its expired entries.
    fn shard(&self, hash: u64, now: u64) -> MutexGuard<'_, Shard> {
        let mut shard = self.shards[(hash >> 48) as usize % SHARDS]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let max = (self.max_keys.load(Ordering::Relaxed) / SHARDS).max(1);
        if shard.collect(&self.hasher, now, self.ttl(), max)
            && !self.warned_eviction.swap(true, Ordering::Relaxed)
        {
            warn!(
                max_keys = self.max_keys.load(Ordering::Relaxed),
                "In-memory deduplication store is full; evicting keys before their TTL (later copies of them are no longer caught)"
            );
        }
        shard
    }

    fn ttl(&self) -> u64 {
        self.ttl_seconds.load(Ordering::Relaxed)
    }

    /// Whether every key has expired or been dropped, so a fresh store would behave the same.
    fn is_idle(&self, now: u64) -> bool {
        (0..SHARDS).all(|i| {
            let mut shard = self.shards[i].lock().unwrap_or_else(|e| e.into_inner());
            shard.collect(&self.hasher, now, self.ttl(), usize::MAX / 2);
            shard.index.is_empty()
        })
    }

    fn reserve_key(&self, key: &[u8], now: u64) -> Reservation {
        let hash = hash_key(&self.hasher, key);
        let mut shard = self.shard(hash, now);
        if let Some(seq) = shard.find(hash, key) {
            let entry = shard.entry_mut(seq);
            if entry.live(now, self.ttl()) {
                return match entry.state {
                    State::Pending => Reservation::InFlight,
                    _ => Reservation::Processed,
                };
            }
            // Superseded rather than reused, so the queue stays in claim order.
            shard.drop_entry(hash, seq);
        }
        shard.push(&self.hasher, hash, key, now, State::Pending);
        Reservation::Claimed
    }

    fn mark_key(&self, key: &[u8], now: u64, response: Option<&[u8]>) {
        let hash = hash_key(&self.hasher, key);
        let mut shard = self.shard(hash, now);
        let seq = match shard.find(hash, key) {
            Some(seq) => {
                let entry = shard.entry_mut(seq);
                entry.state = State::Processed;
                entry.at = now;
                seq
            }
            // The claim lapsed and was collected: record the marker afresh.
            None => shard.push(&self.hasher, hash, key, now, State::Processed),
        };
        match response {
            Some(reply) => shard.responses.insert(seq, reply.into()),
            None => shard.responses.remove(&seq),
        };
    }

    fn renew_key(&self, key: &[u8], now: u64) {
        let hash = hash_key(&self.hasher, key);
        let mut shard = self.shard(hash, now);
        if let Some(seq) = shard.find(hash, key) {
            let entry = shard.entry_mut(seq);
            if entry.state == State::Pending {
                entry.at = now;
            }
        }
    }

    fn release_key(&self, key: &[u8], now: u64) {
        let hash = hash_key(&self.hasher, key);
        let mut shard = self.shard(hash, now);
        if let Some(seq) = shard.find(hash, key) {
            if shard.entry_mut(seq).state == State::Pending {
                shard.drop_entry(hash, seq);
            }
        }
    }
}

fn unix_now() -> u64 {
    super::unix_now()
}

#[async_trait]
impl DedupStore for MemoryDedupStore {
    async fn reserve(&self, key: &[u8], now: u64) -> Result<Reservation, ConsumerError> {
        Ok(self.reserve_key(key, now))
    }

    async fn reserve_many(
        &self,
        keys: &[Vec<u8>],
        now: u64,
    ) -> Result<Vec<Reservation>, ConsumerError> {
        Ok(keys.iter().map(|key| self.reserve_key(key, now)).collect())
    }

    async fn renew_many(&self, keys: &[Vec<u8>], now: u64) {
        for key in keys {
            self.renew_key(key, now);
        }
    }

    async fn mark_processed(&self, key: &[u8], now: u64) {
        self.mark_key(key, now, None);
    }

    async fn mark_processed_many(&self, keys: &[Vec<u8>], now: u64) {
        for key in keys {
            self.mark_key(key, now, None);
        }
    }

    async fn mark_processed_with_response(&self, key: &[u8], now: u64, response: &[u8]) {
        self.mark_key(key, now, Some(response));
    }

    async fn stored_response(&self, key: &[u8]) -> Option<Vec<u8>> {
        let now = unix_now();
        let hash = hash_key(&self.hasher, key);
        let mut shard = self.shard(hash, now);
        let seq = shard.find(hash, key)?;
        let entry = shard.entry_mut(seq);
        if entry.state != State::Processed || !entry.live(now, self.ttl()) {
            return None;
        }
        shard.responses.get(&seq).map(|reply| reply.to_vec())
    }

    async fn release(&self, key: &[u8]) {
        self.release_key(key, unix_now());
    }

    async fn release_many(&self, keys: &[Vec<u8>]) {
        let now = unix_now();
        for key in keys {
            self.release_key(key, now);
        }
    }
}

/// The store named `name`, created on first use. Later callers share it and apply their own
/// `ttl_seconds` and `max_keys`, so a redeployed route keeps its keys under its new settings.
pub(crate) fn memory_dedup_store(
    name: &str,
    ttl_seconds: u64,
    max_keys: usize,
) -> Arc<MemoryDedupStore> {
    static STORES: OnceLock<Mutex<HashMap<String, Arc<MemoryDedupStore>>>> = OnceLock::new();
    let mut stores = STORES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // A store no route holds is dropped only once it holds no live key, so a redeploy that
    // comes later still finds its keys.
    let now = unix_now();
    stores.retain(|n, store| n == name || Arc::strong_count(store) > 1 || !store.is_idle(now));
    let store = stores
        .entry(name.to_string())
        .or_insert_with(|| Arc::new(MemoryDedupStore::new()));
    store.ttl_seconds.store(ttl_seconds, Ordering::Relaxed);
    store.max_keys.store(max_keys, Ordering::Relaxed);
    store.clone()
}

/// Parses `memory:`, `memory://name` and `memory://[name]?max_keys=N` into a name (empty for
/// the route's own) and a key limit.
pub(crate) fn parse_memory_store(spec: &str) -> anyhow::Result<(String, usize)> {
    // The scheme is matched case-insensitively by the caller, so only strip what follows it.
    let rest = spec
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut max_keys = DEFAULT_MAX_KEYS;
    for param in query.split('&').filter(|p| !p.is_empty()) {
        match param.split_once('=') {
            Some(("max_keys", value)) => {
                max_keys = value
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| anyhow::anyhow!("invalid max_keys '{value}' in '{spec}'"))?;
            }
            _ => anyhow::bail!("unknown parameter '{param}' in deduplication store '{spec}'"),
        }
    }
    Ok((name.trim_end_matches('/').to_string(), max_keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Reservation::{Claimed, InFlight, Processed};

    fn store(ttl: u64, max_keys: usize) -> MemoryDedupStore {
        let store = MemoryDedupStore::new();
        store.ttl_seconds.store(ttl, Ordering::Relaxed);
        store.max_keys.store(max_keys, Ordering::Relaxed);
        store
    }

    #[test]
    fn states_follow_claim_commit_release_and_expiry() {
        let store = store(60, DEFAULT_MAX_KEYS);
        let now = 1_000;
        assert_eq!(store.reserve_key(b"k", now), Claimed);
        assert_eq!(store.reserve_key(b"k", now), InFlight);
        store.release_key(b"k", now);
        assert_eq!(store.reserve_key(b"k", now), Claimed);
        store.mark_key(b"k", now, None);
        assert_eq!(store.reserve_key(b"k", now + 59), Processed);
        store.release_key(b"k", now);
        assert_eq!(
            store.reserve_key(b"k", now + 59),
            Processed,
            "release only drops a claim"
        );
        assert_eq!(store.reserve_key(b"k", now + 60), Claimed, "TTL elapsed");
        // A crashed holder's claim lapses after the lease.
        assert_eq!(store.reserve_key(b"c", now), Claimed);
        assert_eq!(store.reserve_key(b"c", now + PENDING_TTL_SECS), Claimed);
    }

    #[test]
    fn keys_match_exactly_and_prefixes_do_not() {
        let store = store(60, DEFAULT_MAX_KEYS);
        assert_eq!(store.reserve_key(b"order-1", 1), Claimed);
        store.mark_key(b"order-1", 1, None);
        for other in [&b"order-"[..], b"order-10", b"order-1 ", b""] {
            assert_eq!(store.reserve_key(other, 1), Claimed, "{other:?}");
        }
        assert_eq!(store.reserve_key(b"order-1", 1), Processed);
    }

    #[test]
    fn expired_entries_are_collected_from_the_front() {
        let store = store(10, DEFAULT_MAX_KEYS);
        for i in 0..10_000u32 {
            store.reserve_key(&i.to_be_bytes(), 100);
            store.mark_key(&i.to_be_bytes(), 100, None);
        }
        // One key per shard touched later collects every expired entry in it.
        for i in 0..10_000u32 {
            store.reserve_key(&(1_000_000 + i).to_be_bytes(), 111);
        }
        let held: usize = store
            .shards
            .iter()
            .map(|s| s.lock().unwrap().index.len())
            .sum();
        let queued: usize = store
            .shards
            .iter()
            .map(|s| s.lock().unwrap().entries.len())
            .sum();
        assert_eq!((held, queued), (10_000, 10_000));
        assert_eq!(store.reserve_key(&5u32.to_be_bytes(), 111), Claimed);
    }

    #[test]
    fn max_keys_bounds_every_shard() {
        let store = store(3600, SHARDS * 4);
        for i in 0..10_000u32 {
            store.reserve_key(&i.to_be_bytes(), 1);
            store.mark_key(&i.to_be_bytes(), 1, None);
        }
        for shard in store.shards.iter() {
            let shard = shard.lock().unwrap();
            assert!(shard.entries.len() <= 5, "{}", shard.entries.len());
        }
        assert_eq!(store.reserve_key(&9_999u32.to_be_bytes(), 1), Processed);
    }

    #[test]
    fn released_claims_do_not_count_against_max_keys() {
        // ~380 released claims per shard: past `max` (256), within the `2 * max` slot bound.
        let store = store(3600, SHARDS * 256);
        let keys: Vec<[u8; 4]> = (0..6_096u32).map(u32::to_be_bytes).collect();
        let (live, churn) = keys.split_at(16);
        for key in live {
            store.reserve_key(key, 1);
            store.mark_key(key, 1, None);
        }
        for key in churn {
            store.reserve_key(key, 1);
            store.release_key(key, 1);
        }
        assert!(!store.warned_eviction.load(Ordering::Relaxed));
        for key in live {
            assert_eq!(store.reserve_key(key, 1), Processed);
        }
        for shard in store.shards.iter() {
            assert!(shard.lock().unwrap().entries.len() <= 2 * 256 + 1);
        }
    }

    #[tokio::test]
    async fn replies_follow_the_latest_commit() {
        let store = store(60, DEFAULT_MAX_KEYS);
        let now = unix_now();
        store.reserve_key(b"k", now);
        store.mark_key(b"k", now, Some(b"reply"));
        assert_eq!(
            store.stored_response(b"k").await.as_deref(),
            Some(&b"reply"[..])
        );
        store.mark_key(b"k", now, None);
        assert_eq!(store.stored_response(b"k").await, None);
    }

    #[test]
    fn stores_are_shared_by_name_and_take_the_latest_settings() {
        let a = memory_dedup_store("memory_store_test_shared", 60, 100);
        a.reserve_key(b"k", 1);
        a.mark_key(b"k", 1, None);
        let b = memory_dedup_store("memory_store_test_shared", 5, 100);
        assert_eq!(b.reserve_key(b"k", 5), Processed);
        assert_eq!(a.reserve_key(b"k", 6), Claimed, "the new TTL applies");
    }

    #[test]
    fn unused_stores_are_pruned_once_their_keys_expire() {
        let key = |name: &str| memory_dedup_store(name, 60, 100);
        let now = unix_now();
        let held = key("memory_store_test_held");
        held.reserve_key(b"k", now);
        held.mark_key(b"k", now, None);
        // Unreferenced but with a live key: kept, so a redeploy finds it.
        let unused = key("memory_store_test_unused");
        unused.reserve_key(b"k", now);
        unused.mark_key(b"k", now, None);
        let weak = Arc::downgrade(&unused);
        drop(unused);
        // Unreferenced and empty: pruned by the next lookup.
        let empty = Arc::downgrade(&key("memory_store_test_empty"));
        drop(key("memory_store_test_other"));
        assert!(empty.upgrade().is_none(), "an idle unused store is pruned");
        assert_eq!(
            key("memory_store_test_unused").reserve_key(b"k", now),
            Processed,
            "an unused store with live keys survives"
        );
        assert!(weak.upgrade().is_some());
        assert_eq!(held.reserve_key(b"k", now), Processed);
    }

    #[test]
    fn parses_names_and_limits() {
        assert_eq!(
            parse_memory_store("memory:").unwrap(),
            (String::new(), DEFAULT_MAX_KEYS)
        );
        assert_eq!(
            parse_memory_store("memory://orders?max_keys=500").unwrap(),
            ("orders".to_string(), 500)
        );
        assert_eq!(
            parse_memory_store("memory://?max_keys=7").unwrap(),
            (String::new(), 7)
        );
        assert_eq!(
            parse_memory_store("MEMORY://Orders").unwrap(),
            ("Orders".to_string(), DEFAULT_MAX_KEYS)
        );
        assert!(parse_memory_store("memory://x?max_keys=0").is_err());
        assert!(parse_memory_store("memory://x?ttl=5").is_err());
    }
}
