//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! SQL-backed state store, selected by a `store:` URL on the `aggregate` middleware.

use super::*;
use crate::middleware::aggregate::store::{StateStore, StateWrite, MAX_KEY_LEN};
use std::collections::HashMap;

/// Rows per statement; three bind values each stay far below every driver's limit.
const CHUNK: usize = 1000;

/// One row per key: `state` is JSON text, `version` counts its writes. A batch is written in
/// one transaction of upserts that only take effect on the expected version, so it lands
/// completely or not at all.
struct SqlStateStore {
    pool: AnyPool,
    driver_name: String,
    table: String,
}

#[async_trait]
impl StateStore for SqlStateStore {
    async fn load_many(&self, keys: &[String]) -> anyhow::Result<HashMap<String, (String, i64)>> {
        let mut found = HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(CHUNK) {
            let marks: Vec<String> = (1..=chunk.len())
                .map(|n| positional_placeholder(&self.driver_name, n))
                .collect();
            let sql = format!(
                "SELECT agg_key, state, version FROM {} WHERE agg_key IN ({})",
                self.table,
                marks.join(", ")
            );
            // Not cached: the statement text changes with the number of keys.
            let mut query = sqlx::query(audited_sql(&sql)).persistent(false);
            for key in chunk {
                query = query.bind(key.as_str());
            }
            for row in query.fetch_all(&self.pool).await? {
                found.insert(row.try_get(0)?, (row.try_get(1)?, row.try_get(2)?));
            }
        }
        Ok(found)
    }

    async fn store_many(&self, writes: &[StateWrite]) -> anyhow::Result<Vec<usize>> {
        let mut tx = match self.driver_name.as_str() {
            // Takes the write lock up front instead of failing on the upgrade.
            "SQLite" => self.pool.begin_with("BEGIN IMMEDIATE").await?,
            _ => self.pool.begin().await?,
        };
        // Sorted, so two instances lock rows in the same order.
        let mut order: Vec<&StateWrite> = writes.iter().collect();
        order.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        for chunk in order.chunks(CHUNK) {
            let rows: Vec<String> = (0..chunk.len())
                .map(|row| {
                    let mark = |n| positional_placeholder(&self.driver_name, row * 3 + n);
                    format!("({}, {}, {})", mark(1), mark(2), mark(3))
                })
                .collect();
            let sql = format!(
                "INSERT INTO {} AS cur (agg_key, state, version) VALUES {} \
                 ON CONFLICT (agg_key) DO UPDATE SET state = excluded.state, \
                 version = excluded.version WHERE cur.version = excluded.version - 1",
                self.table,
                rows.join(", ")
            );
            let mut query = sqlx::query(audited_sql(&sql)).persistent(false);
            for write in chunk {
                query = query
                    .bind(write.key.as_str())
                    .bind(write.state.as_str())
                    .bind(write.expected + 1);
            }
            if query.execute(&mut *tx).await?.rows_affected() != chunk.len() as u64 {
                // A version moved: nothing of the batch is kept, all of it is folded again.
                tx.rollback().await?;
                return Ok((0..writes.len()).collect());
            }
        }
        tx.commit().await?;
        Ok(Vec::new())
    }
}

/// Builds a state store on a SQL database (its own pool). All instances of a route must
/// share the same URL and table.
pub(crate) async fn build_sql_state_store(
    url: &str,
    table: String,
) -> anyhow::Result<Arc<dyn StateStore>> {
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(url).await.with_context(|| {
        format!(
            "Failed to connect aggregate store at '{}'",
            crate::support::redact::url_password(url)
        )
    })?;
    let driver_name = pool.acquire().await?.backend_name().to_string();
    if !matches!(driver_name.as_str(), "PostgreSQL" | "SQLite") {
        return Err(anyhow!(
            "aggregate: a {driver_name} store is not supported; use postgres, sqlite or mongodb"
        ));
    }
    if !is_valid_table_name(&table) {
        return Err(anyhow!("Invalid aggregate table name: '{table}'."));
    }
    let create = format!(
        "CREATE TABLE IF NOT EXISTS {table} (agg_key VARCHAR({MAX_KEY_LEN}) PRIMARY KEY, \
         state TEXT NOT NULL, version BIGINT NOT NULL)"
    );
    sqlx::query(audited_sql(&create))
        .execute(&pool)
        .await
        .with_context(|| format!("Failed to create aggregate table '{table}'"))?;
    Ok(Arc::new(SqlStateStore {
        pool,
        driver_name,
        table,
    }))
}
