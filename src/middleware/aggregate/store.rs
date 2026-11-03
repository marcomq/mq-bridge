//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Where an `aggregate` middleware keeps its states when a `store` is configured.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// Longest stored key; the SQL key column is this wide.
pub(crate) const MAX_KEY_LEN: usize = 512;

/// One state to write, guarded by the version it was computed from.
#[cfg_attr(
    not(any(feature = "sqlite", feature = "sqlx", feature = "mongodb")),
    allow(dead_code)
)]
pub(crate) struct StateWrite {
    pub(crate) key: String,
    /// JSON text of the new state.
    pub(crate) state: String,
    /// Version the state was loaded with; 0 for a key that did not exist.
    pub(crate) expected: i64,
}

/// A keyed state store with versioned compare-and-set writes.
#[async_trait]
pub(crate) trait StateStore: Send + Sync {
    /// JSON text and version of each key that exists.
    async fn load_many(&self, keys: &[String]) -> anyhow::Result<HashMap<String, (String, i64)>>;

    /// Writes each state whose stored version still equals `expected`, as version
    /// `expected + 1`. Returns the indices that lost the race; those were not written.
    async fn store_many(&self, writes: &[StateWrite]) -> anyhow::Result<Vec<usize>>;
}

/// Opens the store a `store:` URL names; the table or collection defaults to
/// `mqb_aggregate_<route>`.
pub(crate) async fn build_store(
    spec: &str,
    route_name: &str,
) -> anyhow::Result<Arc<dyn StateStore>> {
    let name = format!(
        "mqb_aggregate_{}",
        crate::checkpoint::sanitize_ident(route_name)
    );
    match crate::checkpoint::parse_checkpoint_store(spec)? {
        #[cfg(any(feature = "sqlite", feature = "sqlx"))]
        crate::checkpoint::CheckpointBackend::Sqlx { url, table } => {
            crate::endpoints::sqlx::build_sql_state_store(&url, table.unwrap_or(name)).await
        }
        #[cfg(feature = "mongodb")]
        crate::checkpoint::CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => {
            crate::endpoints::mongodb::build_mongo_state_store(
                &url,
                &database,
                &collection.unwrap_or(name),
            )
            .await
        }
        _ => {
            let _ = name;
            anyhow::bail!(
                "aggregate: store '{spec}' is not supported; use a postgres, sqlite or mongodb \
                 URL, in a build with the `sqlx` or `mongodb` feature"
            )
        }
    }
}

/// An in-process store, for tests of the shared path.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryStateStore {
    pub(crate) rows: std::sync::Mutex<HashMap<String, (String, i64)>>,
    /// Fails this many `store_many` calls before working.
    pub(crate) failures: std::sync::atomic::AtomicUsize,
    /// While set, `store_many` does not return.
    pub(crate) paused: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[async_trait]
impl StateStore for MemoryStateStore {
    async fn load_many(&self, keys: &[String]) -> anyhow::Result<HashMap<String, (String, i64)>> {
        tokio::task::yield_now().await;
        let rows = self.rows.lock().unwrap();
        Ok(keys
            .iter()
            .filter_map(|k| Some((k.clone(), rows.get(k)?.clone())))
            .collect())
    }

    async fn store_many(&self, writes: &[StateWrite]) -> anyhow::Result<Vec<usize>> {
        use std::sync::atomic::Ordering;
        tokio::task::yield_now().await;
        while self.paused.load(Ordering::Relaxed) {
            tokio::task::yield_now().await;
        }
        // `try_update`, its replacement, is newer than the MSRV.
        #[allow(deprecated)]
        let fail = self
            .failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if fail {
            anyhow::bail!("store unavailable");
        }
        let mut rows = self.rows.lock().unwrap();
        let mut lost = Vec::new();
        for (i, write) in writes.iter().enumerate() {
            let current = rows.get(&write.key).map_or(0, |(_, version)| *version);
            if current == write.expected {
                rows.insert(write.key.clone(), (write.state.clone(), current + 1));
            } else {
                lost.push(i);
            }
        }
        Ok(lost)
    }
}
