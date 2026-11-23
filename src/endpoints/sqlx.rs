//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::poll::PollBackoff;
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{CaRoots, SqlxConfig};
use crate::support::cursor_batch::{CursorPosition, Unsettled};
use crate::support::interpolation::pick_fields;
use crate::support::lookup_batch::{self, ListQuery};
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use sqlx::any::AnyPoolOptions;
#[cfg(feature = "sqlx")]
use sqlx::postgres::{PgPool, PgPoolCopyExt, PgPoolOptions};
use sqlx::{AnyPool, AssertSqlSafe, Column, Row};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, trace, warn};

static HOST_INSTALLS_DRIVERS: AtomicBool = AtomicBool::new(false);

/// Tells mq-bridge that the application installs the sqlx `Any` drivers itself.
///
/// By default, SQL endpoints call `sqlx::any::install_default_drivers()`, which panics
/// if the application already called `sqlx::any::install_drivers()`. Call this before
/// creating an SQL endpoint when you do; your driver list must cover the endpoint URLs.
pub fn use_host_sqlx_drivers() {
    HOST_INSTALLS_DRIVERS.store(true, Ordering::Release);
}

fn install_sqlx_drivers() {
    if !HOST_INSTALLS_DRIVERS.load(Ordering::Acquire) {
        sqlx::any::install_default_drivers();
    }
}

mod columns;
mod cursor;
#[cfg(feature = "dedup")]
mod dedup;
mod load;
mod lookup;
use lookup::*;
mod template;
pub use cursor::SqlxCursorReader;
pub(crate) use cursor::{build_sql_checkpoint_store, cursor_checkpoint};
use cursor::{
    ensure_transactional_engine, is_permanent_decode_error, quote_ident_for, JsonRowSchema,
    MYSQL_COLUMN_TYPES_SQL, SQLITE_COLUMN_TYPES_SQL,
};
#[cfg(feature = "dedup")]
pub(crate) use dedup::build_sql_dedup_store;
use template::*;
#[cfg(feature = "aggregate")]
mod aggregate;
#[cfg(feature = "aggregate")]
pub(crate) use aggregate::build_sql_state_store;

fn is_deadlock_error(e: &sqlx::Error) -> bool {
    if let Some(db_err) = e.as_database_error() {
        match db_err.code() {
            Some(code) => {
                let c = code.as_ref();
                c == "1213" || c == "40001" || c == "40P01" || c == "1205"
            }
            None => false,
        }
    } else {
        false
    }
}

fn is_valid_table_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return false;
    }
    name.split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
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
        crate::support::tls_check::warn_unverified("sqlx", config.tls.accept_invalid_certs);
    } else if url.scheme() != "sqlite" {
        crate::support::tls_check::warn_plaintext_credentials(
            "sqlx",
            url.as_str(),
            false,
            url_enforces_tls(&url),
        );
    }

    if config.tls.required {
        let ca_file =
            crate::support::tls_check::ca_file_path("sqlx", &config.tls, CaRoots::Bundled)?;
        let scheme = url.scheme().to_string();
        match scheme.as_str() {
            "postgres" | "postgresql" => {
                let mut query_pairs = url.query_pairs_mut();
                if config.tls.accept_invalid_certs {
                    // Explicitly opted out of validation: encrypt but do not verify.
                    query_pairs.append_pair("sslmode", "require");
                } else {
                    // Validation enabled: verify the chain and hostname. `sslrootcert`
                    // (appended below) supplies a custom CA when configured; without one
                    // the driver's bundled public roots are used.
                    query_pairs.append_pair("sslmode", "verify-full");
                }

                if let Some(ca) = ca_file {
                    query_pairs.append_pair("sslrootcert", ca);
                }
                if let Some(cert) = &config.tls.cert_file {
                    query_pairs.append_pair("sslcert", cert);
                }
                if let Some(key) = &config.tls.key_file {
                    query_pairs.append_pair("sslkey", key);
                }
                // The driver reads no key password and would log one it finds in the URL.
                if config.tls.cert_password.is_some() {
                    warn!(
                        "sqlx ignores tls.cert_password: the client key file must be unencrypted."
                    );
                }
            }
            "mysql" | "mariadb" => {
                // MySQL/MariaDB support for TLS options in URL is more limited.
                // It's generally better to use a client-side configuration file (`my.cnf`)
                // for complex TLS setups. We'll add what we can.
                warn!("For complex MySQL/MariaDB TLS setups, using a client configuration file (my.cnf) is recommended over URL parameters.");
                // The driver takes the last `ssl-mode`: a URL mode that enforces TLS is left
                // to win, a weaker one is overridden by the mode `tls` requires.
                let url_sets_mode = url_enforces_tls(&url);
                let mut query_pairs = url.query_pairs_mut();
                if url_sets_mode {
                } else if config.tls.accept_invalid_certs {
                    // Explicitly opted out of validation: encrypt but do not verify.
                    query_pairs.append_pair("ssl-mode", "REQUIRED");
                } else {
                    // Chain and host name, against `ssl-ca` or the driver's bundled public roots.
                    query_pairs.append_pair("ssl-mode", "VERIFY_IDENTITY");
                }
                if let Some(ca) = ca_file {
                    query_pairs.append_pair("ssl-ca", ca);
                }
                if let Some(cert) = &config.tls.cert_file {
                    query_pairs.append_pair("ssl-cert", cert);
                }
                if let Some(key) = &config.tls.key_file {
                    query_pairs.append_pair("ssl-key", key);
                }
                if config.tls.cert_password.is_some() {
                    warn!(
                        "sqlx ignores tls.cert_password: the client key file must be unencrypted."
                    );
                }
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

/// Whether the URL itself makes the driver refuse an unencrypted connection. The drivers'
/// defaults (`sslmode=prefer`, `ssl-mode=PREFERRED`) fall back to plain text, so they do not count.
/// An option only counts for the driver that reads it, and a repeated one as its last value,
/// which is the one the driver applies.
fn url_enforces_tls(url: &url::Url) -> bool {
    let scheme = url.scheme();
    url.query_pairs()
        .filter_map(|(key, value)| {
            let value = value.to_ascii_lowercase();
            match (scheme, key.as_ref()) {
                ("postgres" | "postgresql" | "mysql" | "mariadb", "sslmode" | "ssl-mode") => {
                    Some(value.starts_with("verify") || value.starts_with("require"))
                }
                ("mssql" | "sqlserver", "encrypt") => Some(value == "true" || value == "strict"),
                _ => None,
            }
        })
        .last()
        .unwrap_or(false)
}

/// For a SQLite sink whose file does not exist and whose URL names no `mode`: the URL
/// with `mode=rwc`, so the sink creates the file instead of failing to open it.
fn sqlite_url_creating_missing_file(url: &str) -> Option<String> {
    let rest = url.strip_prefix("sqlite:")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = path.strip_prefix("//").unwrap_or(path);
    if path.is_empty() || path.contains(":memory:") {
        return None;
    }
    if query
        .split('&')
        .any(|p| p.starts_with("mode=") || p == "immutable=true")
    {
        return None;
    }
    if std::path::Path::new(path).exists() {
        return None;
    }
    let separator = if query.is_empty() { '?' } else { '&' };
    Some(format!("{url}{separator}mode=rwc"))
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
    // sqlx defaults this to true, which pings the server on every acquire — a second round-trip
    // per batch. Dead connections instead surface as a query error, which the route already
    // reconnects on.
    pool_options = pool_options.test_before_acquire(config.test_before_acquire.unwrap_or(false));

    // The pool retries a refused connection silently until `acquire_timeout`; one direct
    // attempt first reports why the server cannot be reached.
    if !url.starts_with("sqlite") {
        use sqlx::Connection;
        let limit = Duration::from_millis(config.acquire_timeout_ms.unwrap_or(30_000));
        let probe = tokio::time::timeout(limit, sqlx::AnyConnection::connect(&url))
            .await
            .map_err(|_| anyhow!("connecting to the database timed out after {limit:?}"))??;
        probe.close().await.ok();
    }

    // sqlx logs a statement slower than 1 s at WARN with its full text, which for a batch
    // INSERT is up to a megabyte on one line.
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let connect_options = sqlx::any::AnyConnectOptions::from_str(&url)?
        .log_slow_statements(log::LevelFilter::Debug, Duration::from_secs(1));
    Ok(pool_options.connect_with(connect_options).await?)
}

/// Returns a shared connection pool for this database, building one on first use.
async fn create_shared_sqlx_pool(config: &SqlxConfig) -> anyhow::Result<std::sync::Arc<AnyPool>> {
    let identity = crate::support::connection_registry::connection_identity((
        &config.url,
        &config.username,
        &config.password,
        config.tls.required,
        &config.tls.ca_file,
        &config.tls.cert_file,
        &config.tls.key_file,
        &config.tls.cert_password,
        config.tls.accept_invalid_certs,
        (
            config.max_connections,
            config.min_connections,
            config.acquire_timeout_ms,
            config.idle_timeout_ms,
            config.max_lifetime_ms,
            config.test_before_acquire.unwrap_or(false),
        ),
    ));
    let config_clone = config.clone();
    crate::support::connection_registry::get_or_create(
        "sqlx-pool",
        identity,
        config.shared.unwrap_or(true),
        move || async move { create_sqlx_pool(&config_clone).await },
    )
    .await
}

pub struct SqlxPublisher {
    pool: AnyPool,
    // Retains the shared registry entry so concurrent publishers reuse this pool.
    _shared_pool: std::sync::Arc<AnyPool>,
    insert_query: String,
    /// Ordered value sources for token-based multi-column inserts. Empty = legacy
    /// single-payload-bind mode (query has no `${...}` tokens).
    column_sources: Vec<ColumnSource>,
    bind_shape: BindShape,
    driver_name: String,
    table: String,
    /// Present when `bulk_copy` is enabled (PostgreSQL, token-based query). When set,
    /// `send_batch` streams rows via `COPY FROM STDIN` instead of a multi-row INSERT.
    copy: Option<PgCopySink>,
    /// Set by `lookup_query`: a read-only lookup publisher.
    lookup: Option<SqlLookup>,
    /// PostgreSQL with a writing `lookup_query`: a batch runs as one call to a generated function.
    pg_batch: Option<PgBatchLookup>,
    /// Set by `columns: auto`: rows are built from the record's fields, not from `insert_query`.
    auto: Option<columns::AutoColumns>,
    /// Set by `load_mode`: every write goes into one transaction, committed when the route completes.
    load: Option<load::HeldLoad>,
}

/// Bulk-load sink using PostgreSQL `COPY FROM STDIN`. `columns[i]` receives the value
/// resolved from `sources[i]` — positional, mirroring the token-based INSERT.
struct PgCopySink {
    pool: CopyPool,
    table: String,
    columns: Vec<String>,
    sources: Vec<ColumnSource>,
}

#[cfg(feature = "sqlx")]
type CopyPool = PgPool;
/// Without the PostgreSQL driver (`sqlite` only) there is no COPY pool.
#[cfg(not(feature = "sqlx"))]
enum CopyPool {}

/// Dedicated native Postgres pool: COPY needs the typed pg protocol, not the `Any` layer.
#[cfg(not(feature = "sqlx"))]
async fn open_copy_pool(_config: &SqlxConfig) -> anyhow::Result<CopyPool> {
    Err(anyhow!(
        "bulk_copy needs PostgreSQL support (feature `sqlx`)"
    ))
}

/// Dedicated native Postgres pool: COPY needs the typed pg protocol, not the `Any` layer.
#[cfg(feature = "sqlx")]
async fn open_copy_pool(config: &SqlxConfig) -> anyhow::Result<CopyPool> {
    let url = build_sqlx_url_with_tls(config)?;
    PgPoolOptions::new()
        .max_connections(config.max_connections.unwrap_or(5))
        // Match `create_sqlx_pool`: no liveness ping per acquire.
        .test_before_acquire(config.test_before_acquire.unwrap_or(false))
        .connect(&url)
        .await
        .context("bulk_copy: failed to open native PostgreSQL pool")
}

/// Appends one value in the PostgreSQL COPY *text* format, `\N` for NULL.
fn push_copy_value(buf: &mut String, value: BindValue) {
    match value {
        BindValue::Null => buf.push_str("\\N"),
        BindValue::Int(n) => buf.push_str(&n.to_string()),
        BindValue::Float(f) => buf.push_str(&f.to_string()),
        BindValue::Bool(b) => buf.push_str(if b { "t" } else { "f" }),
        BindValue::Text(s) => buf.push_str(&copy_escape_text(&s)),
    }
}

/// Runs each `(COPY statement, rows)`; several run in one transaction, so a retry cannot write some twice.
#[cfg(feature = "sqlx")]
async fn copy_rows(pool: &CopyPool, copies: &[(String, String)]) -> Result<(), PublisherError> {
    let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
    match copies {
        [] => Ok(()),
        [(stmt, buf)] => {
            let copier = pool.copy_in_raw(stmt).await;
            copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await
        }
        _ => {
            let mut tx = pool.begin().await.map_err(retryable)?;
            for (stmt, buf) in copies {
                let copier = tx.copy_in_raw(stmt).await;
                copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await?;
            }
            tx.commit().await.map_err(retryable)
        }
    }
}

#[cfg(not(feature = "sqlx"))]
async fn copy_rows(pool: &CopyPool, _copies: &[(String, String)]) -> Result<(), PublisherError> {
    match *pool {}
}

/// Runs each `(COPY statement, rows)` on `conn`, inside the transaction it already holds.
#[cfg(feature = "sqlx")]
async fn copy_rows_on(
    conn: &mut load::CopyConn,
    copies: &[(String, String)],
) -> Result<(), PublisherError> {
    for (stmt, buf) in copies {
        let copier = conn.copy_in_raw(stmt).await;
        copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await?;
    }
    Ok(())
}

#[cfg(not(feature = "sqlx"))]
async fn copy_rows_on(
    conn: &mut load::CopyConn,
    _copies: &[(String, String)],
) -> Result<(), PublisherError> {
    match *conn {}
}

/// Sends `buf` as the data of a started `COPY … FROM STDIN` and ends it.
#[cfg(feature = "sqlx")]
async fn copy_in<C>(
    mut copier: sqlx::postgres::PgCopyIn<C>,
    buf: &[u8],
) -> Result<(), PublisherError>
where
    C: std::ops::DerefMut<Target = sqlx::PgConnection>,
{
    if let Err(e) = copier.send(buf).await {
        // Tear the COPY down explicitly. `Drop` only buffers a CopyFail without
        // awaiting the reply, so the connection could go back to the pool still in
        // COPY-in state and poison the next query on it.
        let _ = copier.abort("mq-bridge: COPY send failed").await;
        return Err(classify_sql_error(e));
    }
    copier.finish().await.map_err(classify_sql_error)?;
    Ok(())
}

/// Escape one text value for the PostgreSQL COPY *text* format (tab-separated, NL-terminated).
fn copy_escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

/// Validate that `raw_query` is COPY-compatible and return its ordered column names.
///
/// COPY is positional and cannot evaluate expressions or run `ON CONFLICT`/`RETURNING`,
/// so we require the exact shape `INSERT INTO <table> (c1, .., cn) VALUES (t1, .., tn)`
/// where each `ti` is a `${...}` token and nothing else — guaranteeing `columns[i]`
/// lines up with the i-th resolved value. `token_count` is the number of parsed sources.
/// Classify a `COPY` failure. A database-reported error (bad data, constraint violation, unknown
/// column/table) is deterministic — retrying the identical payload fails the same way — so surface
/// it as non-retryable (dead-letterable) instead of looping forever. Connection/pool/IO failures
/// are transient and stay retryable (at-least-once).
/// True for SQLSTATE classes whose errors are deterministic: the same statement +
/// value fails identically on retry, so retrying can never succeed. Classified by the
/// two-char class prefix (ANSI SQLSTATE, shared by Postgres and MySQL):
///   - `42` syntax error / access-rule violation — undefined column/table, and crucially
///     `42804` datatype_mismatch (text bound into a `numeric`/`timestamptz` column).
///   - `22` data exception — invalid text representation, numeric overflow, bad datetime.
///   - `23` integrity constraint violation (also caught by `ErrorKind` below).
///   - `21` cardinality violation, e.g. one upsert batch updating the same key twice.
///
/// Everything else (08 connection, 40 deadlock/serialization, 53 resources,
/// 55 object-not-ready, 57 operator-intervention, 58 system) is transient and retried.
fn is_deterministic_sqlstate(code: &str) -> bool {
    matches!(
        &code.get(..2),
        Some("42") | Some("22") | Some("23") | Some("21")
    )
}

/// Classifies a SQL error as deterministic (dead-letter) vs transient (retry).
///
/// Deterministic errors — constraint violations and the syntax/data/type classes above —
/// are `NonRetryable`: retrying the identical statement and value fails the same way, so
/// they must dead-letter (or fail the route) rather than loop forever. A `42804` type
/// mismatch could in theory succeed after a concurrent schema migration, but the message
/// belongs in the DLQ for replay, not in an infinite retry that wedges the route.
///
/// Everything else stays `Retryable` (at-least-once): connection drops, pool timeouts,
/// deadlocks/serialization failures, "too many connections", and crash-recovery errors
/// all surface transiently during a restart/failover, and dropping them would lose messages.
/// Shared deterministic-vs-transient decision used by both the sink
/// (`classify_sql_error`) and the source (`classify_sql_consumer_error`).
fn sql_error_is_deterministic(e: &sqlx::Error) -> bool {
    use sqlx::error::ErrorKind;
    let Some(db_err) = e.as_database_error() else {
        return false;
    };
    if matches!(
        db_err.kind(),
        ErrorKind::UniqueViolation
            | ErrorKind::ForeignKeyViolation
            | ErrorKind::NotNullViolation
            | ErrorKind::CheckViolation
            | ErrorKind::ExclusionViolation
    ) {
        return true;
    }
    if db_err.code().is_some_and(|c| is_deterministic_sqlstate(&c)) {
        return true;
    }
    // SQLite reports schema errors as a bare SQLITE_ERROR with no SQLSTATE, so the
    // code/kind checks above miss them. Match the message so a missing column or
    // table fails fast (Postgres `42703`/`42P01` and MySQL `42S22`/`42S02` are
    // already caught by SQLSTATE above).
    let msg = db_err.message().to_ascii_lowercase();
    msg.contains("no such column")
        || msg.contains("no such table")
        || msg.contains("has no column named")
}

/// The error without sqlx's ` at line N`: that is a line of the server's source, not of the query.
fn sql_error_text(e: sqlx::Error) -> anyhow::Error {
    match e.as_database_error() {
        Some(db_err) => anyhow!("error returned from database: {}", db_err.message()),
        None => anyhow!(e),
    }
}

fn classify_sql_error(e: sqlx::Error) -> PublisherError {
    if sql_error_is_deterministic(&e) {
        let mismatch = e
            .as_database_error()
            .and_then(|db_err| db_err.code())
            .is_some_and(|code| code == "42804");
        let error = sql_error_text(e);
        if mismatch {
            return PublisherError::NonRetryable(anyhow!(
                "{error}; cast the value in the query, e.g. `CAST(${{payload:amount}} AS numeric)`, \
                 or use `columns: auto`, which casts every value to its column's type"
            ));
        }
        return PublisherError::NonRetryable(error);
    }
    PublisherError::Retryable(sql_error_text(e))
}

/// Consumer-side twin of [`classify_sql_error`]. A deterministic schema/type/
/// constraint error (missing column, undefined table, bad type) is `Permanent`
/// so the route fails fast instead of reconnecting every `reconnect_interval_ms`
/// forever on an unrecoverable read. Everything else stays `Connection`
/// (retryable) so restarts/failovers recover without losing messages.
///
/// SQLSTATE `21000` (a subquery returning more than one row) depends on the rows a
/// continuous read sees, so it reconnects until the data changes, at the cost of
/// reconnecting forever on a query that is wrong for every row.
fn classify_sql_consumer_error(e: sqlx::Error) -> ConsumerError {
    let cardinality = e
        .as_database_error()
        .and_then(|db_err| db_err.code())
        .is_some_and(|code| code == "21000");
    if sql_error_is_deterministic(&e) && !cardinality {
        ConsumerError::Permanent(sql_error_text(e))
    } else {
        ConsumerError::Connection(sql_error_text(e))
    }
}

/// [`classify_sql_consumer_error`] for the work-queue reader, which needs its own columns.
fn classify_queue_read_error(e: sqlx::Error) -> ConsumerError {
    let missing_queue_column = sql_error_is_deterministic(&e)
        && e.as_database_error().is_some_and(|db_err| {
            let msg = db_err.message().to_ascii_lowercase();
            ["id", "locked_until"].iter().any(|column| {
                msg.contains(&format!("column \"{column}\" does not exist"))
                    || msg.contains(&format!("no such column: {column}"))
                    || msg.contains(&format!("unknown column '{column}'"))
            })
        });
    match classify_sql_consumer_error(e) {
        ConsumerError::Permanent(e) if missing_queue_column => ConsumerError::Permanent(e.context(
            "the table is not an mq-bridge queue table (it lacks `id` or `locked_until`); set `cursor_column=<unique column>` to read a plain table",
        )),
        other => other,
    }
}

/// The INSERT target and its column list, for a COPY into the same table as single-row sends.
fn extract_copy_columns(
    raw_query: &str,
    token_count: usize,
) -> anyhow::Result<(String, Vec<String>)> {
    let upper = raw_query.to_uppercase();
    if upper.contains("ON CONFLICT")
        || upper.contains("RETURNING")
        || upper.contains("ON DUPLICATE")
    {
        return Err(anyhow!(
            "bulk_copy cannot be used with ON CONFLICT/RETURNING/ON DUPLICATE clauses (COPY does not support them)."
        ));
    }
    // Locate VALUES with the ASCII-safe scanner so byte offsets index `raw_query`
    // directly (`upper` may differ in length under non-ASCII uppercasing).
    let values_pos = find_top_level_values(raw_query).ok_or_else(|| {
        anyhow!("bulk_copy requires an INSERT ... VALUES query with a column list.")
    })?;

    // Column list: the parenthesised group in the prefix before VALUES.
    let prefix = &raw_query[..values_pos];
    let open = prefix.find('(').ok_or_else(|| {
        anyhow!(
            "bulk_copy requires an explicit column list, e.g. INSERT INTO t (a, b) VALUES (...)."
        )
    })?;
    let target = prefix[..open]
        .to_ascii_uppercase()
        .find("INTO")
        .map(|into| prefix[into + "INTO".len()..open].trim())
        // COPY takes no alias: `INSERT INTO t AS x (..)` copies into `t`.
        .map(|target| match target.to_ascii_uppercase().rfind(" AS ") {
            Some(alias) => target[..alias].trim_end().to_string(),
            None => target.to_string(),
        })
        .filter(|target| !target.is_empty())
        .ok_or_else(|| anyhow!("bulk_copy: could not find the INSERT INTO table."))?;
    let close = prefix[open..]
        .find(')')
        .map(|off| open + off)
        .ok_or_else(|| anyhow!("bulk_copy: unbalanced parentheses in the column list."))?;
    let columns: Vec<String> = prefix[open + 1..close]
        .split(',')
        .map(|c| c.trim().to_string())
        .collect();
    if columns.iter().any(|c| c.is_empty()) || columns.len() != token_count {
        return Err(anyhow!(
            "bulk_copy: the column list ({} columns) must match the {} `${{...}}` value token(s), one token per column.",
            columns.len(),
            token_count
        ));
    }

    // VALUES tuple must contain only tokens/commas/whitespace so column[i] ↔ value[i] holds.
    let after = &raw_query[values_pos + "VALUES".len()..];
    let vopen = after
        .find('(')
        .ok_or_else(|| anyhow!("bulk_copy: could not find the VALUES tuple."))?;
    let vclose = after[vopen..]
        .find(')')
        .map(|off| vopen + off)
        .ok_or_else(|| anyhow!("bulk_copy: unbalanced parentheses in the VALUES tuple."))?;
    let mut residue = after[vopen + 1..vclose].to_string();
    // Remove every ${...} token, then the remainder must be only commas/whitespace.
    while let Some(s) = residue.find("${") {
        match residue[s..].find('}') {
            Some(e) => residue.replace_range(s..s + e + 1, ""),
            None => break,
        }
    }
    if residue.chars().any(|c| c != ',' && !c.is_whitespace()) {
        return Err(anyhow!(
            "bulk_copy requires every VALUES entry to be a single `${{...}}` token (no literals, expressions, or functions)."
        ));
    }
    if !after[vclose + 1..]
        .trim()
        .trim_end_matches(';')
        .trim()
        .is_empty()
    {
        return Err(anyhow!(
            "bulk_copy requires a single VALUES tuple and nothing after it."
        ));
    }

    Ok((target, columns))
}

impl SqlxPublisher {
    pub async fn new(config: &SqlxConfig) -> anyhow::Result<Self> {
        install_sqlx_drivers();
        if !is_valid_table_name(&config.table) {
            return Err(anyhow!(
                "Invalid table name: '{}'. Only alphanumeric characters and underscores are allowed.",
                config.table
            ));
        }
        let creating = sqlite_url_creating_missing_file(&config.url);
        let config = &match creating {
            Some(url) => SqlxConfig {
                url,
                ..config.clone()
            },
            None => config.clone(),
        };
        let shared_pool = create_shared_sqlx_pool(config).await?;
        let pool = (*shared_pool).clone();

        // Acquire a connection to determine the driver so we can use the correct SQL syntax.
        let driver_name = pool.acquire().await?.backend_name().to_string();

        info!(table = %config.table, driver = %driver_name, "SQLx publisher connected");

        if config.load_mode.is_some() && config.lookup_query.is_some() {
            return Err(invalid(
                "`load_mode` writes a whole load; it cannot be combined with `lookup_query`",
            ));
        }
        if config.load_mode.is_none()
            && (config.before_load.is_some() || config.after_load.is_some())
        {
            return Err(invalid(
                "`before_load` and `after_load` run inside a load; set `load_mode` as well",
            ));
        }
        if config.load_mode.is_some() && matches!(driver_name.as_str(), "MySQL" | "MariaDB") {
            ensure_transactional_engine(&pool, &config.table).await?;
        }
        let load = config.load_mode.map(|mode| {
            load::HeldLoad::new(
                mode,
                &config.table,
                config.before_load.clone(),
                config.after_load.clone(),
            )
        });
        let base = Self {
            pool,
            _shared_pool: shared_pool,
            insert_query: String::new(),
            column_sources: Vec::new(),
            bind_shape: BindShape::default(),
            driver_name,
            table: config.table.clone(),
            copy: None,
            lookup: None,
            pg_batch: None,
            auto: None,
            load: None,
        };
        if let Some(template) = &config.lookup_query {
            let writes = lookup_writes(template);
            if writes {
                info!(table = %config.table, "SQLx lookup_query writes on every lookup");
            }
            let lookup = SqlLookup::parse(template, &base.driver_name)?;
            let pg_batch = (writes
                && base.driver_name == "PostgreSQL"
                && matches!(lookup, SqlLookup::Single(..)))
            .then(|| PgBatchLookup {
                template: template.clone(),
                disabled: Default::default(),
                functions: Default::default(),
            });
            return Ok(Self {
                pg_batch,
                lookup: Some(lookup),
                ..base
            });
        }

        if config.key.is_some() && config.columns.is_none() {
            return Err(invalid(
                "`key` needs `columns: auto`; with an `insert_query`, write the ON CONFLICT clause into the query",
            ));
        }
        if config.extra_column.is_some() && config.columns.is_none() {
            return Err(invalid("`extra_column` needs `columns: auto`"));
        }
        if config.columns.is_some() {
            let auto = columns::AutoColumns::new(&base.pool, &base.driver_name, config).await?;
            return Ok(Self {
                auto: Some(auto),
                load,
                ..base
            });
        }

        // Resolve the insert query and parse any `${metadata:...}`/`${payload:...}`
        // tokens into ordered value sources, rewriting them to positional placeholders.
        let driver_name = base.driver_name.as_str();
        let raw_insert_query = config
            .insert_query
            .clone()
            .unwrap_or_else(|| match driver_name {
                "PostgreSQL" => format!("INSERT INTO {} (payload) VALUES ($1)", config.table),
                "Microsoft SQL Server" => {
                    format!("INSERT INTO {} (payload) VALUES (@p1)", config.table)
                }
                _ => format!("INSERT INTO {} (payload) VALUES (?)", config.table),
            });
        let (insert_query, column_sources) = parse_insert_template(&raw_insert_query, driver_name)?;

        if config.auto_create_table && !column_sources.is_empty() {
            return Err(anyhow!(
                "auto_create_table is not supported with a multi-column insert_query; create the table manually."
            ));
        }
        if config.auto_create_table {
            auto_create_payload_table(&base.pool, driver_name, &config.table).await;
        }

        let copy = if config.bulk_copy {
            if driver_name != "PostgreSQL" {
                return Err(anyhow!(
                    "bulk_copy is only supported for PostgreSQL (driver: {}).",
                    driver_name
                ));
            }
            if column_sources.is_empty() {
                return Err(anyhow!(
                    "bulk_copy requires a token-based insert_query (e.g. INSERT INTO t (a, b) VALUES (${{payload:a}}, ${{payload:b}})); single-payload COPY is not supported."
                ));
            }
            let (table, columns) = extract_copy_columns(&raw_insert_query, column_sources.len())?;
            Some(PgCopySink {
                pool: open_copy_pool(config).await?,
                table,
                columns,
                sources: column_sources.clone(),
            })
        } else {
            None
        };

        Ok(Self {
            insert_query,
            column_sources,
            copy,
            load,
            ..base
        })
    }
}

/// How long a status check waits for the server to answer a ping.
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Healthy only if a pooled connection still answers: the pool hands out an idle one the
/// server has closed unless `test_before_acquire` is set.
async fn pool_health(pool: &AnyPool) -> (bool, Option<String>) {
    use sqlx::Connection;
    let answered = match pool.acquire().await {
        Ok(mut conn) => match tokio::time::timeout(PING_TIMEOUT, conn.ping()).await {
            Ok(answered) => answered,
            Err(_) => {
                // A connection that stopped answering must not go back to the pool.
                conn.close_on_drop();
                return (
                    false,
                    Some(format!("ping timed out after {PING_TIMEOUT:?}")),
                );
            }
        },
        Err(e) => Err(e),
    };
    match answered {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    }
}

fn invalid(message: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(crate::errors::InvalidConfig(anyhow!("{message}")))
}

/// `n` bind placeholders for an `IN (...)` list. Built by hand because the `Any` driver
/// does not rewrite `?` to `$N` for PostgreSQL here.
fn id_placeholders(driver_name: &str, n: usize) -> String {
    (1..=n)
        .map(|i| match driver_name {
            "PostgreSQL" => format!("${i}"),
            "Microsoft SQL Server" => format!("@p{i}"),
            _ => "?".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Creates the default `payload` table and its `locked_until` index; a failure only warns,
/// since the table may exist already under rights that forbid DDL.
async fn auto_create_payload_table(pool: &AnyPool, driver_name: &str, table: &str) {
    let create_table_query = match driver_name {
        "PostgreSQL" => format!(
            "CREATE TABLE IF NOT EXISTS {} (id BIGSERIAL PRIMARY KEY, payload BYTEA NOT NULL, locked_until TIMESTAMPTZ, created_at TIMESTAMPTZ DEFAULT NOW())",
            table
        ),
        "MySQL" | "MariaDB" => format!(
            "CREATE TABLE IF NOT EXISTS {} (id BIGINT AUTO_INCREMENT PRIMARY KEY, payload BLOB NOT NULL, locked_until DATETIME, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            table
        ),
        "SQLite" => format!(
            "CREATE TABLE IF NOT EXISTS {} (id INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB NOT NULL, locked_until DATETIME, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            table
        ),
        "Microsoft SQL Server" => format!(
            "IF NOT EXISTS (SELECT * FROM sys.objects WHERE object_id = OBJECT_ID(N'{0}') AND type in (N'U'))
                CREATE TABLE {0} (id BIGINT IDENTITY(1,1) PRIMARY KEY, payload VARBINARY(MAX) NOT NULL, locked_until DATETIME2, created_at DATETIME2 DEFAULT GETUTCDATE())",
            table
        ),
        _ => return, // Don't attempt for unknown drivers
    };
    if let Err(e) = sqlx::query(audited_sql(&create_table_query))
        .execute(pool)
        .await
    {
        warn!(
            "Failed to auto-create table '{}': {}. Please ensure it exists.",
            table, e
        );
        return;
    }

    let table_name_for_index = table.split('.').next_back().unwrap_or(table);
    let index_name = format!("idx_{}_locked_until", table_name_for_index);
    let create_index_query = match driver_name {
        "PostgreSQL" | "SQLite" | "MariaDB" => format!(
            "CREATE INDEX IF NOT EXISTS {} ON {} (locked_until)",
            index_name, table
        ),
        "MySQL" => format!("CREATE INDEX {} ON {} (locked_until)", index_name, table),
        "Microsoft SQL Server" => format!(
            "IF NOT EXISTS (SELECT * FROM sys.indexes WHERE name = N'{}' AND object_id = OBJECT_ID(N'{}'))
                                CREATE INDEX {} ON {} (locked_until)",
            index_name, table, index_name, table
        ),
        _ => return,
    };
    let Err(e) = sqlx::query(audited_sql(&create_index_query))
        .execute(pool)
        .await
    else {
        return;
    };
    let driver_lc = driver_name.to_lowercase();
    if (driver_lc.contains("mysql") || driver_lc.contains("mariadb"))
        && e.as_database_error()
            .is_some_and(|db_err| db_err.code().as_deref() == Some("1061"))
    {
        trace!("Index {} on {} already exists.", index_name, table);
    } else {
        warn!("Failed to create index on '{}': {}", table, e);
    }
}

#[async_trait]
impl MessagePublisher for SqlxPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if self.lookup.is_some() {
            return self.lookup_one(&message).await;
        }
        if let Some(load) = &self.load {
            return self.send_held(load, &[message]).await.map(|()| Sent::Ack);
        }
        if let Some(auto) = &self.auto {
            return match auto.send(&self.pool, vec![message]).await? {
                SentBatch::Partial { mut failed, .. } if !failed.is_empty() => {
                    Err(failed.swap_remove(0).1)
                }
                _ => Ok(Sent::Ack),
            };
        }
        trace!(message_id = %format!("{:032x}", message.message_id), table = %self.table, "Publishing to SQL");
        let query = sqlx::query(audited_sql(&self.insert_query));
        let query = if self.column_sources.is_empty() {
            query.bind(message.payload.to_vec())
        } else {
            let (query, cacheable) =
                bind_cacheable(query, &message, &self.column_sources, &self.bind_shape)?;
            query.persistent(cacheable)
        };
        query
            .execute(&self.pool)
            .await
            .map_err(classify_sql_error)?;
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        if self.lookup.is_some() {
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        if let Some(load) = &self.load {
            return self
                .send_held(load, &messages)
                .await
                .map(|()| SentBatch::Ack);
        }

        if let Some(auto) = &self.auto {
            return auto.send(&self.pool, messages).await;
        }

        match self.write_batch(&messages, None).await {
            Err(PublisherError::NonRetryable(e)) if messages.len() > 1 => {
                warn!(error = %e, "SQLx batch rejected; writing it one message at a time");
                self.send_batch_isolating(messages).await
            }
            other => other.map(|()| SentBatch::Ack),
        }
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = pool_health(&self.pool).await;

        EndpointStatus {
            healthy,
            target: self.table.clone(),
            error,
            details: serde_json::json!({ "driver": self.driver_name, "pool_size": self.pool.size(), "pool_idle": self.pool.num_idle() }),
            ..Default::default()
        }
    }

    async fn lookup_batch(
        &self,
        requests: &[CanonicalMessage],
    ) -> Option<Result<Vec<Option<serde_json::Value>>, PublisherError>> {
        match &self.lookup {
            Some(SqlLookup::List { query, source }) => {
                Some(self.lookup_list(query, source, requests).await)
            }
            Some(SqlLookup::Single(sql, sources)) => {
                self.lookup_writing_batch(sql, sources, requests).await
            }
            _ => None,
        }
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        let load = self.load.as_ref()?;
        Some(Box::pin(load.finish()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SqlxPublisher {
    /// `load_mode`: writes into the held transaction; one rejected row fails the whole load.
    async fn send_held(
        &self,
        load: &load::HeldLoad,
        messages: &[CanonicalMessage],
    ) -> Result<(), PublisherError> {
        let copy_pool = match &self.auto {
            Some(auto) => auto.copy_pool(),
            None => self.copy.as_ref().map(|sink| &sink.pool),
        };
        let mut write = load.lock(&self.pool, copy_pool).await?;
        let result = match (&self.auto, write.tx()) {
            (_, Err(e)) => Err(e),
            (Some(auto), Ok(tx)) => auto.send_held(&self.pool, tx, messages).await,
            (None, Ok(tx)) => self.write_batch(messages, Some(tx)).await,
        };
        write.settle(result)
    }

    /// Writes the batch with as few statements as the query allows; all of it or nothing.
    /// With `held`, it writes into that transaction and leaves committing to its owner.
    async fn write_batch(
        &self,
        messages: &[CanonicalMessage],
        mut held: Option<&mut load::HeldTx>,
    ) -> Result<(), PublisherError> {
        if let Some(sink) = &self.copy {
            return self.send_batch_copy(sink, messages, held).await;
        }

        trace!(count = messages.len(), message_ids = ?LazyMessageIds(messages), "Publishing batch to SQLx");

        // Manually construct the query with appropriate placeholders because
        // sqlx::QueryBuilder with the `Any` driver does not correctly rewrite `?` to `$N`.
        let values_pos = match find_top_level_values(&self.insert_query) {
            Some(pos) => pos,
            None => {
                warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
                return self.send_batch_iterative(messages, held).await;
            }
        };
        let base_query = &self.insert_query[..values_pos];
        // Preserve any clause after the VALUES tuple (e.g. ON CONFLICT … DO UPDATE, ON DUPLICATE
        // KEY UPDATE, RETURNING). Single-row send() keeps it verbatim; without this the batch
        // rebuild would silently drop it, making batched inserts non-idempotent.
        let after_values = values_pos + "VALUES".len();
        let (values_tuple, values_suffix) = match self.insert_query[after_values..].find('(') {
            Some(rel_open) => {
                let open = after_values + rel_open;
                let mut depth = 0usize;
                let mut end = None;
                for (idx, ch) in self.insert_query[open..].char_indices() {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(open + idx + ch.len_utf8());
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                match end {
                    Some(e) => (&self.insert_query[open..e], &self.insert_query[e..]),
                    None => ("", ""),
                }
            }
            None => ("", ""),
        };

        // A token after the VALUES tuple (e.g. `ON CONFLICT … DO UPDATE SET v = ${payload:v}`)
        // was already rewritten to a placeholder by `parse_insert_template`, numbered for the
        // single-row query. The rebuild below renumbers only the row tuples, so the suffix
        // would keep a stale index and the bind count would not line up. Single-row `send()`
        // handles these correctly.
        if binds_after_values(values_suffix) {
            warn!("insert_query binds a value after the VALUES tuple. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages, held).await;
        }
        // A further tuple is part of every single-row insert; the rebuild would write it once.
        // An unterminated comment is ambiguous, so it is treated as a further tuple.
        if skip_sql_comments(values_suffix).is_none_or(|rest| rest.starts_with(',')) {
            warn!(
                "insert_query has more than one VALUES tuple. Falling back to iterative inserts."
            );
            return self.send_batch_iterative(messages, held).await;
        }

        // The `(payload)` single-column guard only applies to legacy mode; a
        // token-based query is already known-correct from `parse_insert_template`.
        if self.column_sources.is_empty() && !contains_payload_clause(base_query) {
            warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages, held).await;
        }

        // Placeholders per row: N tokens in token mode, 1 (the payload) in legacy mode.
        let per_row = self.column_sources.len().max(1);
        // The tuple is repeated per row with its expressions (`CAST(…)`, `decode(…)`) kept.
        let Some(template) = tuple_template(values_tuple, &self.driver_name, per_row) else {
            warn!("insert_query's VALUES tuple cannot be repeated per row. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages, held).await;
        };
        let per_statement = (columns::MAX_BINDS / per_row).max(1);
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
        // One statement is atomic by itself; a batch split at the bind limit needs a transaction.
        let mut tx = match held.is_none() && messages.len() > per_statement {
            true => Some(self.pool.begin().await.map_err(retryable)?),
            false => None,
        };
        for chunk in messages.chunks(per_statement) {
            let mut placeholders = String::new();
            for i in 0..chunk.len() {
                if i > 0 {
                    placeholders.push_str(", ");
                }
                render_tuple(&template, &self.driver_name, i * per_row, &mut placeholders);
            }

            let sql = format!("{} VALUES {}{}", base_query, placeholders, values_suffix);

            let mut query = sqlx::query(audited_sql(&sql));
            let mut cacheable = true;
            for msg in chunk {
                if self.column_sources.is_empty() {
                    query = query.bind(msg.payload.to_vec());
                } else {
                    let (bound, same_shape) =
                        bind_cacheable(query, msg, &self.column_sources, &self.bind_shape)?;
                    query = bound;
                    cacheable &= same_shape;
                }
            }
            let query = query.persistent(cacheable);
            match (&mut held, &mut tx) {
                (Some(held), _) => query.execute(held.any()?).await,
                (None, Some(tx)) => query.execute(&mut **tx).await,
                (None, None) => query.execute(&self.pool).await,
            }
            .map_err(classify_sql_error)?;
        }
        if let Some(tx) = tx {
            tx.commit().await.map_err(retryable)?;
        }
        Ok(())
    }

    /// Redoes a batch the database rejected, a savepoint per message: only the rejected
    /// messages fail, any other failure rolls every write of the redo back.
    async fn send_batch_isolating(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        use sqlx::Acquire;
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
        let mut tx = self.pool.begin().await.map_err(retryable)?;
        let mut failed = Vec::new();
        for message in messages {
            // Not cached: a statement prepared for one message's bind types fails the next.
            let query = sqlx::query(audited_sql(&self.insert_query)).persistent(false);
            let bound = match self.column_sources.is_empty() {
                true => Ok(query.bind(message.payload.to_vec())),
                false => bind_message_sources(query, &message, &self.column_sources),
            };
            let written = match bound {
                Ok(query) => {
                    let mut step = tx.begin().await.map_err(retryable)?;
                    match query.execute(&mut *step).await.map_err(classify_sql_error) {
                        Ok(_) => step.commit().await.map_err(retryable),
                        Err(e) => step.rollback().await.map_err(retryable).and(Err(e)),
                    }
                }
                Err(e) => Err(e),
            };
            match written {
                Ok(()) => {}
                Err(e @ PublisherError::NonRetryable(_)) => failed.push((message, e)),
                Err(e) => return Err(e),
            }
        }
        tx.commit().await.map_err(retryable)?;
        Ok(SentBatch::from_failures(failed))
    }

    /// Bulk-load a batch via PostgreSQL `COPY FROM STDIN` (text format). Each row is a
    /// tab-separated line of the resolved token values, `\N` for NULL. Far faster than a
    /// multi-row INSERT for large batches; retryable on transport errors (at-least-once).
    async fn send_batch_copy(
        &self,
        sink: &PgCopySink,
        messages: &[CanonicalMessage],
        held: Option<&mut load::HeldTx>,
    ) -> Result<(), PublisherError> {
        let stmt = format!(
            "COPY {} ({}) FROM STDIN WITH (FORMAT text)",
            sink.table,
            sink.columns.join(", ")
        );

        let mut buf = String::new();
        for msg in messages {
            let payload_json = payload_fields(msg, &sink.sources);
            for (i, source) in sink.sources.iter().enumerate() {
                if i > 0 {
                    buf.push('\t');
                }
                let value = resolve_source(msg, source, &payload_json);
                reject_embedded_nul(source, &value)?;
                push_copy_value(&mut buf, value);
            }
            buf.push('\n');
        }

        match held {
            Some(held) => copy_rows_on(held.pg()?, &[(stmt, buf)]).await?,
            None => copy_rows(&sink.pool, &[(stmt, buf)]).await?,
        }

        trace!(count = messages.len(), table = %sink.table, "Bulk-copied batch to PostgreSQL");
        Ok(())
    }

    /// Fallback implementation that inserts messages one by one within a transaction.
    /// This is less performant than a single multi-row insert statement.
    async fn send_batch_iterative(
        &self,
        messages: &[CanonicalMessage],
        held: Option<&mut load::HeldTx>,
    ) -> Result<(), PublisherError> {
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
        let mut own = None;
        let conn = match held {
            Some(held) => held.any()?,
            None => &mut **own.insert(self.pool.begin().await.map_err(retryable)?),
        };
        for msg in messages {
            let query = sqlx::query(audited_sql(&self.insert_query));
            let query = if self.column_sources.is_empty() {
                query.bind(msg.payload.to_vec())
            } else {
                let (query, cacheable) =
                    bind_cacheable(query, msg, &self.column_sources, &self.bind_shape)?;
                query.persistent(cacheable)
            };
            query
                .execute(&mut *conn)
                .await
                .map_err(classify_sql_error)?;
        }
        if let Some(tx) = own {
            tx.commit().await.map_err(retryable)?;
        }
        Ok(())
    }
}

pub struct SqlxConsumer {
    pool: AnyPool,
    select_query: String,
    delete_after_read: bool,
    table: String,
    backoff: PollBackoff,
    driver_name: String,
}

impl SqlxConsumer {
    pub async fn new(config: &SqlxConfig) -> anyhow::Result<Self> {
        install_sqlx_drivers();
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
            backoff: PollBackoff::new(
                Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
                config.max_polling_interval_ms.map(Duration::from_millis),
            ),
            driver_name,
        })
    }
}

impl SqlxConsumer {
    async fn fetch_and_lock_mysql(
        &self,
        limit: usize,
    ) -> Result<Vec<sqlx::any::AnyRow>, ConsumerError> {
        let mut tx = self.pool.begin().await.map_err(classify_queue_read_error)?;

        let lock_query = format!(
            "SELECT id FROM {} WHERE locked_until IS NULL OR locked_until < NOW() ORDER BY id LIMIT ? FOR UPDATE SKIP LOCKED",
            self.table
        );

        let locked_ids: Vec<i64> = sqlx::query(audited_sql(&lock_query))
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?
            .iter()
            .map(|row| row.try_get("id"))
            .collect::<Result<_, _>>()
            .map_err(classify_queue_read_error)?;

        if locked_ids.is_empty() {
            tx.commit().await.ok(); // Nothing to do, commit and return
            return Ok(vec![]);
        }

        // Update the `locked_until` for the locked rows
        let placeholders = locked_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let update_query = format!(
            "UPDATE {} SET locked_until = NOW() + INTERVAL 60 SECOND WHERE id IN ({})",
            self.table, placeholders
        );

        let mut query = sqlx::query(audited_sql(&update_query));
        for id in &locked_ids {
            query = query.bind(*id);
        }

        query
            .execute(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?;

        // Select the full rows that we just locked
        let select_query = format!(
            "SELECT id, payload FROM {} WHERE id IN ({})",
            self.table, placeholders
        );

        let mut query = sqlx::query(audited_sql(&select_query));
        for id in &locked_ids {
            query = query.bind(*id);
        }

        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?;

        tx.commit().await.map_err(classify_queue_read_error)?;

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
            .map_err(classify_queue_read_error)?;

        let select_query = format!(
            "SELECT id FROM {} WHERE locked_until IS NULL OR locked_until < datetime('now') ORDER BY id LIMIT ?",
            self.table
        );

        let locked_ids: Vec<i64> = sqlx::query(audited_sql(&select_query))
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?
            .iter()
            .map(|row| row.try_get("id"))
            .collect::<Result<_, _>>()
            .map_err(classify_queue_read_error)?;

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

        let mut query = sqlx::query(audited_sql(&update_query));
        for id in &locked_ids {
            query = query.bind(*id);
        }
        query
            .execute(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?;

        let select_payload_query = format!(
            "SELECT id, payload FROM {} WHERE id IN ({})",
            self.table, placeholders
        );
        let mut query = sqlx::query(audited_sql(&select_payload_query));
        for id in &locked_ids {
            query = query.bind(*id);
        }
        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(classify_queue_read_error)?;

        tx.commit().await.map_err(classify_queue_read_error)?;

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

        let row: sqlx::any::AnyRow = sqlx::query(audited_sql(&query))
            .fetch_one(&self.pool)
            .await?;
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
    // Acking deletes rows by id (`DELETE ... WHERE id IN (...)`), so each batch's
    // commit is independent; out-of-order concurrent commits cannot lose other
    // batches' rows.
    fn commit_requires_order(&self) -> bool {
        false
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        let rows = match self.driver_name.as_str() {
            "PostgreSQL" | "Microsoft SQL Server" => sqlx::query(audited_sql(&self.select_query))
                .bind(max_messages as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(classify_queue_read_error)?,
            "MySQL" | "MariaDB" => self.fetch_and_lock_mysql(max_messages).await?,
            "SQLite" => self.fetch_and_lock_sqlite(max_messages).await?,
            _ => {
                // Fallback for unknown drivers with a simple, non-locking read.
                warn!("SQLx consumer for driver '{}' is using a non-locking read strategy. This is not safe for concurrent consumers.", self.driver_name);
                let final_query = format!("{} LIMIT ?", self.select_query);
                sqlx::query(audited_sql(&final_query))
                    .bind(max_messages as i64)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(classify_queue_read_error)?
            }
        };

        if rows.is_empty() {
            // Source is drained: sleep to preserve the DB polling cadence (backing off if
            // configured), then surface an empty batch so the route can pause
            // (empty_batch_delay_ms) or, when exit_on_empty is set, terminate gracefully.
            tokio::time::sleep(self.backoff.idle_delay()).await;
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        // Rows arrived: return to the base polling interval.
        self.backoff.reset();

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

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let pool = pool.clone();
            let table = table.clone();
            let ids = ids_to_delete.clone();
            let driver_name = driver_name.clone();
            Box::pin(async move {
                if !delete {
                    return Ok(());
                }
                let mut ids_to_ack = Vec::new();
                let mut ids_to_release = Vec::new();
                for (i, disp) in dispositions.iter().enumerate() {
                    let should_ack = match disp {
                        MessageDisposition::Ack => true,
                        MessageDisposition::Reply(_) => {
                            tracing::warn!("SQLx consumer received a Reply/StreamReply, but replying is not supported by this endpoint. The reply payload is dropped, and the original message is acknowledged.");
                            true
                        }
                        MessageDisposition::Nack => false,
                    };

                    if let Some(id) = ids.get(i) {
                        if should_ack {
                            ids_to_ack.push(*id);
                        } else {
                            ids_to_release.push(*id);
                        }
                    }
                }

                // A nacked row is read again at once instead of after its lock expires.
                if !ids_to_release.is_empty() {
                    let sql = format!(
                        "UPDATE {} SET locked_until = NULL WHERE id IN ({})",
                        table,
                        id_placeholders(&driver_name, ids_to_release.len())
                    );
                    let mut query = sqlx::query(audited_sql(&sql)).persistent(false);
                    for id in &ids_to_release {
                        query = query.bind(*id);
                    }
                    if let Err(e) = query.execute(&pool).await {
                        warn!(error = %e, "Failed to release nacked rows; they are read again once their lock expires");
                    }
                }

                if !ids_to_ack.is_empty() {
                    let placeholders = id_placeholders(&driver_name, ids_to_ack.len());
                    let sql = format!("DELETE FROM {} WHERE id IN ({})", table, placeholders);

                    let mut attempts = 0;
                    loop {
                        // Not cached: a Postgres generic plan made on a small table stays a seq scan.
                        let mut query = sqlx::query(audited_sql(&sql)).persistent(false);
                        for id in &ids_to_ack {
                            query = query.bind(*id);
                        }

                        match query.execute(&pool).await {
                            Ok(_) => break,
                            Err(e) => {
                                if is_deadlock_error(&e) && attempts < 5 {
                                    attempts += 1;
                                    warn!(
                                        attempts,
                                        error = %e,
                                        "Deadlock detected during SQLx commit, retrying..."
                                    );
                                    tokio::time::sleep(Duration::from_millis(attempts * 50)).await;
                                    continue;
                                }
                                return Err(anyhow!("Failed to delete acked messages: {}", e));
                            }
                        }
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
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
mod tests;
