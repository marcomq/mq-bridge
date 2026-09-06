//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! SQL-backed deduplication store, selected by a `store:` URL on the `deduplication` middleware.

use super::*;

/// A shared, multi-instance deduplication store on a SQL database. One row per key
/// (`dedup_key` PK, `expire_at` = absolute unix-second expiry). SQL has no native TTL, so
/// `reserve` deletes the key first *if expired* and then relies on the primary-key unique
/// constraint as the atomic gate (a surviving live row makes the INSERT fail —
/// `is_unique_violation()` is driver-agnostic). `maybe_cleanup` sweeps expired rows
/// periodically. At-least-once, like the sled and Mongo stores.
struct SqlDedupStore {
    pool: AnyPool,
    driver_name: String,
    table: String,
    ttl_seconds: u64,
    last_cleanup: Arc<std::sync::atomic::AtomicU64>,
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
        Ok(())
    }
}

#[async_trait]
impl crate::middleware::deduplication::DedupStore for SqlDedupStore {
    async fn reserve(&self, key: &[u8], now: u64) -> Result<bool, ConsumerError> {
        let id = crate::middleware::deduplication::hex_key(key);
        let pending_expiry = (now + crate::middleware::deduplication::PENDING_TTL_SECS) as i64;

        // Reclaim this key if its previous reservation/processing has expired.
        let del = format!(
            "DELETE FROM {} WHERE dedup_key = {} AND expire_at <= {}",
            self.table,
            positional_placeholder(&self.driver_name, 1),
            positional_placeholder(&self.driver_name, 2)
        );
        sqlx::query(audited_sql(&del))
            .bind(id.clone())
            .bind(now as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| ConsumerError::Connection(anyhow!("dedup SQL delete failed: {e}")))?;

        // The primary key is the atomic gate: a surviving live row fails the INSERT.
        let ins = format!(
            "INSERT INTO {} (dedup_key, expire_at) VALUES ({}, {})",
            self.table,
            positional_placeholder(&self.driver_name, 1),
            positional_placeholder(&self.driver_name, 2)
        );
        match sqlx::query(audited_sql(&ins))
            .bind(id)
            .bind(pending_expiry)
            .execute(&self.pool)
            .await
        {
            Ok(_) => Ok(false),
            Err(e) => {
                if e.as_database_error()
                    .map(|d| d.is_unique_violation())
                    .unwrap_or(false)
                {
                    Ok(true)
                } else {
                    Err(ConsumerError::Connection(anyhow!(
                        "dedup SQL reserve failed: {e}"
                    )))
                }
            }
        }
    }

    async fn mark_processed(&self, key: &[u8], now: u64) {
        let id = crate::middleware::deduplication::hex_key(key);
        let expire_at = (now + self.ttl_seconds) as i64;
        let sql = format!(
            "UPDATE {} SET expire_at = {} WHERE dedup_key = {}",
            self.table,
            positional_placeholder(&self.driver_name, 1),
            positional_placeholder(&self.driver_name, 2)
        );
        match sqlx::query(audited_sql(&sql))
            .bind(expire_at)
            .bind(id.clone())
            .execute(&self.pool)
            .await
        {
            // The reserved row was reclaimed by cleanup in the meantime. Insert the marker
            // instead of dropping it, so the key stays deduplicated (matching the Mongo
            // store's upsert).
            Ok(result) if result.rows_affected() == 0 => {
                let ins = format!(
                    "INSERT INTO {} (dedup_key, expire_at) VALUES ({}, {})",
                    self.table,
                    positional_placeholder(&self.driver_name, 1),
                    positional_placeholder(&self.driver_name, 2)
                );
                if let Err(e) = sqlx::query(audited_sql(&ins))
                    .bind(id)
                    .bind(expire_at)
                    .execute(&self.pool)
                    .await
                {
                    warn!("Failed to insert dedup key marker in SQL: {}", e);
                }
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to mark dedup key processed in SQL: {}", e),
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
                let sql = format!(
                    "DELETE FROM {} WHERE expire_at < {}",
                    table,
                    positional_placeholder(&driver, 1)
                );
                if let Err(e) = sqlx::query(audited_sql(&sql))
                    .bind(now as i64)
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
    };
    store.ensure_table().await?;
    Ok(Arc::new(store))
}
