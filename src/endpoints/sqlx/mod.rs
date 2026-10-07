//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::poll::PollBackoff;
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::SqlxConfig;
use crate::support::lookup_batch::{self, ListQuery};
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use sqlx::any::AnyPoolOptions;
use sqlx::postgres::{PgPool, PgPoolCopyExt, PgPoolOptions};
use sqlx::{AnyPool, AssertSqlSafe, Column, Row};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, trace, warn};

mod columns;
#[cfg(feature = "dedup")]
mod dedup;
#[cfg(feature = "dedup")]
pub(crate) use dedup::build_sql_dedup_store;
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

/// Checks if a SQL query string contains a `(payload)` clause, ignoring case and whitespace.
/// Locate the top-level `VALUES` keyword that introduces the insert tuple.
///
/// Scans at parenthesis-depth 0 and returns the *first* such keyword, so a
/// `values` column inside the column list (which sits at depth 1) and a MySQL
/// `VALUES(col)` reference in an `ON DUPLICATE KEY UPDATE` suffix (which follows
/// the real keyword) are both ignored. Word-boundary checks avoid matching
/// substrings like `myvalues`. Returns `None` when no top-level keyword is
/// found, letting the caller fall back to iterative inserts.
fn find_top_level_values(sql: &str) -> Option<usize> {
    fn is_word_byte(b: Option<u8>) -> bool {
        matches!(b, Some(c) if c.is_ascii_alphanumeric() || c == b'_')
    }

    let bytes = sql.as_bytes();
    let mut depth: i32 = 0;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0
                    && bytes.len() - i >= 6
                    && bytes[i..i + 6].eq_ignore_ascii_case(b"VALUES")
                    && !is_word_byte(i.checked_sub(1).map(|p| bytes[p]))
                    && !is_word_byte(bytes.get(i + 6).copied())
                {
                    return Some(i);
                }
            }
        }
    }
    None
}

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

fn audited_sql(sql: &str) -> AssertSqlSafe<&str> {
    AssertSqlSafe(sql)
}

/// Where a single bound value in a token-based `insert_query` comes from.
/// Resolved per-row; never falls back between the two sources.
#[derive(Debug, Clone, PartialEq)]
enum ColumnSource {
    /// `${metadata:<key>}` — `message.metadata.get(key)`, else NULL.
    Metadata(String),
    /// `${payload:<field>}` — top-level JSON field of the payload, else NULL.
    Payload(String),
}

/// A value ready to be bound, preserving JSON scalar type so strict dialects
/// (Postgres/MSSQL) accept numeric/bool columns instead of erroring on text.
#[derive(Debug, Clone, PartialEq)]
enum BindValue {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

/// Driver-specific positional placeholder for the given 1-based index.
fn positional_placeholder(driver_name: &str, index: usize) -> String {
    match driver_name {
        "PostgreSQL" => format!("${}", index),
        "Microsoft SQL Server" => format!("@p{}", index),
        _ => "?".to_string(),
    }
}

/// True when `sql` still binds a value: an unrewritten `${…}` token, or a positional
/// placeholder in any of the driver spellings (`?`, `$N`, `@pN`).
fn binds_after_values(sql: &str) -> bool {
    if sql.contains("${") {
        return true;
    }
    let bytes = sql.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| match b {
        b'?' => true,
        b'$' => bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit()),
        b'@' => bytes.get(i + 1) == Some(&b'p'),
        _ => false,
    })
}

/// True when `tuple` (including its outer parens) is nothing but comma-separated
/// positional placeholders, i.e. the batch rebuild can regenerate it losslessly.
/// Anything else — `decode($1, 'base64')`, `$1::bytea`, `now()`, a literal — must keep
/// the user's own SQL.
fn tuple_is_bare_placeholders(tuple: &str, driver_name: &str) -> bool {
    let inner = tuple
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'));
    let Some(inner) = inner else {
        return false;
    };
    inner.split(',').all(|part| {
        let part = part.trim();
        match driver_name {
            "PostgreSQL" => part
                .strip_prefix('$')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
            "Microsoft SQL Server" => part
                .strip_prefix("@p")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
            _ => part == "?",
        }
    })
}

/// Parse `${metadata:<key>}` / `${payload:<field>}` tokens out of an `insert_query`,
/// rewriting each into a driver-appropriate positional placeholder assigned a running
/// 1-based index in encounter order. Returns the rewritten query and the ordered
/// sources (`sources[i]` resolves the value for the i-th placeholder). An empty
/// `Vec` means the query had no tokens → legacy single-payload-bind mode.
fn parse_insert_template(
    query: &str,
    driver_name: &str,
) -> anyhow::Result<(String, Vec<ColumnSource>)> {
    parse_template_with(query, |index| positional_placeholder(driver_name, index))
}

/// [`parse_insert_template`] with the text each 1-based token index is rewritten to.
fn parse_template_with(
    query: &str,
    placeholder: impl Fn(usize) -> String,
) -> anyhow::Result<(String, Vec<ColumnSource>)> {
    let mut out = String::with_capacity(query.len());
    let mut sources: Vec<ColumnSource> = Vec::new();
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find the closing brace.
            let close = query[i + 2..].find('}').map(|off| i + 2 + off);
            let close = close.ok_or_else(|| {
                anyhow!(
                    "Malformed token in insert_query: unclosed '${{' near '{}'",
                    &query[i..]
                )
            })?;
            let inner = &query[i + 2..close];
            let (prefix, name) = inner.split_once(':').ok_or_else(|| {
                anyhow!("Malformed token in insert_query: '${{{}}}' is missing a ':' separator (expected ${{metadata:key}} or ${{payload:field}})", inner)
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(anyhow!(
                    "Malformed token in insert_query: '${{{}}}' has an empty key/field name",
                    inner
                ));
            }
            let source = match prefix.trim() {
                "metadata" => ColumnSource::Metadata(name.to_string()),
                "payload" => ColumnSource::Payload(name.to_string()),
                other => {
                    return Err(anyhow!(
                        "Malformed token in insert_query: unknown prefix '{}' in '${{{}}}' (expected 'metadata' or 'payload')",
                        other,
                        inner
                    ))
                }
            };
            sources.push(source);
            out.push_str(&placeholder(sources.len()));
            i = close + 1;
        } else {
            // Copy this UTF-8 char verbatim.
            let ch = query[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok((out, sources))
}

/// Resolve a `ColumnSource` for one message into a typed `BindValue`. `payload_json`
/// is the payload parsed once as JSON (`None` if not valid JSON). No fallback: a
/// `Payload` source never consults metadata and vice versa; an unresolvable source
/// yields `Null`.
fn resolve_source(
    msg: &CanonicalMessage,
    source: &ColumnSource,
    payload_json: &Option<serde_json::Value>,
) -> BindValue {
    match source {
        ColumnSource::Metadata(key) => match msg.metadata.get(key) {
            Some(v) => BindValue::Text(v.clone()),
            None => BindValue::Null,
        },
        ColumnSource::Payload(field) => match payload_json.as_ref().and_then(|v| v.get(field)) {
            Some(serde_json::Value::String(s)) => BindValue::Text(s.clone()),
            Some(serde_json::Value::Bool(b)) => BindValue::Bool(*b),
            Some(serde_json::Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    BindValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    BindValue::Float(f)
                } else {
                    BindValue::Null
                }
            }
            Some(serde_json::Value::Null) => BindValue::Null,
            other => {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let reason = match other {
                        Some(_) => "is an object or array",
                        None => "is missing from the payload",
                    };
                    warn!(
                        "`${{payload:{field}}}` binds NULL: the field {reason}. Tokens read top-level scalar fields only. Further occurrences are not logged."
                    );
                }
                BindValue::Null
            }
        },
    }
}

type AnyQuery<'q> = sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments>;

/// Bind one typed value, mapping `Null` to a typed SQL NULL.
fn bind_value(query: AnyQuery<'_>, value: BindValue) -> AnyQuery<'_> {
    match value {
        BindValue::Null => query.bind(None::<String>),
        BindValue::Int(i) => query.bind(i),
        BindValue::Float(f) => query.bind(f),
        BindValue::Bool(b) => query.bind(b),
        BindValue::Text(s) => query.bind(s),
    }
}

/// Reject a string carrying an embedded NUL. `text`/`varchar` cannot represent one on
/// PostgreSQL or MySQL, and the driver dropped the bind instead of failing — the row
/// landed with a silent SQL `NULL` while the route reported a clean success.
fn reject_embedded_nul(source: &ColumnSource, value: &BindValue) -> Result<(), PublisherError> {
    let BindValue::Text(s) = value else {
        return Ok(());
    };
    if !s.contains('\0') {
        return Ok(());
    }
    let (kind, name) = match source {
        ColumnSource::Metadata(k) => ("metadata", k),
        ColumnSource::Payload(f) => ("payload", f),
    };
    Err(PublisherError::NonRetryable(anyhow!(
        "${{{kind}:{name}}} contains an embedded NUL byte, which a SQL text column cannot store"
    )))
}

/// Parse the payload as JSON once, then resolve+bind every column source for one row.
fn bind_message_sources<'q>(
    mut query: AnyQuery<'q>,
    msg: &CanonicalMessage,
    sources: &[ColumnSource],
) -> Result<AnyQuery<'q>, PublisherError> {
    let payload_json: Option<serde_json::Value> = serde_json::from_slice(&msg.payload).ok();
    for source in sources {
        let value = resolve_source(msg, source, &payload_json);
        reject_embedded_nul(source, &value)?;
        query = bind_value(query, value);
    }
    Ok(query)
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
                    // Explicitly opted out of validation: encrypt but do not verify.
                    query_pairs.append_pair("sslmode", "require");
                } else {
                    // Validation enabled: verify the chain and hostname. `sslrootcert`
                    // (appended below) supplies a custom CA when configured; without one
                    // the system trust store is used.
                    query_pairs.append_pair("sslmode", "verify-full");
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
                if config.tls.accept_invalid_certs {
                    // Explicitly opted out of validation: encrypt but do not verify.
                    query_pairs.append_pair("ssl-mode", "REQUIRED");
                } else if config.tls.ca_file.is_some() {
                    // Verify the chain against the configured CA.
                    query_pairs.append_pair("ssl-mode", "VERIFY_CA");
                } else {
                    // Verify the chain and server identity against the system trust store.
                    query_pairs.append_pair("ssl-mode", "VERIFY_IDENTITY");
                }
                if let Some(ca) = &config.tls.ca_file {
                    query_pairs.append_pair("ssl-ca", ca);
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

    Ok(pool_options.connect(&url).await?)
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
}

/// The raw `lookup_query` a PostgreSQL batch function is generated from.
struct PgBatchLookup {
    template: String,
    /// Set once the query turns out not to run inside a function; it then runs per message.
    disabled: std::sync::atomic::AtomicBool,
    /// `CREATE` and name per token-type combination, built once.
    functions: Mutex<PgFunctions>,
}

/// `CREATE` and name of a batch function, keyed by its token types.
type PgFunctions = std::collections::HashMap<Vec<&'static str>, Arc<(String, String)>>;

/// Metadata key telling whether a `lookup_query` returned a row.
const FOUND_KEY: &str = "sqlx.found";
/// First key of the advisory lock a batch function holds; the second is the query's hash, so
/// only batches of the same `lookup_query` wait for each other instead of deadlocking.
const PG_BATCH_LOCK_CLASS: i32 = i32::from_be_bytes(*b"mqbr");

/// A parsed `lookup_query`: one query per message, or one `IN (…)` query per batch.
enum SqlLookup {
    Single(String, Vec<ColumnSource>),
    List {
        query: ListQuery,
        source: ColumnSource,
    },
}

impl SqlLookup {
    fn parse(sql: &str, driver_name: &str) -> anyhow::Result<Self> {
        let Some(query) = lookup_batch::parse_sql(sql)? else {
            let (sql, sources) = parse_insert_template(sql, driver_name)?;
            return Ok(Self::Single(sql, sources));
        };
        let (_, mut sources) = parse_insert_template(&query.token, driver_name)?;
        let source = sources
            .pop()
            .ok_or_else(|| anyhow!("lookup_query: `IN (…)` holds no ${{…}} token"))?;
        Ok(Self::List { query, source })
    }
}

/// Whether a `lookup_query` modifies data, e.g. an upsert with `RETURNING`.
/// `REPLACE` counts only as a statement (first word, or `REPLACE INTO`), not the string function.
fn lookup_writes(sql: &str) -> bool {
    let words: Vec<&str> = sql
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let is = |word: &str, kw: &str| word.eq_ignore_ascii_case(kw);
    words.iter().any(|word| {
        ["insert", "update", "delete", "merge"]
            .iter()
            .any(|kw| is(word, kw))
    }) || words.first().is_some_and(|w| is(w, "replace"))
        || words
            .windows(2)
            .any(|pair| is(pair[0], "replace") && is(pair[1], "into"))
}

/// A bound key in the form [`lookup_batch::key_of`] gives the matching row's column.
fn bind_key(value: &BindValue) -> Option<String> {
    match value {
        BindValue::Null => None,
        BindValue::Int(i) => Some(i.to_string()),
        BindValue::Float(f) => serde_json::Number::from_f64(*f).map(|n| n.to_string()),
        BindValue::Bool(b) => Some(b.to_string()),
        BindValue::Text(s) => Some(s.clone()),
    }
}

/// A `lookup_query` row as JSON.
fn row_json(row: &sqlx::any::AnyRow) -> Result<serde_json::Value, PublisherError> {
    let mut buf = Vec::new();
    JsonRowSchema::from_row(row).encode_row(row, &mut buf);
    serde_json::from_slice(&buf).map_err(|e| PublisherError::NonRetryable(e.into()))
}

/// A batch's bind values as one `jsonb` array, plus each token's PostgreSQL type (`null`
/// when null throughout). `None` when a token's type varies across the batch, or a string
/// holds a NUL, which `jsonb` cannot store.
fn pg_batch_args(
    messages: &[&CanonicalMessage],
    sources: &[ColumnSource],
) -> Option<(Vec<&'static str>, String)> {
    let mut types: Vec<Option<&'static str>> = vec![None; sources.len()];
    let mut rows = Vec::with_capacity(messages.len());
    for message in messages {
        let payload = serde_json::from_slice(&message.payload).ok();
        let mut row = Vec::with_capacity(sources.len());
        for (source, ty) in sources.iter().zip(types.iter_mut()) {
            let (value, kind) = match resolve_source(message, source, &payload) {
                BindValue::Null => (serde_json::Value::Null, None),
                BindValue::Int(i) => (i.into(), Some("int8")),
                // `jsonb` has no NaN or infinity; `float8` parses them from text.
                BindValue::Float(f) => match serde_json::Number::from_f64(f) {
                    Some(n) => (n.into(), Some("float8")),
                    None => (f.to_string().into(), Some("float8")),
                },
                BindValue::Bool(b) => (b.into(), Some("boolean")),
                BindValue::Text(s) if s.contains('\0') => return None,
                BindValue::Text(s) => (s.into(), Some("text")),
            };
            if let Some(kind) = kind {
                if *ty.get_or_insert(kind) != kind {
                    return None;
                }
            }
            row.push(value);
        }
        rows.push(serde_json::Value::Array(row));
    }
    let types = types.into_iter().map(|t| t.unwrap_or("null")).collect();
    Some((types, serde_json::Value::Array(rows).to_string()))
}

/// `CREATE` for a `pg_temp` function that runs `template` once per element of a `jsonb`
/// batch, in order, returning each element's rows as `(index, json)`; and its name.
fn pg_batch_function(template: &str, types: &[&str]) -> anyhow::Result<(String, String)> {
    // A token null across the batch stays an untyped NULL, as an unbound parameter would.
    let (body, _) = parse_template_with(template, |i| match types[i - 1] {
        "null" => "NULL".to_string(),
        ty => format!("(mqb_args->(mqb_i-1)->>{})::{ty}", i - 1),
    })?;
    let body = body.trim_end().trim_end_matches(';').trim_end();
    let tag = (0..)
        .map(|n| format!("$mqb{n}$"))
        .find(|tag| !body.contains(tag.as_str()))
        .expect("an unused dollar-quote tag");
    let definition = format!(
        "(mqb_args jsonb) RETURNS TABLE(mqb_idx int, mqb_row json) LANGUAGE plpgsql AS {tag} \
         BEGIN PERFORM pg_advisory_xact_lock({PG_BATCH_LOCK_CLASS}, {}); \
         FOR mqb_i IN 1..jsonb_array_length(mqb_args) LOOP \
         RETURN QUERY WITH mqb_q AS ({body}) SELECT mqb_i, to_json(mqb_q) FROM mqb_q; \
         END LOOP; END {tag}",
        stable_hash(template) as i32
    );
    let name = format!("mqb_lookup_{:016x}", stable_hash(&definition));
    Ok((
        format!("CREATE OR REPLACE FUNCTION pg_temp.{name}{definition}"),
        name,
    ))
}

/// FNV-1a: the lock key must match across builds, which `DefaultHasher` does not promise.
fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// SQLSTATE `3F000` (no `pg_temp` schema yet), or `42883` naming the batch function itself: a
/// connection that has not created it. A `42883` from inside the query is the message's error.
fn pg_function_missing(e: &sqlx::Error, name: &str) -> bool {
    e.as_database_error()
        .is_some_and(|d| match d.code().as_deref() {
            Some("3F000") => true,
            Some("42883") => d.message().contains(name),
            _ => false,
        })
}

enum PgBatchError {
    /// The query cannot run inside the generated function.
    Unsupported(anyhow::Error),
    Query(sqlx::Error),
    Decode(PublisherError),
}

/// What a failed batch-function call means for the batch.
enum PgVerdict {
    /// Use the per-key transactions from now on.
    Disable(String),
    /// A message was rejected: redo the batch one message at a time.
    PerMessage,
    Fail(PublisherError),
}

fn pg_verdict(e: PgBatchError) -> PgVerdict {
    match e {
        PgBatchError::Unsupported(e) => PgVerdict::Disable(e.to_string()),
        PgBatchError::Query(e) => {
            let feature = e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "0A000");
            if feature {
                return PgVerdict::Disable(e.to_string());
            }
            match lookup_error(e) {
                PublisherError::NonRetryable(_) => PgVerdict::PerMessage,
                other => PgVerdict::Fail(other),
            }
        }
        PgBatchError::Decode(e) => PgVerdict::Fail(e),
    }
}

fn lookup_error(e: sqlx::Error) -> PublisherError {
    if is_permanent_decode_error(&e) {
        PublisherError::NonRetryable(anyhow!(
            "lookup_query returned a column the SQL `Any` driver cannot decode: {e}. \
             Cast it in the query, e.g. `amount::text AS amount`."
        ))
    } else {
        classify_sql_error(e)
    }
}

/// Bulk-load sink using PostgreSQL `COPY FROM STDIN`. `columns[i]` receives the value
/// resolved from `sources[i]` — positional, mirroring the token-based INSERT.
struct PgCopySink {
    pool: PgPool,
    table: String,
    columns: Vec<String>,
    sources: Vec<ColumnSource>,
}

/// Dedicated native Postgres pool: COPY needs the typed pg protocol, not the `Any` layer.
async fn open_copy_pool(config: &SqlxConfig) -> anyhow::Result<PgPool> {
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

/// Sends `buf` as the data of a started `COPY … FROM STDIN` and ends it.
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
///
/// Everything else (08 connection, 40 deadlock/serialization, 53 resources,
/// 55 object-not-ready, 57 operator-intervention, 58 system) is transient and retried.
fn is_deterministic_sqlstate(code: &str) -> bool {
    matches!(&code.get(..2), Some("42") | Some("22") | Some("23"))
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
        return PublisherError::NonRetryable(sql_error_text(e));
    }
    PublisherError::Retryable(sql_error_text(e))
}

/// Consumer-side twin of [`classify_sql_error`]. A deterministic schema/type/
/// constraint error (missing column, undefined table, bad type) is `Permanent`
/// so the route fails fast instead of reconnecting every `reconnect_interval_ms`
/// forever on an unrecoverable read. Everything else stays `Connection`
/// (retryable) so restarts/failovers recover without losing messages.
fn classify_sql_consumer_error(e: sqlx::Error) -> ConsumerError {
    if sql_error_is_deterministic(&e) {
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

fn extract_copy_columns(raw_query: &str, token_count: usize) -> anyhow::Result<Vec<String>> {
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

    Ok(columns)
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
        let table = config.table.clone();

        // Acquire a connection to determine the driver so we can use the correct SQL syntax.
        let conn = pool.acquire().await?;
        let driver_name = conn.backend_name().to_string();
        drop(conn);

        info!(table = %config.table, driver = %driver_name, "SQLx publisher connected");

        if let Some(template) = &config.lookup_query {
            let writes = lookup_writes(template);
            if writes {
                info!(table = %config.table, "SQLx lookup_query writes on every lookup");
            }
            let lookup = SqlLookup::parse(template, &driver_name)?;
            let pg_batch =
                (writes && driver_name == "PostgreSQL" && matches!(lookup, SqlLookup::Single(..)))
                    .then(|| PgBatchLookup {
                        template: template.clone(),
                        disabled: Default::default(),
                        functions: Default::default(),
                    });
            return Ok(Self {
                pg_batch,
                pool,
                _shared_pool: shared_pool,
                insert_query: String::new(),
                column_sources: Vec::new(),
                lookup: Some(lookup),
                driver_name,
                table,
                copy: None,
                auto: None,
            });
        }

        if config.key.is_some() && config.columns.is_none() {
            return Err(anyhow!(crate::errors::InvalidConfig(anyhow!(
                "`key` needs `columns: auto`; with an `insert_query`, write the ON CONFLICT clause into the query"
            ))));
        }
        if config.extra_column.is_some() && config.columns.is_none() {
            return Err(anyhow!(crate::errors::InvalidConfig(anyhow!(
                "`extra_column` needs `columns: auto`"
            ))));
        }
        if config.columns.is_some() {
            let auto = columns::AutoColumns::new(&pool, &driver_name, config).await?;
            return Ok(Self {
                pg_batch: None,
                pool,
                _shared_pool: shared_pool,
                insert_query: String::new(),
                column_sources: Vec::new(),
                lookup: None,
                driver_name,
                table,
                copy: None,
                auto: Some(auto),
            });
        }

        // Resolve the insert query and parse any `${metadata:...}`/`${payload:...}`
        // tokens into ordered value sources, rewriting them to positional placeholders.
        let raw_insert_query =
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
        let (insert_query, column_sources) =
            parse_insert_template(&raw_insert_query, &driver_name)?;

        if config.auto_create_table && !column_sources.is_empty() {
            return Err(anyhow!(
                "auto_create_table is not supported with a multi-column insert_query; create the table manually."
            ));
        }

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
                if let Err(e) = sqlx::query(audited_sql(&create_table_query))
                    .execute(&pool)
                    .await
                {
                    warn!(
                        "Failed to auto-create table '{}': {}. Please ensure it exists.",
                        config.table, e
                    );
                } else {
                    let table_name_for_index =
                        config.table.split('.').next_back().unwrap_or(&config.table);
                    let index_name = format!("idx_{}_locked_until", table_name_for_index);

                    let create_index_query = match driver_name.as_str() {
                        "PostgreSQL" | "SQLite" | "MariaDB" => {
                            format!(
                                "CREATE INDEX IF NOT EXISTS {} ON {} (locked_until)",
                                index_name, config.table
                            )
                        }
                        "MySQL" => {
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
                        if let Err(e) = sqlx::query(audited_sql(&create_index_query))
                            .execute(&pool)
                            .await
                        {
                            let driver_lc = driver_name.to_lowercase();
                            if (driver_lc.contains("mysql") || driver_lc.contains("mariadb"))
                                && e.as_database_error()
                                    .is_some_and(|db_err| db_err.code().as_deref() == Some("1061"))
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
            let columns = extract_copy_columns(&raw_insert_query, column_sources.len())?;
            Some(PgCopySink {
                pool: open_copy_pool(config).await?,
                table: table.clone(),
                columns,
                sources: column_sources.clone(),
            })
        } else {
            None
        };

        Ok(Self {
            pool,
            _shared_pool: shared_pool,
            insert_query,
            column_sources,
            driver_name,
            table,
            copy,
            lookup: None,
            pg_batch: None,
            auto: None,
        })
    }

    fn pg_batch_active(&self) -> Option<&PgBatchLookup> {
        use std::sync::atomic::Ordering;
        self.pg_batch
            .as_ref()
            .filter(|pg| !pg.disabled.load(Ordering::Relaxed))
    }

    /// Runs `lookup_query` and answers with its first row, or an empty payload.
    async fn lookup_one(&self, message: &CanonicalMessage) -> Result<Sent, PublisherError> {
        let mut payload = Vec::new();
        let found = match (&self.lookup, self.pg_batch_active()) {
            (Some(SqlLookup::Single(sql, sources)), Some(pg)) => {
                let rows = self.pg_batch_rows(pg, sql, sources, &[message]).await?;
                let answer = match rows {
                    Some(mut rows) => rows.pop().flatten(),
                    None => self
                        .lookup_one_by_one(sql, sources, &[message])
                        .await?
                        .pop()
                        .flatten(),
                };
                if let Some(row) = &answer {
                    payload = serde_json::to_vec(row)
                        .map_err(|e| PublisherError::NonRetryable(e.into()))?;
                }
                answer.is_some()
            }
            (Some(SqlLookup::Single(sql, sources)), None) => {
                let query = bind_message_sources(sqlx::query(audited_sql(sql)), message, sources)?;
                let row = query
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(lookup_error)?;
                if let Some(row) = &row {
                    JsonRowSchema::from_row(row).encode_row(row, &mut payload);
                }
                row.is_some()
            }
            (Some(SqlLookup::List { query, source }), _) => {
                let answer = self
                    .lookup_list(query, source, std::slice::from_ref(message))
                    .await?
                    .pop()
                    .flatten();
                if let Some(row) = &answer {
                    payload = serde_json::to_vec(row)
                        .map_err(|e| PublisherError::NonRetryable(e.into()))?;
                }
                answer.is_some()
            }
            (None, _) => unreachable!("lookup_one runs only for a lookup publisher"),
        };
        let mut response = CanonicalMessage::new(payload, Some(message.message_id));
        response
            .metadata
            .insert(FOUND_KEY.to_string(), found.to_string());
        Ok(Sent::Response(response))
    }

    /// Runs a writing lookup batch in order in one transaction that cannot deadlock: PostgreSQL's
    /// batch function holds an advisory lock per query, SQLite takes its write lock up front.
    /// `None` otherwise: one autocommit statement per message.
    async fn lookup_writing_batch(
        &self,
        sql: &str,
        sources: &[ColumnSource],
        messages: &[CanonicalMessage],
    ) -> Option<Result<Vec<Option<serde_json::Value>>, PublisherError>> {
        if let Some(pg) = self.pg_batch_active() {
            let group: Vec<&CanonicalMessage> = messages.iter().collect();
            return self
                .pg_batch_rows(pg, sql, sources, &group)
                .await
                .transpose();
        }
        // A lone message needs no transaction: BEGIN and COMMIT would triple its trips.
        if self.driver_name != "SQLite" || messages.len() < 2 || !lookup_writes(sql) {
            return None;
        }
        let group: Vec<&CanonicalMessage> = messages.iter().collect();
        Some(
            match self.lookup_in_transaction(sql, sources, &group).await {
                Err(PublisherError::NonRetryable(_)) => {
                    match self.pool.begin_with("BEGIN IMMEDIATE").await {
                        Ok(tx) => self.lookup_redo(tx, sql, sources, &group).await,
                        Err(e) => Err(PublisherError::Retryable(anyhow!(e))),
                    }
                }
                other => other,
            },
        )
    }

    /// Redoes a rolled-back batch in `tx`, a savepoint per message: a rejected message is
    /// answered as not found, any other failure rolls every write of the redo back.
    async fn lookup_redo(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Any>,
        sql: &str,
        sources: &[ColumnSource],
        messages: &[&CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        use sqlx::Acquire;
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
        let answers = async {
            let mut answers = Vec::with_capacity(messages.len());
            for message in messages {
                let mut step = tx.begin().await.map_err(retryable)?;
                // Not cached: a statement prepared for one message's bind types fails the next.
                let query = sqlx::query(audited_sql(sql)).persistent(false);
                let row = match bind_message_sources(query, message, sources) {
                    Ok(query) => match query.fetch_optional(&mut *step).await {
                        Err(e) if is_permanent_decode_error(&e) => return Err(lookup_error(e)),
                        other => other.map_err(lookup_error),
                    },
                    Err(e) => Err(e),
                };
                match row.and_then(|row| row.as_ref().map(row_json).transpose()) {
                    Ok(answer) => {
                        step.commit().await.map_err(retryable)?;
                        answers.push(answer);
                    }
                    Err(PublisherError::NonRetryable(e)) => {
                        step.rollback().await.map_err(retryable)?;
                        warn!(error = %e, "SQLx lookup rejected; answering not found");
                        answers.push(None);
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(answers)
        }
        .await;
        match answers {
            Ok(answers) => {
                tx.commit().await.map_err(retryable)?;
                Ok(answers)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    /// Runs the messages in order in one `BEGIN IMMEDIATE` transaction and commits once.
    async fn lookup_in_transaction(
        &self,
        sql: &str,
        sources: &[ColumnSource],
        messages: &[&CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        let mut answers = Vec::with_capacity(messages.len());
        for message in messages {
            let answer =
                match bind_message_sources(sqlx::query(audited_sql(sql)), message, sources) {
                    Ok(query) => query.fetch_optional(&mut *tx).await.map_err(lookup_error),
                    Err(e) => Err(e),
                }
                .and_then(|row| row.as_ref().map(row_json).transpose());
            match answer {
                Ok(answer) => answers.push(answer),
                Err(e) => {
                    // Released before the caller redoes the batch outside it.
                    let _ = tx.rollback().await;
                    return Err(e);
                }
            }
        }
        tx.commit()
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        Ok(answers)
    }

    /// One autocommit query per message; a rejected one is answered as not found. A failure
    /// leaves earlier writes committed, so callers pass a single message.
    async fn lookup_one_by_one(
        &self,
        sql: &str,
        sources: &[ColumnSource],
        messages: &[&CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let mut answers = Vec::with_capacity(messages.len());
        for message in messages {
            let query = bind_message_sources(sqlx::query(audited_sql(sql)), message, sources);
            let row = match query {
                Ok(query) => match query.fetch_optional(&self.pool).await {
                    Err(e) if is_permanent_decode_error(&e) => return Err(lookup_error(e)),
                    other => other.map_err(lookup_error),
                },
                Err(e) => Err(e),
            };
            answers.push(
                match row.and_then(|row| row.as_ref().map(row_json).transpose()) {
                    Ok(answer) => answer,
                    Err(PublisherError::NonRetryable(e)) => {
                        warn!(error = %e, "SQLx lookup rejected; answering not found");
                        None
                    }
                    Err(e) => return Err(e),
                },
            );
        }
        Ok(answers)
    }

    /// One batch-function call; if it rolls back over a rejected message, one call per message.
    async fn pg_batch_rows(
        &self,
        pg: &PgBatchLookup,
        sql: &str,
        sources: &[ColumnSource],
        group: &[&CanonicalMessage],
    ) -> Result<Option<Vec<Option<serde_json::Value>>>, PublisherError> {
        use std::sync::atomic::Ordering;
        let disable = |e: String| {
            pg.disabled.store(true, Ordering::Relaxed);
            warn!(error = %e, "SQLx lookup_query cannot run batched; running one query per message");
        };
        if group.len() > 1 {
            match self.call_pg_batch(pg, sources, group).await {
                Ok(Some(rows)) => return Ok(Some(rows)),
                Ok(None) => {}
                Err(e) => match pg_verdict(e) {
                    PgVerdict::Disable(e) => {
                        disable(e);
                        return Ok(None);
                    }
                    PgVerdict::PerMessage => {}
                    PgVerdict::Fail(e) => return Err(e),
                },
            }
            // One transaction under the batch function's lock, so a failure commits nothing.
            let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
            let mut tx = self.pool.begin().await.map_err(retryable)?;
            let lock = format!(
                "SELECT pg_advisory_xact_lock({PG_BATCH_LOCK_CLASS}, {})",
                stable_hash(&pg.template) as i32
            );
            sqlx::query(audited_sql(&lock))
                .execute(&mut *tx)
                .await
                .map_err(retryable)?;
            return self.lookup_redo(tx, sql, sources, group).await.map(Some);
        }
        let mut answers = Vec::with_capacity(group.len());
        for &message in group {
            let one = std::slice::from_ref(&message);
            let rows = if pg.disabled.load(Ordering::Relaxed) {
                None
            } else {
                match self.call_pg_batch(pg, sources, one).await {
                    Ok(rows) => rows,
                    Err(e) => match pg_verdict(e) {
                        PgVerdict::Fail(e) => return Err(e),
                        PgVerdict::Disable(e) => {
                            disable(e);
                            None
                        }
                        PgVerdict::PerMessage => None,
                    },
                }
            };
            // The plain query decides whether the message itself is at fault, so a failure
            // of the function alone never answers not found without the write.
            let mut rows = match rows {
                Some(rows) => rows,
                None => self.lookup_one_by_one(sql, sources, one).await?,
            };
            answers.push(rows.pop().flatten());
        }
        Ok(Some(answers))
    }

    /// Runs the messages in order through the batch function, on one connection, creating the
    /// function there first if needed. `Ok(None)` when the bind values don't fit one `jsonb`.
    async fn call_pg_batch(
        &self,
        pg: &PgBatchLookup,
        sources: &[ColumnSource],
        group: &[&CanonicalMessage],
    ) -> Result<Option<Vec<Option<serde_json::Value>>>, PgBatchError> {
        let Some((types, args)) = pg_batch_args(group, sources) else {
            return Ok(None);
        };
        let cached = pg
            .functions
            .lock()
            .ok()
            .and_then(|f| f.get(&types).cloned());
        let function = match cached {
            Some(function) => function,
            None => {
                let function = Arc::new(
                    pg_batch_function(&pg.template, &types).map_err(PgBatchError::Unsupported)?,
                );
                if let Ok(mut functions) = pg.functions.lock() {
                    functions.insert(types, function.clone());
                }
                function
            }
        };
        let (create, name) = (&function.0, &function.1);
        let call = format!("SELECT mqb_idx, mqb_row::text FROM pg_temp.{name}($1::jsonb)");
        let mut conn = self.pool.acquire().await.map_err(PgBatchError::Query)?;
        let rows = match sqlx::query(audited_sql(&call))
            .bind(&args)
            .fetch_all(&mut *conn)
            .await
        {
            Err(e) if pg_function_missing(&e, name) => {
                sqlx::raw_sql(AssertSqlSafe(create.as_str()))
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| match e.as_database_error() {
                        Some(_) => PgBatchError::Unsupported(anyhow!(e)),
                        None => PgBatchError::Query(e),
                    })?;
                // Still missing right after the CREATE: a pooler moved the session.
                match sqlx::query(audited_sql(&call))
                    .bind(&args)
                    .fetch_all(&mut *conn)
                    .await
                {
                    Err(e) if pg_function_missing(&e, name) => {
                        return Err(PgBatchError::Unsupported(anyhow!(e)))
                    }
                    rows => rows,
                }
            }
            rows => rows,
        }
        .map_err(PgBatchError::Query)?;
        let mut answers = vec![None; group.len()];
        for row in rows {
            let idx: i32 = row.try_get(0).map_err(PgBatchError::Query)?;
            let json: String = row.try_get(1).map_err(PgBatchError::Query)?;
            let slot = usize::try_from(idx - 1)
                .ok()
                .and_then(|i| answers.get_mut(i))
                .ok_or_else(|| {
                    PgBatchError::Decode(PublisherError::NonRetryable(anyhow!(
                        "batch function returned index {idx} for {} messages",
                        group.len()
                    )))
                })?;
            if slot.is_none() {
                *slot =
                    Some(serde_json::from_str(&json).map_err(|e| {
                        PgBatchError::Decode(PublisherError::NonRetryable(e.into()))
                    })?);
            }
        }
        Ok(Some(answers))
    }

    /// Answers every message with one `IN (…)` query per chunk of distinct keys.
    async fn lookup_list(
        &self,
        query: &ListQuery,
        source: &ColumnSource,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let values: Vec<BindValue> = messages
            .iter()
            .map(|m| resolve_source(m, source, &serde_json::from_slice(&m.payload).ok()))
            .collect();
        // A key with an embedded NUL cannot match a text column: not found, not an error.
        let keys: Vec<Option<String>> = values
            .iter()
            .map(|v| {
                reject_embedded_nul(source, v)
                    .ok()
                    .and_then(|_| bind_key(v))
            })
            .collect();
        let mut records = Vec::new();
        for chunk in lookup_batch::distinct(&keys).chunks(lookup_batch::MAX_KEYS_PER_QUERY) {
            let sql = query.expand(chunk.len(), |i| {
                positional_placeholder(&self.driver_name, i + 1)
            });
            // Not cached: a Postgres generic plan made on a small table stays a seq scan.
            let mut select = sqlx::query(audited_sql(&sql)).persistent(false);
            for &i in chunk {
                select = bind_value(select, values[i].clone());
            }
            let rows = select.fetch_all(&self.pool).await.map_err(lookup_error)?;
            let Some(first) = rows.first() else { continue };
            let schema = JsonRowSchema::from_row(first);
            let mut buf = Vec::new();
            for row in &rows {
                buf.clear();
                schema.encode_row(row, &mut buf);
                records.push(
                    serde_json::from_slice(&buf)
                        .map_err(|e| PublisherError::NonRetryable(e.into()))?,
                );
            }
        }
        lookup_batch::answer(&keys, records, &query.key).map_err(PublisherError::NonRetryable)
    }
}

#[async_trait]
impl MessagePublisher for SqlxPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if self.lookup.is_some() {
            return self.lookup_one(&message).await;
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
            bind_message_sources(query, &message, &self.column_sources)?
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

        if let Some(auto) = &self.auto {
            return auto.send(&self.pool, messages).await;
        }

        if let Some(sink) = &self.copy {
            return self.send_batch_copy(sink, messages).await;
        }

        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch to SQLx");

        // Manually construct the query with appropriate placeholders because
        // sqlx::QueryBuilder with the `Any` driver does not correctly rewrite `?` to `$N`.
        let values_pos = match find_top_level_values(&self.insert_query) {
            Some(pos) => pos,
            None => {
                warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
                return self.send_batch_iterative(messages).await;
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

        // The rebuild below emits a tuple of bare placeholders, so anything else the user
        // wrote inside the tuple — a cast, `decode(…, 'base64')` for a bytea column, a
        // function call, a literal — would be dropped and the value bound raw against the
        // target column's type. Single-row `send()` keeps the tuple verbatim.
        if !tuple_is_bare_placeholders(values_tuple, &self.driver_name) {
            warn!("insert_query's VALUES tuple contains an expression, not just tokens. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages).await;
        }

        // A token after the VALUES tuple (e.g. `ON CONFLICT … DO UPDATE SET v = ${payload:v}`)
        // was already rewritten to a placeholder by `parse_insert_template`, numbered for the
        // single-row query. The rebuild below renumbers only the row tuples, so the suffix
        // would keep a stale index and the bind count would not line up. Single-row `send()`
        // handles these correctly.
        if binds_after_values(values_suffix) {
            warn!("insert_query binds a value after the VALUES tuple. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages).await;
        }

        // The `(payload)` single-column guard only applies to legacy mode; a
        // token-based query is already known-correct from `parse_insert_template`.
        if self.column_sources.is_empty() && !contains_payload_clause(base_query) {
            warn!("Could not optimize batch insert due to custom query format. Falling back to iterative inserts.");
            return self.send_batch_iterative(messages).await;
        }

        // Placeholders per row: N tokens in token mode, 1 (the payload) in legacy mode.
        let per_row = self.column_sources.len().max(1);
        let per_statement = (columns::MAX_BINDS / per_row).max(1);
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));
        // One statement is atomic by itself; a batch split at the bind limit needs a transaction.
        let mut tx = match messages.len() > per_statement {
            true => Some(self.pool.begin().await.map_err(retryable)?),
            false => None,
        };
        for chunk in messages.chunks(per_statement) {
            // A running 1-based index spans the whole statement.
            let mut placeholders = String::new();
            let mut param_idx = 1;
            for i in 0..chunk.len() {
                if i > 0 {
                    placeholders.push_str(", ");
                }
                placeholders.push('(');
                for j in 0..per_row {
                    if j > 0 {
                        placeholders.push_str(", ");
                    }
                    placeholders.push_str(&positional_placeholder(&self.driver_name, param_idx));
                    param_idx += 1;
                }
                placeholders.push(')');
            }

            let sql = format!("{} VALUES {}{}", base_query, placeholders, values_suffix);

            let mut query = sqlx::query(audited_sql(&sql));
            for msg in chunk {
                if self.column_sources.is_empty() {
                    query = query.bind(msg.payload.to_vec());
                } else {
                    query = bind_message_sources(query, msg, &self.column_sources)?;
                }
            }
            match &mut tx {
                Some(tx) => query.execute(&mut **tx).await,
                None => query.execute(&self.pool).await,
            }
            .map_err(classify_sql_error)?;
        }
        if let Some(tx) = tx {
            tx.commit().await.map_err(retryable)?;
        }
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SqlxPublisher {
    /// Bulk-load a batch via PostgreSQL `COPY FROM STDIN` (text format). Each row is a
    /// tab-separated line of the resolved token values, `\N` for NULL. Far faster than a
    /// multi-row INSERT for large batches; retryable on transport errors (at-least-once).
    async fn send_batch_copy(
        &self,
        sink: &PgCopySink,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let stmt = format!(
            "COPY {} ({}) FROM STDIN WITH (FORMAT text)",
            sink.table,
            sink.columns.join(", ")
        );

        let mut buf = String::new();
        for msg in &messages {
            let payload_json: Option<serde_json::Value> = serde_json::from_slice(&msg.payload).ok();
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

        let copier = sink.pool.copy_in_raw(&stmt).await;
        copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await?;

        trace!(count = messages.len(), table = %sink.table, "Bulk-copied batch to PostgreSQL");
        Ok(SentBatch::Ack)
    }

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
        for msg in &messages {
            let query = sqlx::query(audited_sql(&self.insert_query));
            let query = if self.column_sources.is_empty() {
                query.bind(msg.payload.to_vec())
            } else {
                bind_message_sources(query, msg, &self.column_sources)?
            };
            query.execute(&mut *tx).await.map_err(classify_sql_error)?;
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
    backoff: PollBackoff,
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
            .into_iter()
            .map(|row| row.get("id"))
            .collect();

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
                for (i, disp) in dispositions.iter().enumerate() {
                    let should_ack = match disp {
                        MessageDisposition::Ack => true,
                        MessageDisposition::Reply(_) => {
                            tracing::warn!("SQLx consumer received a Reply/StreamReply, but replying is not supported by this endpoint. The reply payload is dropped, and the original message is acknowledged.");
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

// --- Non-destructive `cursor_column` reader (arbitrary tables) ---

/// A cursor value, tracked per column and persisted as a tagged string.
#[derive(Clone, Debug, PartialEq)]
enum SqlCursor {
    Int(i64),
    Text(String),
}

impl SqlCursor {
    fn encode(&self) -> String {
        match self {
            SqlCursor::Int(n) => format!("int:{}", n),
            SqlCursor::Text(s) => format!("str:{}", s),
        }
    }

    fn decode(s: &str) -> Option<SqlCursor> {
        let (tag, val) = s.split_once(':')?;
        match tag {
            "int" => val.parse::<i64>().ok().map(SqlCursor::Int),
            "str" => Some(SqlCursor::Text(val.to_string())),
            _ => None,
        }
    }
}

/// Serialize a full row into a JSON object payload (`{column: value, ...}`), trying the
/// value types the `Any` driver supports. Unknown/unsupported types bind to JSON null.
/// One column's pre-rendered JSON key. Every row of a batch shares the query's column
/// list, so resolving this per row only re-allocated the same key strings. The value
/// *kind* is deliberately not cached — see [`JsonRowSchema::encode_row`].
struct JsonColumn {
    /// `"name":` — quoted, escaped and colon-terminated, ready to memcpy into a payload.
    key: Vec<u8>,
    ordinal: usize,
    /// The text value is already JSON and is embedded as it is.
    raw_json: bool,
}

/// The column layout of a batch, resolved once from its first row.
struct JsonRowSchema {
    columns: Vec<JsonColumn>,
}

impl JsonRowSchema {
    fn from_row(row: &sqlx::any::AnyRow) -> Self {
        let columns = row
            .columns()
            .iter()
            .map(|col| {
                let mut key = Vec::new();
                // Serialize the name so column names containing `"` or `\` stay valid JSON.
                let _ = serde_json::to_writer(&mut key, col.name());
                key.push(b':');
                JsonColumn {
                    key,
                    ordinal: col.ordinal(),
                    raw_json: false,
                }
            })
            .collect();
        Self { columns }
    }

    fn mark_raw_json(&mut self, row: &sqlx::any::AnyRow, names: &[String]) {
        if names.is_empty() {
            return;
        }
        for (col, meta) in self.columns.iter_mut().zip(row.columns()) {
            col.raw_json = names.iter().any(|n| n == meta.name());
        }
    }

    /// Write one row as a JSON object straight into `buf`. Going through
    /// `serde_json::Map`/`Value` first allocated a `String` per key per row and copied every
    /// text value an extra time before the final serialization pass.
    ///
    /// The value kind is read from the row being encoded, not cached with the column:
    /// SQLite types a *value*, not a column, so `Any` can report a different kind per row.
    /// Caching row 1's kind decoded later rows as the wrong type — and a NULL first row
    /// pinned the whole column to `null`.
    fn encode_row(&self, row: &sqlx::any::AnyRow, buf: &mut Vec<u8>) {
        buf.push(b'{');
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            buf.extend_from_slice(&col.key);
            if col.raw_json {
                match row.try_get::<Option<&str>, _>(col.ordinal) {
                    Ok(Some(json)) if !json.is_empty() => buf.extend_from_slice(json.as_bytes()),
                    _ => buf.extend_from_slice(b"null"),
                }
                continue;
            }
            let kind = value_kind(row, col.ordinal).unwrap_or(sqlx::any::AnyTypeInfoKind::Null);
            write_json_value(row, col.ordinal, kind, buf);
        }
        buf.push(b'}');
    }
}

/// The type of the *value* at `idx`, not of the column. `AnyRow` builds its columns from the
/// statement's column metadata (one fixed kind for the whole result set), but stores each
/// decoded value's own kind — which is what SQLite actually types. Reading the kind off the
/// column pinned every row to the declared/inferred type, so a text or blob value in an
/// `INTEGER`-typed column (or any value in a column SQLite typed from a NULL) decoded wrong.
fn value_kind(row: &sqlx::any::AnyRow, idx: usize) -> Option<sqlx::any::AnyTypeInfoKind> {
    use sqlx::ValueRef;
    Some(row.try_get_raw(idx).ok()?.type_info().kind())
}

/// Append a JSON-encoded scalar, or `null` when the value is absent or fails to decode.
/// Only non-finite floats can fail mid-write, and serde_json emits `null` for those before
/// writing anything, so a partial value can never land in `buf`.
fn write_opt<T: serde::Serialize>(buf: &mut Vec<u8>, value: Result<Option<T>, sqlx::Error>) {
    match value {
        Ok(Some(v)) if serde_json::to_writer(&mut *buf, &v).is_ok() => {}
        _ => buf.extend_from_slice(b"null"),
    }
}

/// Decode one column's value straight to its known `AnyTypeInfoKind`, instead of guessing
/// via a cascade of `try_get::<T>` calls (each of which round-trips through `Any`'s dynamic
/// dispatch and fails before the right type is found). This is the hot path for the cursor
/// reader, so avoiding N wasted decode attempts per column matters at scale.
///
/// Text and blobs are borrowed (`&str`/`&[u8]`), not owned: `Any` already materialized them
/// into an `Arc<String>`/`Arc<Vec<u8>>` when it built the row, and decoding to `String`/
/// `Vec<u8>` copies that a second time for no reason.
fn write_json_value(
    row: &sqlx::any::AnyRow,
    idx: usize,
    kind: sqlx::any::AnyTypeInfoKind,
    buf: &mut Vec<u8>,
) {
    use sqlx::any::AnyTypeInfoKind;
    match kind {
        AnyTypeInfoKind::Null => buf.extend_from_slice(b"null"),
        AnyTypeInfoKind::Bool => write_opt(buf, row.try_get::<Option<bool>, _>(idx)),
        AnyTypeInfoKind::SmallInt | AnyTypeInfoKind::Integer | AnyTypeInfoKind::BigInt => {
            write_opt(buf, row.try_get::<Option<i64>, _>(idx))
        }
        // Widened to f64 so the rendered decimal matches the old `Value::from(v as f64)`.
        AnyTypeInfoKind::Real => write_opt(
            buf,
            row.try_get::<Option<f32>, _>(idx).map(|v| v.map(f64::from)),
        ),
        AnyTypeInfoKind::Double => write_opt(buf, row.try_get::<Option<f64>, _>(idx)),
        AnyTypeInfoKind::Text => write_opt(buf, row.try_get::<Option<&str>, _>(idx)),
        // Bytes have no JSON scalar; expose as a base16 string so the copy is lossless-ish.
        // Hex is ASCII-only, so it needs no escaping and can be written nibble by nibble.
        AnyTypeInfoKind::Blob => match row.try_get::<Option<&[u8]>, _>(idx) {
            Ok(Some(b)) => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                buf.reserve(b.len() * 2 + 2);
                buf.push(b'"');
                for x in b {
                    buf.push(HEX[(x >> 4) as usize]);
                    buf.push(HEX[(x & 0x0f) as usize]);
                }
                buf.push(b'"');
            }
            _ => buf.extend_from_slice(b"null"),
        },
    }
}

/// Resolve the cursor column's ordinal once per batch (same query, same column order for
/// every row) instead of re-resolving the name -> ordinal lookup on every row. The kind is
/// not resolved here — it is per value on SQLite, so [`extract_cursor_at`] reads it per row.
fn resolve_cursor_column(row: &sqlx::any::AnyRow, column: &str) -> Option<usize> {
    Some(row.try_column(column).ok()?.ordinal())
}

fn extract_cursor_at(row: &sqlx::any::AnyRow, idx: usize) -> Option<SqlCursor> {
    use sqlx::any::AnyTypeInfoKind;
    match value_kind(row, idx)? {
        AnyTypeInfoKind::SmallInt | AnyTypeInfoKind::Integer | AnyTypeInfoKind::BigInt => row
            .try_get::<Option<i64>, _>(idx)
            .ok()
            .flatten()
            .map(SqlCursor::Int),
        AnyTypeInfoKind::Text => row
            .try_get::<Option<String>, _>(idx)
            .ok()
            .flatten()
            .map(SqlCursor::Text),
        _ => None,
    }
}

/// PostgreSQL base type names (`pg_type.typname`) that the sqlx `Any` driver can map to an
/// `AnyValue`. This mirrors sqlx-postgres' `PgTypeInfo -> AnyTypeInfo` conversion exactly;
/// anything not listed (timestamptz, uuid, numeric, json/jsonb, arrays, date/time, interval,
/// inet, and notably `name`/`bpchar`/`char`) makes `Any` abort row decoding, so we cast it to
/// `text`. Keep this in lockstep with sqlx: over-including a type reintroduces the hang.
fn pg_typname_is_any_safe(typname: &str) -> bool {
    matches!(
        typname,
        "bool"
            | "int2"
            | "int4"
            | "int8"
            | "float4"
            | "float8"
            | "bytea"
            | "text"
            | "varchar"
            | "citext"
    )
}

/// Double-quote a SQL identifier, escaping embedded quotes.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a SQL identifier for `driver_name`. MySQL/MariaDB use backticks; everyone else uses
/// the SQL-standard double quote. Required for the checkpoint meta table: `last_value` is a
/// reserved word in MySQL 8 (the `LAST_VALUE` window function), so unquoted DDL is a syntax
/// error there — and a user-supplied table name may need quoting on any driver.
fn quote_ident_for(driver_name: &str, name: &str) -> String {
    match driver_name {
        "MySQL" | "MariaDB" => format!("`{}`", name.replace('`', "``")),
        _ => quote_ident(name),
    }
}

/// MySQL/MariaDB `information_schema.columns.DATA_TYPE` values that the sqlx `Any` driver can
/// map to an `AnyValue`. Mirrors sqlx-mysql's `MySqlTypeInfo -> AnyTypeInfo` conversion: the
/// integer/float column types it names explicitly, plus everything `str`/`[u8]` declare
/// compatible. Everything else — `decimal`, the whole date/time family, `json`, `bit`, `set`,
/// `tinyint`, `mediumint`, and MariaDB's `uuid`/`inet6` — aborts row decoding, so it is cast.
/// Unlisted types default to unsafe, which is the harmless direction (a needless cast).
fn mysql_data_type_is_any_safe(data_type: &str) -> bool {
    matches!(
        data_type,
        "smallint"
            | "int"
            | "integer"
            | "bigint"
            | "float"
            | "double"
            | "char"
            | "varchar"
            | "binary"
            | "varbinary"
            | "enum"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
    )
}

/// SQLite declared column types that sqlx maps to a `DataType` the `Any` driver rejects.
/// sqlx-sqlite parses `sqlite3_column_decltype` with `DataType::from_str`, and only these exact
/// spellings yield `Bool`/`Date`/`Time`/`Datetime` — everything else either maps to a supported
/// type or fails to parse and falls back to the runtime value type, which is always safe.
/// Notably this covers the `DATETIME` columns our own `auto_create_table` DDL emits.
fn sqlite_decltype_is_any_safe(decl_type: &str) -> bool {
    !matches!(
        decl_type,
        "boolean" | "bool" | "date" | "time" | "datetime" | "timestamp"
    )
}

/// Binds are the table name and its schema (`main` unless the name was `schema.table`).
const SQLITE_COLUMN_TYPES_SQL: &str =
    "SELECT name AS name, LOWER(type) AS typname FROM pragma_table_info(?, ?) ORDER BY cid";

/// `$1::regclass` resolves the (optionally schema-qualified) table name against the current
/// search_path. `pg_attribute.attname`/`pg_type.typname` are Postgres `name`-typed columns,
/// which `Any` cannot decode directly (distinct from `text`), so cast them explicitly.
const PG_COLUMN_TYPES_SQL: &str = "SELECT a.attname::text AS name, t.typname::text AS typname \
     FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
     WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

/// The bind is the schema part of a `db.table` name, empty for an unqualified one — in which
/// case the connection's current database is used.
const MYSQL_COLUMN_TYPES_SQL: &str = "SELECT COLUMN_NAME AS name, LOWER(DATA_TYPE) AS typname \
     FROM information_schema.columns \
     WHERE table_schema = COALESCE(NULLIF(?, ''), DATABASE()) AND table_name = ? \
     ORDER BY ORDINAL_POSITION";

/// Build the SELECT projection used in place of `*` for the cursor reader.
///
/// The `Any` driver eagerly decodes every column when building a row and aborts the whole
/// query on the first type it cannot map (`TIMESTAMPTZ` on Postgres, `DECIMAL`/`TIMESTAMP` on
/// MySQL). We introspect the table's columns and cast every `Any`-incompatible column to a
/// string type, so the copy succeeds (unmappable values arrive as strings) instead of failing
/// every read forever. SQLite is affected too: its type info comes from the *declared* type, so
/// a `DATETIME` column — including the ones `auto_create_table` writes — breaks the read.
/// Any introspection failure (or an unsupported driver) falls back to `*`, preserving the
/// previous behaviour.
async fn build_cursor_projection(
    pool: &AnyPool,
    driver_name: &str,
    table: &str,
    timestamps: crate::models::SqlTimestamps,
) -> (String, Vec<String>) {
    let star = || ("*".to_string(), Vec::new());
    type SafeFn = fn(&str) -> bool;
    type CastFn = fn(&str) -> String;

    let (sql, binds, is_safe, cast): (&str, Vec<String>, SafeFn, CastFn) = match driver_name {
        "PostgreSQL" => (
            PG_COLUMN_TYPES_SQL,
            vec![table.to_string()],
            pg_typname_is_any_safe,
            |ident| format!("{ident}::text AS {ident}"),
        ),
        "MySQL" | "MariaDB" => {
            let (schema, name) = match table.split_once('.') {
                Some((s, t)) => (
                    s.trim_matches('`').to_string(),
                    t.trim_matches('`').to_string(),
                ),
                None => (String::new(), table.trim_matches('`').to_string()),
            };
            (
                MYSQL_COLUMN_TYPES_SQL,
                vec![schema, name],
                mysql_data_type_is_any_safe,
                |ident| format!("CAST({ident} AS CHAR) AS {ident}"),
            )
        }
        "SQLite" => {
            let (schema, name) = match table.split_once('.') {
                Some((s, t)) => (
                    s.trim_matches('"').to_string(),
                    t.trim_matches('"').to_string(),
                ),
                None => ("main".to_string(), table.trim_matches('"').to_string()),
            };
            (
                SQLITE_COLUMN_TYPES_SQL,
                vec![name, schema],
                sqlite_decltype_is_any_safe,
                |ident| format!("CAST({ident} AS TEXT) AS {ident}"),
            )
        }
        _ => return star(),
    };

    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(bind);
    }
    let rows = match query.fetch_all(pool).await {
        Ok(rows) if !rows.is_empty() => rows,
        Ok(_) => return star(),
        Err(e) => {
            warn!(table = %table, error = %e, "Could not introspect {driver_name} columns; falling back to SELECT * (timestamp/decimal/uuid/etc. columns may fail to decode)");
            return star();
        }
    };
    let mut parts = Vec::with_capacity(rows.len());
    let mut raw_json = Vec::new();
    for row in &rows {
        let name: String = match row.try_get("name") {
            Ok(n) => n,
            Err(_) => return star(),
        };
        let typname: String = row.try_get("typname").unwrap_or_default();
        let ident = quote_ident_for(driver_name, &name);
        if is_safe(&typname) {
            parts.push(ident);
        } else if driver_name == "PostgreSQL" && matches!(typname.as_str(), "json" | "jsonb") {
            parts.push(cast(&ident));
            raw_json.push(name);
        } else if let Some(rendered) = pg_rfc3339(driver_name, &typname, &ident, timestamps) {
            parts.push(rendered);
        } else if driver_name == "PostgreSQL" && typname.starts_with('_') {
            // Array types: the text form is `{1,2}`, so render them as a JSON array.
            parts.push(format!("to_jsonb({ident})::text AS {ident}"));
            raw_json.push(name);
        } else {
            // Cast to a string type so `Any` can decode it; keep the column name via alias.
            parts.push(cast(&ident));
        }
    }
    (parts.join(", "), raw_json)
}

/// The projection for a Postgres timestamp column under `timestamps: rfc3339`.
fn pg_rfc3339(
    driver_name: &str,
    typname: &str,
    ident: &str,
    timestamps: crate::models::SqlTimestamps,
) -> Option<String> {
    if driver_name != "PostgreSQL" || timestamps != crate::models::SqlTimestamps::Rfc3339 {
        return None;
    }
    match typname {
        "timestamptz" => Some(format!(
            "to_char({ident} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS {ident}"
        )),
        "timestamp" => Some(format!(
            "to_char({ident}, 'YYYY-MM-DD\"T\"HH24:MI:SS.US') AS {ident}"
        )),
        _ => None,
    }
}

/// A permanent (non-transient) failure: the `Any` driver cannot decode a column type, so
/// retrying the identical query will fail identically. Detected so the route fails fast with
/// a clear message instead of reconnecting on a 5s loop forever.
fn is_permanent_decode_error(e: &sqlx::Error) -> bool {
    if matches!(e, sqlx::Error::ColumnDecode { .. }) {
        return true;
    }
    let msg = e.to_string();
    msg.contains("Any driver does not support") || msg.contains("Any driver mapping")
}

/// Checkpoint store backed by a `mqb_cursors` table in the source database.
struct SqlTableCheckpointStore {
    pool: AnyPool,
    driver_name: String,
    meta_table: String,
    cursor_id: String,
}

impl SqlTableCheckpointStore {
    /// The meta table and its columns, quoted for this driver.
    fn idents(&self) -> (String, String, String) {
        let q = |n: &str| quote_ident_for(&self.driver_name, n);
        (q(&self.meta_table), q("cursor_id"), q("last_value"))
    }

    async fn ensure_table(&self) -> anyhow::Result<()> {
        let (table, cursor_id, last_value) = self.idents();
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {table} ({cursor_id} VARCHAR(255) PRIMARY KEY, {last_value} TEXT)"
        );
        sqlx::query(audited_sql(&sql))
            .execute(&self.pool)
            .await
            // Inline the driver error rather than layering it as an anyhow source:
            // the route logs only the top-level message, which hid e.g. MySQL's
            // ERROR 1064 and made this near-undiagnosable.
            .map_err(|e| {
                anyhow::anyhow!("Failed to create meta table '{}': {e}", self.meta_table)
            })?;
        Ok(())
    }
}

#[async_trait]
impl crate::checkpoint::CheckpointStore for SqlTableCheckpointStore {
    async fn load(&self) -> anyhow::Result<Option<String>> {
        let (table, cursor_id, last_value) = self.idents();
        let sql = format!(
            "SELECT {last_value} FROM {table} WHERE {cursor_id} = {}",
            positional_placeholder(&self.driver_name, 1)
        );
        let row = sqlx::query(audited_sql(&sql))
            .bind(self.cursor_id.clone())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("last_value").ok().flatten()))
    }

    async fn save(&self, value: &str) -> anyhow::Result<()> {
        let (table, cursor_id, last_value) = self.idents();
        let p1 = positional_placeholder(&self.driver_name, 1);
        let p2 = positional_placeholder(&self.driver_name, 2);
        let sql = match self.driver_name.as_str() {
            "MySQL" | "MariaDB" => format!(
                "INSERT INTO {table} ({cursor_id}, {last_value}) VALUES ({p1}, {p2}) \
                 ON DUPLICATE KEY UPDATE {last_value} = VALUES({last_value})"
            ),
            _ => format!(
                "INSERT INTO {table} ({cursor_id}, {last_value}) VALUES ({p1}, {p2}) \
                 ON CONFLICT ({cursor_id}) DO UPDATE SET {last_value} = excluded.{last_value}"
            ),
        };
        sqlx::query(audited_sql(&sql))
            .bind(self.cursor_id.clone())
            .bind(value.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to persist cursor to '{}': {e}", self.meta_table)
            })?;
        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        let (table, cursor_id, _) = self.idents();
        let sql = format!(
            "DELETE FROM {table} WHERE {cursor_id} = {}",
            positional_placeholder(&self.driver_name, 1)
        );
        sqlx::query(audited_sql(&sql))
            .bind(self.cursor_id.clone())
            .execute(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to clear cursor in '{}': {e}", self.meta_table))?;
        Ok(())
    }
}

/// Build a checkpoint store on an **external** SQL database (its own pool), creating the meta
/// table if needed. Used when `checkpoint_store` is a `postgres|mysql|sqlite://…` URL.
pub(crate) async fn build_sql_checkpoint_store(
    url: &str,
    table: Option<String>,
    source_name: &str,
    cursor_id: &str,
) -> anyhow::Result<Arc<dyn crate::checkpoint::CheckpointStore>> {
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(url)
        .await
        .with_context(|| format!("Failed to connect checkpoint store at '{}'", url))?;
    let driver_name = {
        let conn = pool.acquire().await?;
        let name = conn.backend_name().to_string();
        drop(conn);
        name
    };
    let meta_table = table.unwrap_or_else(|| crate::checkpoint::default_meta_name(source_name));
    source_sql_checkpoint_store(pool, driver_name, meta_table, source_name, cursor_id).await
}

/// Build a checkpoint store on an already-connected pool (typically the source's own datastore),
/// creating the meta table if needed.
async fn source_sql_checkpoint_store(
    pool: AnyPool,
    driver_name: String,
    meta_table: String,
    source_name: &str,
    cursor_id: &str,
) -> anyhow::Result<Arc<dyn crate::checkpoint::CheckpointStore>> {
    if !is_valid_table_name(&meta_table) {
        return Err(anyhow!("Invalid checkpoint table name: '{}'.", meta_table));
    }
    let store = SqlTableCheckpointStore {
        pool,
        driver_name,
        meta_table,
        cursor_id: crate::checkpoint::checkpoint_key(source_name, cursor_id),
    };
    store.ensure_table().await?;
    Ok(Arc::new(store))
}

/// Opens a cursor reader's checkpoint, identified by table and cursor column. `source` is the
/// reader's own pool and driver name; without it one is connected only if the store needs it.
async fn open_cursor_checkpoint(
    config: &SqlxConfig,
    cursor_id: &str,
    cursor_column: &str,
    source: Option<(AnyPool, String)>,
) -> anyhow::Result<Arc<crate::checkpoint::VersionedCheckpoint>> {
    use crate::checkpoint::CheckpointBackend;
    let backend = match &config.checkpoint_store {
        // Absent: source datastore with an auto-unique meta table.
        None => CheckpointBackend::Source {
            name: crate::checkpoint::default_meta_name(&config.table),
        },
        Some(spec) => crate::checkpoint::parse_checkpoint_store(spec)?,
    };
    let store = match backend {
        CheckpointBackend::Source { name } => {
            let (pool, driver_name) = match source {
                Some(source) => source,
                None => {
                    let pool = create_sqlx_pool(config).await?;
                    let driver_name = pool.acquire().await?.backend_name().to_string();
                    (pool, driver_name)
                }
            };
            source_sql_checkpoint_store(pool, driver_name, name, &config.table, cursor_id).await?
        }
        external => {
            crate::checkpoint::build_external_store(external, &config.table, cursor_id).await?
        }
    };
    Ok(Arc::new(crate::checkpoint::VersionedCheckpoint::new(
        store,
        format!("sqlx:{}:{cursor_column}", config.table),
    )))
}

/// The checkpoint of a `cursor_column` source, or `None` without a `cursor_id`.
pub(crate) async fn cursor_checkpoint(
    config: &SqlxConfig,
) -> anyhow::Result<Option<Arc<crate::checkpoint::VersionedCheckpoint>>> {
    let (Some(cursor_id), Some(cursor_column)) = (&config.cursor_id, &config.cursor_column) else {
        return Ok(None);
    };
    sqlx::any::install_default_drivers();
    open_cursor_checkpoint(config, cursor_id, cursor_column, None)
        .await
        .map(Some)
}

/// A non-destructive, resumable reader over an **arbitrary** SQL table. Pages by a
/// monotonic `cursor_column` (`SELECT * ... WHERE col > $last ORDER BY col ASC LIMIT n`),
/// never deletes/locks source rows, and persists the last successfully-sunk value (keyed
/// by `cursor_id`) to a pluggable checkpoint store (a `mqb_cursors` table by default, or a
/// local file). At-least-once. Supported drivers: PostgreSQL, MySQL/MariaDB, SQLite.
pub struct SqlxCursorReader {
    pool: AnyPool,
    table: String,
    cursor_column: String,
    driver_name: String,
    backoff: PollBackoff,
    checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>>,
    last_value: Arc<Mutex<Option<SqlCursor>>>,
    run_position: Option<(crate::route::RunPositions, String)>,
    /// Columns the projection renders as JSON text (Postgres `json`/`jsonb` and arrays).
    raw_json_columns: Vec<String>,
    /// Page queries, built once: only the bound cursor and limit vary between polls.
    sql_first: String,
    sql_next: String,
    source_metadata: bool,
}

/// Run one keyset page. `from` is the exclusive lower bound (`None` = start of table).
async fn fetch_page(
    pool: &AnyPool,
    sql: &str,
    from: Option<&SqlCursor>,
    limit: i64,
) -> Result<Vec<sqlx::any::AnyRow>, sqlx::Error> {
    let mut query = sqlx::query(audited_sql(sql));
    if let Some(c) = from {
        query = match c {
            SqlCursor::Int(n) => query.bind(*n),
            SqlCursor::Text(s) => query.bind(s.clone()),
        };
    }
    query.bind(limit).fetch_all(pool).await
}

impl SqlxCursorReader {
    pub async fn new(config: &SqlxConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, config.source_metadata).await
    }

    pub async fn new_with_source_metadata(
        config: &SqlxConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        Self::new_with_source_metadata_and_no_resume(config, source_metadata, false).await
    }

    pub(crate) async fn new_with_source_metadata_and_no_resume(
        config: &SqlxConfig,
        source_metadata: bool,
        no_resume: bool,
    ) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        if config.delete_after_read {
            return Err(anyhow!(
                "SQLx `cursor_column` (non-destructive) and `delete_after_read` are mutually exclusive"
            ));
        }
        if !is_valid_table_name(&config.table) {
            return Err(anyhow!("Invalid table name: '{}'.", config.table));
        }
        let cursor_column = config
            .cursor_column
            .clone()
            .ok_or_else(|| anyhow!("cursor_column is required for the SQLx cursor reader"))?;
        if !is_valid_table_name(&cursor_column) {
            return Err(anyhow!("Invalid cursor_column name: '{}'.", cursor_column));
        }

        let pool = create_sqlx_pool(config).await?;
        let conn = pool.acquire().await?;
        let driver_name = conn.backend_name().to_string();
        drop(conn);

        if driver_name == "Microsoft SQL Server" {
            return Err(anyhow!(
                "cursor_column mode is not supported for Microsoft SQL Server"
            ));
        }
        info!(table = %config.table, column = %cursor_column, driver = %driver_name, "SQLx cursor reader connected");

        let checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>> = if no_resume {
            None
        } else if let Some(cid) = &config.cursor_id {
            Some(
                open_cursor_checkpoint(
                    config,
                    cid,
                    &cursor_column,
                    Some((pool.clone(), driver_name.clone())),
                )
                .await?,
            )
        } else {
            warn!(
                table = %config.table,
                "SQLx cursor reader has no cursor_id; resume is disabled and every restart re-copies from the beginning. Set cursor_id to persist progress."
            );
            None
        };

        // Without a durable checkpoint the position still survives a reconnect of this run.
        let run_position = match &checkpoint {
            Some(_) => None,
            None => crate::route::RUN_POSITIONS
                .try_with(Arc::clone)
                .ok()
                .map(|positions| {
                    let key = format!("sqlx\n{}\n{}\n{}", config.url, config.table, cursor_column);
                    (positions, key)
                }),
        };
        let last_value = match &checkpoint {
            Some(cp) => cp.load().await?.and_then(|s| {
                let decoded = SqlCursor::decode(&s);
                if decoded.is_none() {
                    warn!(value = %s, "Ignoring unparseable sql cursor; starting from beginning");
                }
                decoded
            }),
            None => run_position.as_ref().and_then(|(positions, key)| {
                let saved = positions.lock().unwrap().get(key).cloned();
                saved.and_then(|s| SqlCursor::decode(&s))
            }),
        };
        info!(table = %config.table, cursor_id = ?config.cursor_id, has_checkpoint = %last_value.is_some(), "SQLx cursor reader initialized");

        let (projection, raw_json_columns) =
            build_cursor_projection(&pool, &driver_name, &config.table, config.timestamps).await;

        let sql_first = format!(
            "SELECT {0} FROM {1} ORDER BY {2} ASC LIMIT {3}",
            projection,
            config.table,
            cursor_column,
            positional_placeholder(&driver_name, 1),
        );
        let sql_next = format!(
            "SELECT {0} FROM {1} WHERE {2} > {3} ORDER BY {2} ASC LIMIT {4}",
            projection,
            config.table,
            cursor_column,
            positional_placeholder(&driver_name, 1),
            positional_placeholder(&driver_name, 2),
        );

        Ok(Self {
            pool,
            table: config.table.clone(),
            cursor_column,
            driver_name,
            sql_first,
            sql_next,
            backoff: PollBackoff::new(
                Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
                config.max_polling_interval_ms.map(Duration::from_millis),
            ),
            checkpoint,
            last_value: Arc::new(Mutex::new(last_value)),
            run_position,
            raw_json_columns,
            source_metadata,
        })
    }
}

impl SqlxCursorReader {
    fn page_sql(&self, has_cursor: bool) -> &str {
        if has_cursor {
            &self.sql_next
        } else {
            &self.sql_first
        }
    }

    /// Stamp the replay position an idempotent sink names its objects from. The cursor value
    /// *is* the position, so it has to be a unique non-negative integer: a text cursor has no
    /// contiguous ordering, and a repeated value would make two rows resolve to the same
    /// output record, silently dropping one. Rows arrive ordered, so ties are adjacent.
    fn stamp_source_position(
        &self,
        message: &mut CanonicalMessage,
        cursor: &SqlCursor,
        previous: Option<&SqlCursor>,
    ) -> Result<(), ConsumerError> {
        let SqlCursor::Int(value) = cursor else {
            return Err(ConsumerError::Permanent(anyhow!(
                "source_metadata requires cursor_column '{}' to be an integer, but it read as \
                 text. Point it at an integer key column, or CAST one in a view.",
                self.cursor_column
            )));
        };
        let value = u64::try_from(*value).map_err(|_| {
            ConsumerError::Permanent(anyhow!(
                "source_metadata requires cursor_column '{}' to be non-negative; got {value}.",
                self.cursor_column
            ))
        })?;
        if previous == Some(cursor) {
            return Err(ConsumerError::Permanent(anyhow!(
                "source_metadata requires cursor_column '{}' to be unique, but value {value} \
                 repeats. Both rows would name the same output record and one would be dropped.",
                self.cursor_column
            )));
        }

        message
            .metadata
            .insert("mqb.src.sqlx_table".to_string(), self.table.clone());
        message
            .metadata
            .insert("mqb.src.sqlx_cursor".to_string(), value.to_string());
        Ok(())
    }
}

#[async_trait]
impl MessageConsumer for SqlxCursorReader {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        let last = self.last_value.lock().unwrap().clone();
        // Peek one extra row beyond the batch so we can detect a run of equal cursor
        // values split across the LIMIT boundary; `col > last` would otherwise skip the
        // remainder of that run (silent row loss for a non-unique cursor_column).
        let fetch_limit = (max_messages as i64).saturating_add(1);

        let sql = self.page_sql(last.is_some());
        let rows = match fetch_page(&self.pool, sql, last.as_ref(), fetch_limit).await {
            Ok(rows) => rows,
            // A decode/type-mapping failure is permanent: the same query will fail identically
            // every poll. Surface it as a non-retryable error so the route stops instead of
            // reconnecting forever, and point the user at the fix.
            Err(e) if is_permanent_decode_error(&e) => {
                return Err(ConsumerError::Connection(anyhow::Error::new(
                    crate::errors::ProcessingError::NonRetryable(anyhow!(
                        "SQLx cursor reader on table '{}' hit a column of a type the SQL `Any` \
                         driver cannot decode: {e}. This is permanent; expose the column as \
                         TEXT/BIGINT (e.g. via a view that CASTs it) and point the reader there.",
                        self.table
                    )),
                )));
            }
            Err(e) => return Err(classify_sql_consumer_error(e)),
        };

        if rows.is_empty() {
            // Drained: preserve polling cadence (backing off if configured), then surface an empty
            // batch so the route can pause or terminate.
            tokio::time::sleep(self.backoff.idle_delay()).await;
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        // Rows arrived: return to the base polling interval.
        self.backoff.reset();

        // Resolve column positions and JSON keys once; every row in this batch shares the
        // same query/column order. Value *kinds* are still read per row (SQLite types
        // values, not columns).
        let cursor_col = resolve_cursor_column(&rows[0], &self.cursor_column);
        let mut schema = JsonRowSchema::from_row(&rows[0]);
        schema.mark_raw_json(&rows[0], &self.raw_json_columns);

        // Extract (cursor, message) for every fetched row.
        let mut fetched: Vec<(SqlCursor, CanonicalMessage)> = Vec::with_capacity(rows.len());
        // Rows of one table are near-uniform in size, so the previous row's length is a good
        // capacity guess and spares the payload buffer its doubling reallocations.
        let mut size_hint = 256usize;
        for row in &rows {
            let cursor = cursor_col
                .and_then(|idx| extract_cursor_at(row, idx))
                .ok_or_else(|| {
                    // Schema-level, so re-polling fails identically: permanent, not a reconnect.
                    ConsumerError::Permanent(anyhow!(
                        "cursor_column '{}' is missing or of a type the SQL `Any` driver cannot decode \
                         (only integer and text cursors are supported). CAST it to BIGINT/TEXT in a view, \
                         or point cursor_column at an integer or text column.",
                        self.cursor_column
                    ))
                })?;
            let mut payload = Vec::with_capacity(size_hint);
            schema.encode_row(row, &mut payload);
            size_hint = payload.len() + payload.len() / 8;
            fetched.push((cursor, CanonicalMessage::new(payload, None)));
        }

        // If we fetched the peek row, more rows exist beyond this page. Drop the trailing
        // run whose value equals the peek row's value so a group of equal cursor values is
        // never split across pages; the trimmed rows are re-read next poll via `col > last`.
        let had_more = fetched.len() > max_messages;
        let mut emit_len = fetched.len().min(max_messages);
        if had_more {
            let peek_val = fetched[max_messages].0.clone();
            while emit_len > 0 && fetched[emit_len - 1].0 == peek_val {
                emit_len -= 1;
            }
            if emit_len == 0 {
                // A single cursor value fills the whole batch and more rows with that value exist
                // beyond it. Advancing past the value would silently skip the remainder, so fail
                // loudly instead of losing rows. Permanent: re-polling returns the same page forever.
                return Err(ConsumerError::Permanent(anyhow!(
                    "cursor_column '{}' has a group of equal values larger than batch_size ({}); \
                     cannot page without skipping rows. Increase batch_size above the size of the \
                     largest equal-value group.",
                    self.cursor_column,
                    max_messages
                )));
            }
        }
        fetched.truncate(emit_len);

        let mut messages = Vec::with_capacity(fetched.len());
        let mut cursors: Vec<SqlCursor> = Vec::with_capacity(fetched.len());
        for (cursor, mut msg) in fetched {
            if self.source_metadata {
                self.stamp_source_position(&mut msg, &cursor, cursors.last())?;
            }
            cursors.push(cursor.clone());
            messages.push(msg);
            // Advance optimistically so the next page continues past this row; rolled back
            // in commit if a row is not acked.
            *self.last_value.lock().unwrap() = Some(cursor);
        }
        trace!(count = messages.len(), "Received batch of SQLx cursor rows");

        let checkpoint = self.checkpoint.clone();
        let run_position = self.run_position.clone();
        let last_value = self.last_value.clone();
        let resume_from = last; // cursor value before this batch (for rollback on nack)
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Count the contiguous run of Acks from the front (stop at first Nack).
                let mut acked = 0usize;
                for disp in dispositions.iter().take(cursors.len()) {
                    if matches!(disp, MessageDisposition::Ack | MessageDisposition::Reply(_)) {
                        acked += 1;
                    } else {
                        break;
                    }
                }
                let boundary = if acked == 0 {
                    resume_from
                } else {
                    Some(cursors[acked - 1].clone())
                };
                // If any row was not acked, roll the in-memory read cursor back to the
                // committed boundary so nacked/unprocessed rows are re-read next poll
                // (at-least-once) instead of being skipped until a restart.
                if acked < cursors.len() {
                    *last_value.lock().unwrap() = boundary.clone();
                }
                if let (Some(cur), Some((positions, key))) = (&boundary, run_position) {
                    positions.lock().unwrap().insert(key, cur.encode());
                }
                if let (Some(cur), Some(cp)) = (boundary, checkpoint) {
                    if let Err(e) = cp.save(&cur.encode()).await {
                        tracing::warn!(error = %e, "Failed to persist sql cursor. Rows may be reprocessed on restart.");
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
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
            details: serde_json::json!({ "driver": self.driver_name, "mode": "cursor_column", "cursor_column": self.cursor_column }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests;
