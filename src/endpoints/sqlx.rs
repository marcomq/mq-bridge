//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::SqlxConfig;
use crate::traits::{
    BoxFuture, CommitDisposition, ConsumerError, EndpointStatus, MessageConsumer,
    MessageDisposition, MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Row};
use std::borrow::Cow;
use std::time::Duration;
use tracing::{info, trace, warn};

fn is_valid_table_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return false;
    }
    name.split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Checks if a SQL query string contains a `(payload)` clause, ignoring case and whitespace.
fn contains_payload_clause(query: &str) -> bool {
    let lower_query = query.to_lowercase();
    let mut search_start = 0;
    while let Some(open_paren_idx) = lower_query[search_start..].find('(') {
        let absolute_open_idx = search_start + open_paren_idx;
        // Find the matching closing parenthesis
        if let Some(close_paren_idx) = lower_query[absolute_open_idx..].find(')') {
            let absolute_close_idx = absolute_open_idx + close_paren_idx;
            // Extract content between parentheses
            let content = &lower_query[absolute_open_idx + 1..absolute_close_idx];
            // Trim whitespace and check if it's "payload"
            if content.trim() == "payload" {
                return true;
            }
            // Continue searching after the found closing parenthesis
            search_start = absolute_close_idx + 1;
        } else {
            // No closing parenthesis found, stop searching
            break;
        }
    }
    false
}

fn build_sqlx_url_with_tls(config: &SqlxConfig) -> anyhow::Result<String> {
    let mut url = url::Url::parse(&config.url)?;

    if let Some(username) = &config.username {
        url.set_username(username)
            .map_err(|_| anyhow!("Cannot set username on sqlx URL"))?;
    }
    if let Some(password) = &config.password {
        url.set_password(Some(password))
            .map_err(|_| anyhow!("Cannot set password on sqlx URL"))?;
    }

    if config.tls.required {
        let scheme = url.scheme().to_string();
        match scheme.as_str() {
            "postgres" | "postgresql" => {
                let mut query_pairs = url.query_pairs_mut();
                if config.tls.accept_invalid_certs {
                    query_pairs.append_pair("sslmode", "require");
                } else if config.tls.ca_file.is_some() {
                    query_pairs.append_pair("sslmode", "verify-ca");
                } else {
                    query_pairs.append_pair("sslmode", "require");
                }

                if let Some(ca) = &config.tls.ca_file {
                    query_pairs.append_pair("sslrootcert", ca);
                }
                if let Some(cert) = &config.tls.cert_file {
                    query_pairs.append_pair("sslcert", cert);
                }
                if let Some(key) = &config.tls.key_file {
                    query_pairs.append_pair("sslkey", key);
                }
                if let Some(pass) = &config.tls.cert_password {
                    query_pairs.append_pair("sslpassword", pass);
                }
            }
            "mysql" | "mariadb" => {
                // MySQL/MariaDB support for TLS options in URL is more limited.
                // It's generally better to use a client-side configuration file (`my.cnf`)
                // for complex TLS setups. We'll add what we can.
                warn!("For complex MySQL/MariaDB TLS setups, using a client configuration file (my.cnf) is recommended over URL parameters.");
                let mut query_pairs = url.query_pairs_mut();
                query_pairs.append_pair("ssl-mode", "REQUIRED");
            }
            "mssql" | "sqlserver" => {
                let mut query_pairs = url.query_pairs_mut();
                if config.tls.accept_invalid_certs {
                    query_pairs.append_pair("encrypt", "true");
                    query_pairs.append_pair("trust-server-certificate", "true");
                } else {
                    query_pairs.append_pair("encrypt", "strict");
                }
            }
            _ => {}
        }
    }

    Ok(url.to_string())
}

async fn create_sqlx_pool(config: &SqlxConfig) -> anyhow::Result<AnyPool> {
    let url = build_sqlx_url_with_tls(config)?;
    let mut pool_options = AnyPoolOptions::new();

    if let Some(max_conn) = config.max_connections {
        pool_options = pool_options.max_connections(max_conn);
    }
    if let Some(min_conn) = config.min_connections {
        pool_options = pool_options.min_connections(min_conn);
    }
    if let Some(timeout) = config.acquire_timeout_ms {
        pool_options = pool_options.acquire_timeout(Duration::from_millis(timeout));
    }
    if let Some(timeout) = config.idle_timeout_ms {
        pool_options = pool_options.idle_timeout(Duration::from_millis(timeout));
    }
    if let Some(lifetime) = config.max_lifetime_ms {
        pool_options = pool_options.max_lifetime(Duration::from_millis(lifetime));
    }

    Ok(pool_options.connect(&url).await?)
}

pub struct SqlxPublisher {
    pool: AnyPool,
    insert_query: String,
    driver_name: String,
    table: String,
}

impl SqlxPublisher {
    pub async fn new(config: &SqlxConfig) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        if !is_valid_table_name(&config.table) {
            return Err(anyhow!(
                "Invalid table name: '{}'. Only alphanumeric characters and underscores are allowed.",
                config.table
            ));
        }
        let pool = create_sqlx_pool(config).await?;
        let table = config.table.clone();

        // Acquire a connection to determine the driver so we can use the correct SQL syntax.
        let conn = pool.acquire().await?;
        let driver_name = conn.backend_name().to_string();
        drop(conn);

        info!(table = %config.table, driver = %driver_name, "SQLx publisher connected");

        if config.auto_create_table {
            // --- Auto-create table and index ---
            let create_table_query = match driver_name.as_str() {
                "PostgreSQL" => format!(
                    "CREATE TABLE IF NOT EXISTS {} (id BIGSERIAL PRIMARY KEY, payload BYTEA NOT NULL, locked_until TIMESTAMPTZ, created_at TIMESTAMPTZ DEFAULT NOW())",
                    config.table
                ),
                "MySQL" | "MariaDB" => format!(
                    "CREATE TABLE IF NOT EXISTS {} (id BIGINT AUTO_INCREMENT PRIMARY KEY, payload BLOB NOT NULL, locked_until DATETIME, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
                    config.table
                ),
                "SQLite" => format!(
                    "CREATE TABLE IF NOT EXISTS {} (id INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB NOT NULL, locked_until DATETIME, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
                    config.table
                ),
                "Microsoft SQL Server" => format!(
                    "IF NOT EXISTS (SELECT * FROM sys.objects WHERE object_id = OBJECT_ID(N'{0}') AND type in (N'U'))
                CREATE TABLE {0} (id BIGINT IDENTITY(1,1) PRIMARY KEY, payload VARBINARY(MAX) NOT NULL, locked_until DATETIME2, created_at DATETIME2 DEFAULT GETUTCDATE())",
                    config.table
                ),
                _ => "".to_string(), // Don't attempt for unknown drivers
            };

            if !create_table_query.is_empty() {
                if let Err(e) = sqlx::query(&create_table_query).execute(&pool).await {
                    warn!(
                        "Failed to auto-create table '{}': {}. Please ensure it exists.",
                        config.table, e
                    );
                } else {
                    let table_name_for_index =
                        config.table.split('.').next_back().unwrap_or(&config.table);
                    let index_name = format!("idx_{}_locked_until", table_name_for_index);

                    let create_index_query = match driver_name.as_str() {
                        "PostgreSQL" | "SQLite" => {
                            format!(
                                "CREATE INDEX IF NOT EXISTS {} ON {} (locked_until)",
                                index_name, config.table
                            )
                        }
                        "MySQL" | "MariaDB" => {
                            format!(
                                "CREATE INDEX {} ON {} (locked_until)",
                                index_name, config.table
                            )
                        }
                        "Microsoft SQL Server" => {
                            format!(
                                "IF NOT EXISTS (SELECT * FROM sys.indexes WHERE name = N'{}' AND object_id = OBJECT_ID(N'{}'))
                                CREATE INDEX {} ON {} (locked_until)",
                                index_name, config.table, index_name, config.table
                            )
                        }
                        _ => "".to_string(),
                    };

                    if !create_index_query.is_empty() {
                        if let Err(e) = sqlx::query(&create_index_query).execute(&pool).await {
                            if (driver_name.as_str() == "MySQL"
                                || driver_name.as_str() == "MariaDB")
                                && e.as_database_error()
                                    .is_some_and(|db_err| db_err.code() == Some(Cow::from("1061")))
                            {
                                trace!("Index {} on {} already exists.", index_name, config.table);
                            } else {
                                warn!("Failed to create index on '{}': {}", config.table, e);
                            }
                        }
                    }
                }
            }
        }

        let insert_query =
            config
                .insert_query
                .clone()
                .unwrap_or_else(|| match driver_name.as_str() {
                    "PostgreSQL" => format!("INSERT INTO {} (payload) VALUES ($1)", config.table),
                    "Microsoft SQL Server" => {
                        format!("INSERT INTO {} (payload) VALUES (@p1)", config.table)
                    }
                    _ => format!("INSERT INTO {} (payload) VALUES (?)", config.table),
                });

        Ok(Self {
            pool,
            insert_query,
            driver_name,
            table,
        })
    }
}

#[async_trait]
impl MessagePublisher for SqlxPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(message_id = %format!("{:032x}", message.message_id), table = %self.table, "Publishing to SQL");
        sqlx::query(&self.insert_query)
            .bind(message.payload.to_vec())
            .execute(&self.pool)
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch to SQLx");

        // Manually construct the query with appropriate placeholders because
        // sqlx::QueryBuilder with the `Any` driver does not correctly rewrite `?` to `$N`.
        let base_query = match self.insert_query.to_uppercase().rfind("VALUES") {
            Some(pos) => &self.insert_query[..pos],
            None => {
                warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
                return self.send_batch_iterative(messages).await;
            }
        };

        if !contains_payload_clause(base_query) {
            warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages).await;
        }

        let mut placeholders = String::new();
        for i in 0..messages.len() {
            if i > 0 {
                placeholders.push_str(", ");
            }
            placeholders.push('(');
            match self.driver_name.as_str() {
                "PostgreSQL" => placeholders.push_str(&format!("${}", i + 1)),
                "Microsoft SQL Server" => placeholders.push_str(&format!("@p{}", i + 1)),
                _ => placeholders.push('?'),
            }
            placeholders.push(')');
        }

        let sql = format!("{} VALUES {}", base_query, placeholders);

        let mut query = sqlx::query(&sql);
        for msg in messages {
            query = query.bind(msg.payload.to_vec());
        }

        query
            .execute(&self.pool)
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        Ok(SentBatch::Ack)
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.pool.acquire().await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };

        EndpointStatus {
            healthy,
            target: self.table.clone(),
            error,
            details: serde_json::json!({ "driver": self.driver_name, "pool_size": self.pool.size(), "pool_idle": self.pool.num_idle() }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SqlxPublisher {
    /// Fallback implementation that inserts messages one by one within a transaction.
    /// This is less performant than a single multi-row insert statement.
    async fn send_batch_iterative(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        for msg in messages {
            sqlx::query(&self.insert_query)
                .bind(msg.payload.to_vec())
                .execute(&mut *tx)
                .await
                .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        }
        tx.commit()
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        Ok(SentBatch::Ack)
    }
}

pub struct SqlxConsumer {
    pool: AnyPool,
    select_query: String,
    delete_after_read: bool,
    table: String,
    polling_interval: Duration,
    driver_name: String,
}

impl SqlxConsumer {
    pub async fn new(config: &SqlxConfig) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        if !is_valid_table_name(&config.table) {
            return Err(anyhow!(
                "Invalid table name: '{}'. Only alphanumeric characters and underscores are allowed.",
                config.table
            ));
        }
        let pool = create_sqlx_pool(config).await?;

        // Acquire a connection to determine the driver so we can use the correct SQL syntax later.
        let conn = pool.acquire().await?;
        let driver_name = conn.backend_name().to_string();
        // Immediately return the connection to the pool.
        drop(conn);
        info!(table = %config.table, driver = %driver_name, "SQLx consumer connected");

        let select_query = if let Some(query) = &config.select_query {
            match driver_name.as_str() {
                "PostgreSQL" => {
                    if !query.contains("$1") {
                        return Err(anyhow!("Custom select_query for PostgreSQL must contain a '$1' placeholder for the batch size limit."));
                    }
                    query.clone()
                }
                "Microsoft SQL Server" => {
                    if !query.contains("@p1") {
                        return Err(anyhow!("Custom select_query for SQL Server must contain a '@p1' placeholder for the batch size limit."));
                    }
                    query.clone()
                }
                _ => {
                    return Err(anyhow!("Custom select_query is not supported for the '{}' driver. It is only supported for PostgreSQL and Microsoft SQL Server.", driver_name));
                }
            }
        } else {
            match driver_name.as_str() {
                "PostgreSQL" => {
                    // This CTE-based query atomically finds available rows, locks them,
                    // updates their `locked_until` timestamp, and returns them.
                    // This is a robust pattern for a work queue with multiple consumers.
                    format!(
                        r#"
WITH available AS (
    SELECT id FROM {0}
    WHERE locked_until IS NULL OR locked_until < NOW()
    ORDER BY id
    LIMIT $1
    FOR UPDATE SKIP LOCKED
),
updated AS (
    UPDATE {0}
    SET locked_until = NOW() + interval '60 seconds'
    WHERE id IN (SELECT id FROM available)
    RETURNING id, payload
)
SELECT id, payload FROM updated"#,
                        config.table,
                    )
                }
                "Microsoft SQL Server" => {
                    // This query atomically finds available rows, locks them,
                    // updates their `locked_until` timestamp, and returns them.
                    format!(
                        r#"
UPDATE {0}
SET locked_until = DATEADD(second, 60, GETUTCDATE())
OUTPUT INSERTED.id, INSERTED.payload
WHERE id IN (SELECT TOP (@p1) id FROM {0} WITH (UPDLOCK, READPAST) WHERE locked_until IS NULL OR locked_until < GETUTCDATE() ORDER BY id)"#,
                        config.table
                    )
                }
                _ => format!("SELECT id, payload FROM {}", config.table),
            }
        };
        Ok(Self {
            pool,
            select_query,
            delete_after_read: config.delete_after_read,
            table: config.table.clone(),
            polling_interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
            driver_name,
        })
    }
}

impl SqlxConsumer {
    async fn fetch_and_lock_mysql(
        &self,
        limit: usize,
    ) -> Result<Vec<sqlx::any::AnyRow>, ConsumerError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        // 1. Find and lock rows
        let lock_query = format!(
            "SELECT id FROM {} WHERE locked_until IS NULL OR locked_until < NOW() ORDER BY id LIMIT ? FOR UPDATE SKIP LOCKED",
            self.table
        );

        let locked_ids: Vec<i64> = sqlx::query(&lock_query)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?
            .into_iter()
            .map(|row| row.get("id"))
            .collect();

        if locked_ids.is_empty() {
            tx.commit().await.ok(); // Nothing to do, commit and return
            return Ok(vec![]);
        }

        // 2. Update the `locked_until` for the locked rows
        let placeholders = locked_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let update_query = format!(
            "UPDATE {} SET locked_until = NOW() + INTERVAL 60 SECOND WHERE id IN ({})",
            self.table, placeholders
        );

        let mut query = sqlx::query(&update_query);
        for id in &locked_ids {
            query = query.bind(*id);
        }

        query
            .execute(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        // 3. Select the full rows that we just locked
        let select_query = format!(
            "SELECT id, payload FROM {} WHERE id IN ({})",
            self.table, placeholders
        );

        let mut query = sqlx::query(&select_query);
        for id in &locked_ids {
            query = query.bind(*id);
        }

        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        // 4. Commit the transaction
        tx.commit()
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        Ok(rows)
    }

    async fn fetch_and_lock_sqlite(
        &self,
        limit: usize,
    ) -> Result<Vec<sqlx::any::AnyRow>, ConsumerError> {
        // Use `BEGIN IMMEDIATE` to acquire a RESERVED lock on the database file,
        // preventing other connections from reading until this transaction is complete.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        let select_query = format!(
            "SELECT id FROM {} WHERE locked_until IS NULL OR locked_until < datetime('now') ORDER BY id LIMIT ?",
            self.table
        );

        let locked_ids: Vec<i64> = sqlx::query(&select_query)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?
            .into_iter()
            .map(|row| row.get("id"))
            .collect();

        if locked_ids.is_empty() {
            tx.commit().await.ok();
            return Ok(vec![]);
        }

        let placeholders = locked_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let update_query = format!(
            "UPDATE {} SET locked_until = datetime('now', '+60 seconds') WHERE id IN ({})",
            self.table, placeholders
        );

        let mut query = sqlx::query(&update_query);
        for id in &locked_ids {
            query = query.bind(*id);
        }
        query
            .execute(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        let select_payload_query = format!(
            "SELECT id, payload FROM {} WHERE id IN ({})",
            self.table, placeholders
        );
        let mut query = sqlx::query(&select_payload_query);
        for id in &locked_ids {
            query = query.bind(*id);
        }
        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        tx.commit()
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        Ok(rows)
    }
    async fn get_pending_count(&self) -> anyhow::Result<usize> {
        let query = match self.driver_name.as_str() {
            "PostgreSQL" | "MySQL" | "MariaDB" => format!(
                "SELECT COUNT(*) FROM {} WHERE locked_until IS NULL OR locked_until < NOW()",
                self.table
            ),
            "SQLite" => format!(
                "SELECT COUNT(*) FROM {} WHERE locked_until IS NULL OR locked_until < datetime('now')",
                self.table
            ),
            "Microsoft SQL Server" => format!(
                "SELECT COUNT(*) FROM {} WHERE locked_until IS NULL OR locked_until < GETUTCDATE()",
                self.table
            ),
            _ => anyhow::bail!("Unsupported driver for pending count: {}", self.driver_name),
        };

        let row: sqlx::any::AnyRow = sqlx::query(&query).fetch_one(&self.pool).await?;
        if let Ok(c) = row.try_get::<i64, _>(0) {
            usize::try_from(c).map_err(|e| anyhow!("i64 to usize conversion failed: {}", e))
        } else {
            let c: i32 = row.try_get(0)?;
            usize::try_from(c).map_err(|e| anyhow!("i32 to usize conversion failed: {}", e))
        }
    }
}
#[async_trait]
impl MessageConsumer for SqlxConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        loop {
            let rows = match self.driver_name.as_str() {
                "PostgreSQL" | "Microsoft SQL Server" => sqlx::query(&self.select_query)
                    .bind(max_messages as i64)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| ConsumerError::Connection(anyhow!(e)))?,
                "MySQL" | "MariaDB" => self.fetch_and_lock_mysql(max_messages).await?,
                "SQLite" => self.fetch_and_lock_sqlite(max_messages).await?,
                _ => {
                    // Fallback for unknown drivers with a simple, non-locking read.
                    warn!("SQLx consumer for driver '{}' is using a non-locking read strategy. This is not safe for concurrent consumers.", self.driver_name);
                    let final_query = format!("{} LIMIT ?", self.select_query);
                    sqlx::query(&final_query)
                        .bind(max_messages as i64)
                        .fetch_all(&self.pool)
                        .await
                        .map_err(|e| ConsumerError::Connection(anyhow!(e)))?
                }
            };

            if !rows.is_empty() {
                let mut messages = Vec::new();
                let mut ids_to_delete = Vec::new();

                for row in rows.into_iter().take(max_messages) {
                    let payload: Vec<u8> = row
                        .try_get("payload")
                        .context("Failed to get 'payload' column")?;
                    let id: i64 = row.try_get("id").context("Failed to get 'id' column")?;
                    messages.push(CanonicalMessage::new(payload, None));
                    ids_to_delete.push(id);
                }
                trace!(count = messages.len(), "Received batch of SQLx messages");

                let pool = self.pool.clone();
                let table = self.table.clone();
                let delete = self.delete_after_read;
                let driver_name = self.driver_name.clone();

                let commit = Box::new(move |disposition: CommitDisposition| {
                    let pool = pool.clone();
                    let table = table.clone();
                    let ids = ids_to_delete.clone();
                    let driver_name = driver_name.clone();
                    Box::pin(async move {
                        if !delete {
                            return Ok(());
                        }
                        let ids_len = ids.len();
                        let dispositions = match disposition {
                            CommitDisposition::All(d) => vec![d; ids_len],
                            CommitDisposition::Individual(v) => v,
                        };
                        let mut ids_to_ack = Vec::new();
                        for (i, disp) in dispositions.iter().enumerate() {
                            let should_ack = match disp {
                                MessageDisposition::Ack => true,
                                MessageDisposition::Reply(_)
                                | MessageDisposition::ReplyBatch(_) => {
                                    tracing::warn!("SQLx consumer received a Reply or ReplyBatch, but replying is not supported by this endpoint. The reply payload is dropped, and the original message is acknowledged.");
                                    true
                                }
                                MessageDisposition::Nack => false,
                            };

                            if should_ack {
                                if let Some(id) = ids.get(i) {
                                    ids_to_ack.push(*id);
                                }
                            }
                        }

                        if !ids_to_ack.is_empty() {
                            // Manually construct the query with appropriate placeholders
                            // because sqlx::QueryBuilder with the `Any` driver does not
                            // correctly rewrite `?` to `$N` for PostgreSQL in this context.
                            let mut placeholders = String::new();
                            for i in 0..ids_to_ack.len() {
                                if i > 0 {
                                    placeholders.push_str(", ");
                                }
                                match driver_name.as_str() {
                                    "PostgreSQL" => placeholders.push_str(&format!("${}", i + 1)),
                                    "Microsoft SQL Server" => {
                                        placeholders.push_str(&format!("@p{}", i + 1))
                                    }
                                    _ => placeholders.push('?'),
                                }
                            }

                            let sql =
                                format!("DELETE FROM {} WHERE id IN ({})", table, placeholders);

                            let mut query = sqlx::query(&sql);
                            for id in ids_to_ack {
                                query = query.bind(id);
                            }

                            query
                                .execute(&pool)
                                .await
                                .map_err(|e| anyhow!("Failed to delete acked messages: {}", e))?;
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                return Ok(ReceivedBatch { messages, commit });
            }

            tokio::time::sleep(self.polling_interval).await;
        }
    }

    async fn status(&self) -> EndpointStatus {
        let (mut healthy, mut error) = match self.pool.acquire().await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };

        let mut pending = None;
        if healthy {
            match self.get_pending_count().await {
                Ok(c) => pending = Some(c),
                Err(e) => {
                    healthy = false;
                    error = Some(e.to_string());
                }
            }
        };

        EndpointStatus {
            healthy,
            target: self.table.clone(),
            pending,
            error,
            details: serde_json::json!({ "driver": self.driver_name, "pool_size": self.pool.size(), "pool_idle": self.pool.num_idle() }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{MessageConsumer, MessagePublisher};
    use tempfile::tempdir;

    async fn setup_db_file() -> (tempfile::TempDir, String) {
        use sqlx::Connection;
        sqlx::any::install_default_drivers();
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        #[cfg(windows)]
        let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
        #[cfg(not(windows))]
        let url = format!("sqlite://{}", path.to_str().unwrap());

        // Explicitly create the file first and drop the handle to avoid locking issues on Windows.
        // The `connect` call will create the file if it doesn't exist, but this can be racy in tests.
        drop(tokio::fs::File::create(&path).await.unwrap());

        let mut conn = sqlx::AnyConnection::connect(&url).await.unwrap();
        sqlx::query(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                payload BLOB NOT NULL,
                locked_until DATETIME,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        (dir, url)
    }

    #[tokio::test]
    async fn test_sqlx_roundtrip_delete() {
        let (_dir, url) = setup_db_file().await;

        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            delete_after_read: true,
            ..Default::default()
        };

        let publisher = SqlxPublisher::new(&config).await.unwrap();
        let msg_payload = b"hello sqlx".to_vec();
        let msg = CanonicalMessage::new(msg_payload.clone(), None);
        publisher.send(msg).await.unwrap();

        let mut consumer = SqlxConsumer::new(&config).await.unwrap();
        let received_batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(received_batch.messages.len(), 1);
        assert_eq!(received_batch.messages[0].payload.as_ref(), &msg_payload);

        (received_batch.commit)(CommitDisposition::All(MessageDisposition::Ack))
            .await
            .unwrap();

        let pool = AnyPool::connect(&url).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_sqlx_roundtrip_no_delete() {
        let (_dir, url) = setup_db_file().await;

        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            delete_after_read: false,
            ..Default::default()
        };

        let publisher = SqlxPublisher::new(&config).await.unwrap();
        let msg_payload = b"hello sqlx no delete".to_vec();
        let msg = CanonicalMessage::new(msg_payload.clone(), None);
        publisher.send(msg).await.unwrap();

        let mut consumer = SqlxConsumer::new(&config).await.unwrap();
        let received_batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(received_batch.messages.len(), 1);

        (received_batch.commit)(CommitDisposition::All(MessageDisposition::Ack))
            .await
            .unwrap();

        let pool = AnyPool::connect(&url).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_sqlx_status() {
        let (_dir, url) = setup_db_file().await;
        let config = SqlxConfig {
            url: url.clone(),
            table: "messages".to_string(),
            ..Default::default()
        };

        let publisher = SqlxPublisher::new(&config).await.unwrap();
        let status = publisher.status().await;
        assert!(status.healthy);
        assert_eq!(status.target, "messages");
        assert!(status.details.get("driver").is_some());
    }
}
