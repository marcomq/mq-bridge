//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! SQL-backed deduplication store, selected by a `store:` URL on the `deduplication` middleware.

use super::*;

/// A shared, multi-instance deduplication store on a SQL database. One row per key
/// (`dedup_key` PK = hex key). `expire_at` is the absolute unix-second expiry, and its sign is
/// the state: negative for an in-flight claim, positive once processed — so claiming is one
/// INSERT and committing one UPDATE of the same row, with no state column to migrate. The
/// primary key is the atomic gate (`is_unique_violation()` is driver-agnostic). SQL has no
/// native TTL, so expiry is judged on read and `maybe_cleanup` sweeps expired rows
/// periodically. At-least-once, like the sled and Mongo stores.
struct SqlDedupStore {
    pool: AnyPool,
    driver_name: String,
    table: String,
    ttl_seconds: u64,
    last_cleanup: Arc<std::sync::atomic::AtomicU64>,
    /// Keep replies in a `response` column, added on startup when enabled.
    replay_response: bool,
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.is_unique_violation())
        .unwrap_or(false)
}

fn store_failed(e: sqlx::Error) -> ConsumerError {
    ConsumerError::Connection(anyhow!("dedup SQL reserve failed: {e}"))
}

impl SqlDedupStore {
    async fn ensure_table(&self) -> anyhow::Result<()> {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (dedup_key VARCHAR(255) PRIMARY KEY, expire_at BIGINT NOT NULL)",
            self.table
        );
        sqlx::query(audited_sql(&sql))
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to create dedup table '{}'", self.table))?;
        // Speeds up the cleanup sweep. Best-effort: some drivers reject `IF NOT EXISTS` here.
        let idx = format!(
            "CREATE INDEX IF NOT EXISTS {0}_expire_idx ON {0} (expire_at)",
            self.table
        );
        let _ = sqlx::query(audited_sql(&idx)).execute(&self.pool).await;
        if self.replay_response {
            self.ensure_response_column().await?;
        }
        Ok(())
    }

    /// Adds the `response` column to a table created without it. Probed with a query rather
    /// than `ADD COLUMN IF NOT EXISTS`, which MySQL and SQLite do not support.
    async fn ensure_response_column(&self) -> anyhow::Result<()> {
        let probe = format!("SELECT response FROM {} WHERE 1 = 0", self.table);
        if sqlx::query(audited_sql(&probe))
            .fetch_optional(&self.pool)
            .await
            .is_ok()
        {
            return Ok(());
        }
        let column_type = match self.driver_name.as_str() {
            "PostgreSQL" => "BYTEA",
            "MySQL" => "LONGBLOB",
            _ => "BLOB",
        };
        let alter = format!(
            "ALTER TABLE {} ADD COLUMN response {column_type}",
            self.table
        );
        sqlx::query(audited_sql(&alter))
            .execute(&self.pool)
            .await
            .with_context(|| {
                format!(
                    "Failed to add the response column to dedup table '{}'",
                    self.table
                )
            })?;
        Ok(())
    }

    fn placeholder(&self, n: usize) -> String {
        positional_placeholder(&self.driver_name, n)
    }

    async fn insert_row(&self, id: &str, expire_at: i64) -> Result<(), sqlx::Error> {
        let sql = format!(
            "INSERT INTO {} (dedup_key, expire_at) VALUES ({}, {})",
            self.table,
            self.placeholder(1),
            self.placeholder(2)
        );
        sqlx::query(audited_sql(&sql))
            .bind(id.to_string())
            .bind(expire_at)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    async fn stored_expiry(&self, id: &str) -> Result<Option<i64>, ConsumerError> {
        let sql = format!(
            "SELECT expire_at FROM {} WHERE dedup_key = {}",
            self.table,
            self.placeholder(1)
        );
        let row = sqlx::query(audited_sql(&sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(store_failed)?;
        row.map(|row| row.try_get::<i64, _>(0))
            .transpose()
            .map_err(store_failed)
    }

    /// Replaces `expected` with `expire_at` (and clears any stored reply), unless another
    /// instance changed the row first. Returns whether this call won.
    async fn swap_expiry(
        &self,
        id: &str,
        expected: i64,
        expire_at: i64,
    ) -> Result<bool, ConsumerError> {
        let clear_response = if self.replay_response {
            ", response = NULL"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE {} SET expire_at = {}{clear_response} WHERE dedup_key = {} AND expire_at = {}",
            self.table,
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3)
        );
        sqlx::query(audited_sql(&sql))
            .bind(expire_at)
            .bind(id.to_string())
            .bind(expected)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() == 1)
            .map_err(store_failed)
    }

    /// Turns the key's claim into a committed marker. A missing row (swept after its claim
    /// lapsed) is inserted instead. Without `replay_response` there is no `response` column.
    async fn write_marker(&self, id: &str, expire_at: i64, response: Option<&[u8]>) {
        let set_response = if self.replay_response {
            format!(", response = {}", self.placeholder(2))
        } else {
            String::new()
        };
        let key_slot = if self.replay_response { 3 } else { 2 };
        let sql = format!(
            "UPDATE {} SET expire_at = {}{set_response} WHERE dedup_key = {}",
            self.table,
            self.placeholder(1),
            self.placeholder(key_slot)
        );
        let mut update = sqlx::query(audited_sql(&sql)).bind(expire_at);
        if self.replay_response {
            update = update.bind(response.map(<[u8]>::to_vec));
        }
        match update.bind(id.to_string()).execute(&self.pool).await {
            Ok(result) if result.rows_affected() == 0 => {
                if let Err(e) = self.insert_marker(id, expire_at, response).await {
                    warn!("Failed to insert dedup key marker in SQL: {}", e);
                }
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to mark dedup key processed in SQL: {}", e),
        }
    }

    async fn insert_marker(
        &self,
        id: &str,
        expire_at: i64,
        response: Option<&[u8]>,
    ) -> Result<(), sqlx::Error> {
        if !self.replay_response {
            return self.insert_row(id, expire_at).await;
        }
        let sql = format!(
            "INSERT INTO {} (dedup_key, expire_at, response) VALUES ({}, {}, {})",
            self.table,
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3)
        );
        sqlx::query(audited_sql(&sql))
            .bind(id.to_string())
            .bind(expire_at)
            .bind(response.map(<[u8]>::to_vec))
            .execute(&self.pool)
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl crate::middleware::deduplication::DedupStore for SqlDedupStore {
    async fn reserve(
        &self,
        key: &[u8],
        now: u64,
    ) -> Result<crate::middleware::deduplication::Reservation, ConsumerError> {
        use crate::middleware::deduplication::{hex_key, Reservation, PENDING_TTL_SECS};
        let id = hex_key(key);
        let claim = -((now + PENDING_TTL_SECS) as i64);
        let now = now as i64;

        // Bounded: each pass loses only to another instance that changed the row meanwhile.
        for _ in 0..3 {
            match self.insert_row(&id, claim).await {
                Ok(()) => return Ok(Reservation::Claimed),
                Err(e) if is_unique_violation(&e) => {}
                Err(e) => return Err(store_failed(e)),
            }
            let Some(stored) = self.stored_expiry(&id).await? else {
                continue;
            };
            let (expiry, state) = if stored < 0 {
                (-stored, Reservation::InFlight)
            } else {
                (stored, Reservation::Processed)
            };
            if expiry > now {
                return Ok(state);
            }
            // Expired: a crashed holder's claim or an old marker. Take it over.
            if self.swap_expiry(&id, stored, claim).await? {
                return Ok(Reservation::Claimed);
            }
        }
        Ok(Reservation::InFlight)
    }

    async fn mark_processed(&self, key: &[u8], now: u64) {
        let id = crate::middleware::deduplication::hex_key(key);
        self.write_marker(&id, (now + self.ttl_seconds) as i64, None)
            .await;
    }

    async fn mark_processed_with_response(&self, key: &[u8], now: u64, response: &[u8]) {
        let id = crate::middleware::deduplication::hex_key(key);
        self.write_marker(&id, (now + self.ttl_seconds) as i64, Some(response))
            .await;
    }

    async fn stored_response(&self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.replay_response {
            return None;
        }
        let id = crate::middleware::deduplication::hex_key(key);
        let sql = format!(
            "SELECT response FROM {} WHERE dedup_key = {}",
            self.table,
            self.placeholder(1)
        );
        let row = sqlx::query(audited_sql(&sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| warn!("Failed to read dedup reply from SQL: {}", e))
            .ok()??;
        row.try_get::<Option<Vec<u8>>, _>(0).ok().flatten()
    }

    async fn release(&self, key: &[u8]) {
        let id = crate::middleware::deduplication::hex_key(key);
        let sql = format!(
            "DELETE FROM {} WHERE dedup_key = {} AND expire_at < 0",
            self.table,
            self.placeholder(1)
        );
        if let Err(e) = sqlx::query(audited_sql(&sql))
            .bind(id)
            .execute(&self.pool)
            .await
        {
            warn!("Failed to release dedup key in SQL: {}", e);
        }
    }

    fn maybe_cleanup(&self, now: u64) {
        use std::sync::atomic::Ordering;
        let last = self.last_cleanup.load(Ordering::Acquire);
        if now.saturating_sub(last) > 30
            && self
                .last_cleanup
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::Acquire)
                .is_ok()
        {
            let pool = self.pool.clone();
            let driver = self.driver_name.clone();
            let table = self.table.clone();
            tokio::spawn(async move {
                // Expired markers, and expired claims (negative, so "past" is above -now).
                let sql = format!(
                    "DELETE FROM {table} WHERE (expire_at >= 0 AND expire_at < {0}) OR (expire_at < 0 AND expire_at > {1})",
                    positional_placeholder(&driver, 1),
                    positional_placeholder(&driver, 2)
                );
                if let Err(e) = sqlx::query(audited_sql(&sql))
                    .bind(now as i64)
                    .bind(-(now as i64))
                    .execute(&pool)
                    .await
                {
                    warn!("dedup SQL cleanup failed: {}", e);
                }
            });
        }
    }
}

/// Build a deduplication store on a SQL database (its own pool), selected by a
/// `postgres|mysql|mariadb|sqlite://…[/table]` `store:` URL. All instances of a route must
/// share the same URL and table; the table defaults to `mqb_dedup_<route>`.
pub(crate) async fn build_sql_dedup_store(
    url: &str,
    table: Option<String>,
    ttl_seconds: u64,
    route_name: &str,
    replay_response: bool,
) -> anyhow::Result<Arc<dyn crate::middleware::deduplication::DedupStore>> {
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(url)
        .await
        .with_context(|| format!("Failed to connect deduplication store at '{}'", url))?;
    let driver_name = {
        let conn = pool.acquire().await?;
        let name = conn.backend_name().to_string();
        drop(conn);
        name
    };
    let table = table.unwrap_or_else(|| {
        format!(
            "mqb_dedup_{}",
            crate::checkpoint::sanitize_ident(route_name)
        )
    });
    if !is_valid_table_name(&table) {
        return Err(anyhow!("Invalid deduplication table name: '{}'.", table));
    }
    let store = SqlDedupStore {
        pool,
        driver_name,
        table,
        ttl_seconds,
        last_cleanup: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        replay_response,
    };
    store.ensure_table().await?;
    Ok(Arc::new(store))
}
