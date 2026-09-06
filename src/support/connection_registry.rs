//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Process-wide cache of shared transport clients.
//!
//! Many publishers (and consumers) target the same broker/server. Their underlying
//! transport clients — an rdkafka `FutureProducer`, an `async_nats::Client`, a SQLx
//! pool — are designed to be shared across topics/subjects and threads. Without a
//! cache, each route builds its own, multiplying TCP connections, background threads,
//! and buffers.
//!
//! This module keeps one shared client per *connection identity* (URL + auth + TLS +
//! client-level options — never topic/subject/collection/queue). Entries are held as
//! `Weak` references, so a shared client (and any side-effecting `Drop`, e.g. a Kafka
//! flush) is released once the last holder is dropped.
//!
//! Callers opt out with `shared = false`, which always builds a fresh, un-cached client
//! — this is also how a latency-sensitive Kafka publisher gets its own producer queue.

use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

/// `(tag, identity)` where `identity` is the SHA-256 hex digest of the full structural
/// connection identity (see [`connection_identity`]). Two distinct connections sharing a
/// cache entry would therefore require a SHA-256 collision.
type Key = (&'static str, String);

static REGISTRY: OnceLock<RwLock<HashMap<Key, Weak<dyn Any + Send + Sync>>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<Key, Weak<dyn Any + Send + Sync>>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Per-key async locks that serialize first-time construction so concurrent cold
/// callers don't each run `build` and open duplicate connections.
static INFLIGHT: OnceLock<Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

fn inflight() -> &'static Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>> {
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build the canonical connection identity used in the cache key from any
/// `Debug`able value.
///
/// The `Debug` representation can carry secrets (URL passwords, TLS key passwords), and
/// keys live in the process-wide `REGISTRY`/`INFLIGHT` maps where they could leak via key
/// `Debug` output. So the debug encoding is passed through SHA-256 rather than stored
/// verbatim: the returned identity binds the same connection to the same key without ever
/// retaining the plaintext, and SHA-256's collision resistance keeps distinct connections
/// from sharing a cache entry.
///
/// Pass only fields that determine which underlying client is appropriate, e.g.
/// `(url, username, &tls)`. Two callers with the same tag and identity share a client.
///
/// The encoding is order-sensitive: any map-like field (e.g. a `Vec` of option
/// pairs) must be canonicalized — typically sorted — by the caller first, so that
/// configs that differ only in field order still resolve to the same client.
pub fn connection_identity<H: Debug>(identity: H) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{identity:?}").as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Return the cached shared client of type `T` for `(tag, identity)`, or build one with
/// `build` and cache it.
///
/// When `shared` is false, always builds a fresh client and does not touch the cache.
/// The build runs without holding the registry lock across `.await`; a concurrent build
/// racing on the same key resolves by preferring whichever entry is already live.
pub async fn get_or_create<T, F, Fut>(
    tag: &'static str,
    identity: String,
    shared: bool,
    build: F,
) -> anyhow::Result<Arc<T>>
where
    T: Send + Sync + 'static,
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    if !shared {
        return Ok(Arc::new(build().await?));
    }

    let key = (tag, identity);

    if let Some(existing) = lookup::<T>(&key) {
        return Ok(existing);
    }

    // Serialize construction per key: only one builder runs at a time, so racing
    // cold callers wait here and then find the cached client instead of each
    // opening its own connection.
    let build_lock = {
        let mut inflight = inflight()
            .lock()
            .expect("connection registry inflight lock poisoned");
        Arc::clone(inflight.entry(key.clone()).or_default())
    };
    let _build_guard = build_lock.lock().await;

    // A builder that won the race may have cached a live client while we waited.
    if let Some(existing) = lookup::<T>(&key) {
        return Ok(existing);
    }

    // Build outside the registry lock — never await while holding it.
    let built: Arc<T> = match build().await {
        Ok(client) => Arc::new(client),
        Err(err) => {
            // Build failed: nothing to cache. Remove our INFLIGHT entry so keys that
            // never build don't accumulate dead locks — but only if it's still our
            // lock and no other caller is queued or holds a clone (strong_count == 2:
            // our `build_lock` plus the map's). When callers are serialized behind us
            // we leave the entry so they keep coordinating on the same lock and retry
            // the build once we release the guard.
            let mut inflight = inflight()
                .lock()
                .expect("connection registry inflight lock poisoned");
            if inflight
                .get(&key)
                .is_some_and(|queued| Arc::ptr_eq(queued, &build_lock))
                && Arc::strong_count(&build_lock) == 2
            {
                inflight.remove(&key);
            }
            return Err(err);
        }
    };

    let resolved = {
        let mut map = registry()
            .write()
            .expect("connection registry lock poisoned");
        // Opportunistically drop keys whose shared client has been released, so the
        // registry doesn't accumulate dead `Weak` entries over the process lifetime.
        map.retain(|_, weak| weak.strong_count() > 0);
        // Another task may have inserted a live client while we were building.
        match map
            .get(&key)
            .and_then(Weak::upgrade)
            .and_then(|arc| arc.downcast::<T>().ok())
        {
            Some(existing) => existing,
            None => {
                map.insert(
                    key.clone(),
                    Arc::downgrade(&(built.clone() as Arc<dyn Any + Send + Sync>)),
                );
                built
            }
        }
    };

    // The client is now cached, so future callers resolve via `lookup` without the
    // build lock. Release our guard and drop the per-key INFLIGHT entry so the map
    // doesn't accumulate dead locks. Callers already blocked on this lock still hold
    // their own `Arc` clone and, once they wake, find the cached client.
    drop(_build_guard);
    inflight()
        .lock()
        .expect("connection registry inflight lock poisoned")
        .remove(&key);

    Ok(resolved)
}

fn lookup<T: Send + Sync + 'static>(key: &Key) -> Option<Arc<T>> {
    let map = registry()
        .read()
        .expect("connection registry lock poisoned");
    map.get(key)
        .and_then(Weak::upgrade)
        .and_then(|arc| arc.downcast::<T>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn build_client(value: u32) -> anyhow::Result<u32> {
        Ok(value)
    }

    #[tokio::test]
    async fn shared_key_returns_same_arc_and_builds_once() {
        let builds = Arc::new(AtomicUsize::new(0));
        let id = connection_identity(("broker:9092", "alice"));
        let make = |value: u32| {
            let builds = builds.clone();
            move || {
                let builds = builds.clone();
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(value)
                }
            }
        };

        let a = get_or_create("test-share", id.clone(), true, make(1))
            .await
            .unwrap();
        let b = get_or_create("test-share", id, true, make(2))
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a, 1); // second build is discarded; first client wins
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_callers_build_once() {
        let builds = Arc::new(AtomicUsize::new(0));
        let id = connection_identity(("broker:9092", "concurrent"));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let builds = builds.clone();
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                get_or_create("test-concurrent", id, true, || {
                    let builds = builds.clone();
                    async move {
                        builds.fetch_add(1, Ordering::SeqCst);
                        // Hold the build open so racing callers pile up behind it.
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok(42u32)
                    }
                })
                .await
                .unwrap()
            }));
        }

        let first = handles.pop().unwrap().await.unwrap();
        for h in handles {
            assert!(Arc::ptr_eq(&first, &h.await.unwrap()));
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn not_shared_always_builds_fresh() {
        let id = connection_identity(("broker:9092", "bob"));
        let a = get_or_create("test-dedicated", id.clone(), false, || build_client(1))
            .await
            .unwrap();
        let b = get_or_create("test-dedicated", id, false, || build_client(1))
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn entry_released_after_last_arc_drops() {
        let id = connection_identity(("broker:9092", "carol"));
        let a = get_or_create("test-release", id.clone(), true, || build_client(7))
            .await
            .unwrap();
        let key = ("test-release", id);
        assert!(lookup::<u32>(&key).is_some());
        drop(a);
        // Weak no longer upgrades once the last strong reference is gone.
        assert!(lookup::<u32>(&key).is_none());
    }

    #[tokio::test]
    async fn differing_identity_yields_distinct_clients() {
        let a = get_or_create("test-id", connection_identity("server-a"), true, || {
            build_client(1)
        })
        .await
        .unwrap();
        let b = get_or_create("test-id", connection_identity("server-b"), true, || {
            build_client(2)
        })
        .await
        .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }
}
