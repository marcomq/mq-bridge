//! Logical-replication transport: wraps `pgwire-replication` for the streaming
//! wire protocol and uses `sqlx` for the slot-lifecycle control plane
//! (`CREATE`/`ADVANCE`/`DROP` replication slot), which requires an ordinary SQL
//! connection rather than a replication one.
//!
//! The URL parsing, slot lifecycle, and TLS mapping follow the approach in
//! faucet-stream (crates/source/postgres-cdc/src/replication.rs, dual
//! Apache-2.0 OR MIT), adapted to mq-bridge's config and error types.
//! Pinned upstream commit and per-file changes: `./VENDORED.md`.

use crate::endpoints::postgres::state::format_lsn;
use crate::models::PostgresCdcConfig;
use anyhow::{anyhow, Context};
use percent_encoding::percent_decode_str;
use pgwire_replication::{Lsn, ReplicationClient, ReplicationConfig, TlsConfig as PgTlsConfig};
use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, PgConnection};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, warn};

pub use pgwire_replication::ReplicationEvent;

/// Postgres connection coordinates parsed from the configured URL.
struct PgCoords {
    host: String,
    port: u16,
    user: String,
    password: String,
    dbname: String,
}

fn parse_url(url: &str) -> anyhow::Result<PgCoords> {
    let parsed = url::Url::parse(url).context("postgres-cdc: invalid connection URL")?;
    let host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "postgres-cdc: connection URL is missing a host \
                 (expected postgres://user@host[:port]/dbname)"
            )
        })?
        .to_owned();
    let port = parsed.port().unwrap_or(5432);
    // The `url` crate leaves userinfo percent-encoded; decode it so credentials
    // with reserved characters (e.g. `@`, `:`, `/`) reach the server intact.
    let user = percent_decode_str(parsed.username())
        .decode_utf8_lossy()
        .into_owned();
    if user.is_empty() {
        return Err(anyhow!(
            "postgres-cdc: connection URL is missing a user \
             (expected postgres://user@host[:port]/dbname)"
        ));
    }
    let password = percent_decode_str(parsed.password().unwrap_or(""))
        .decode_utf8_lossy()
        .into_owned();
    let dbname = parsed.path().trim_start_matches('/').to_owned();
    let dbname = if dbname.is_empty() {
        "postgres".to_owned()
    } else {
        dbname
    };
    Ok(PgCoords {
        host,
        port,
        user,
        password,
        dbname,
    })
}

/// Map mq-bridge's shared [`TlsConfig`](crate::models::TlsConfig) onto
/// pgwire-replication's TLS model. Client-certificate mTLS is not exposed by
/// the replication transport in this version — warn if it was requested.
fn map_tls(tls: &crate::models::TlsConfig) -> PgTlsConfig {
    if tls.cert_file.is_some() || tls.key_file.is_some() {
        warn!(
            "postgres-cdc: client-certificate mTLS is not supported by the replication \
             transport yet; ignoring cert_file/key_file"
        );
    }
    if !tls.required {
        return PgTlsConfig::disabled();
    }
    if tls.accept_invalid_certs {
        // Encrypt but do not verify the server certificate.
        return PgTlsConfig::require();
    }
    // Verify the server certificate (and hostname), optionally against a
    // supplied CA bundle; falls back to the system roots when `ca_file` is None.
    PgTlsConfig::verify_full(tls.ca_file.clone().map(PathBuf::from))
}

/// Open a plain SQL connection for control-plane slot operations, applying the
/// same TLS enforcement as the replication stream so the slot lifecycle can't
/// silently fall back to an unverified `sslmode=prefer` connection.
async fn control_conn(url: &str, tls: &crate::models::TlsConfig) -> anyhow::Result<PgConnection> {
    let mut opts: PgConnectOptions = url
        .parse()
        .context("postgres-cdc: invalid connection URL")?;
    if tls.required {
        opts = opts.ssl_mode(if tls.accept_invalid_certs {
            sqlx::postgres::PgSslMode::Require
        } else {
            sqlx::postgres::PgSslMode::VerifyFull
        });
        if let Some(ca_file) = &tls.ca_file {
            opts = opts.ssl_root_cert(ca_file);
        }
    }
    opts.connect()
        .await
        .map_err(|e| anyhow!("postgres-cdc: control-plane connect failed: {e}"))
}

/// Ensure the logical replication slot exists, creating it with the `pgoutput`
/// plugin if missing and `create_if_missing` is true.
///
/// The slot is always created as **permanent**, even for an ephemeral run. A
/// Postgres *temporary* slot lives and dies with the session that created it, and
/// the streaming connection is a separate session from this control connection —
/// so a temporary slot would be gone before `START_REPLICATION` could use it.
/// Ephemeral runs (`temporary_slot: true`) instead get the slot dropped on
/// teardown; see [`drop_slot`] and `PostgresCdcConsumer`'s `Drop`.
pub async fn ensure_slot(
    url: &str,
    slot_name: &str,
    create_if_missing: bool,
    drop_on_stop: bool,
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<()> {
    let mut conn = control_conn(url, tls).await?;

    let row: Option<(String,)> =
        sqlx::query_as("SELECT slot_name::text FROM pg_replication_slots WHERE slot_name = $1")
            .bind(slot_name)
            .fetch_optional(&mut conn)
            .await
            .map_err(|e| anyhow!("postgres-cdc: slot lookup failed: {e}"))?;

    if row.is_some() {
        debug!("postgres-cdc: replication slot '{slot_name}' already exists");
        return Ok(());
    }
    if !create_if_missing {
        return Err(anyhow!(
            "postgres-cdc: replication slot '{slot_name}' does not exist and create_slot = false"
        ));
    }

    sqlx::query("SELECT pg_create_logical_replication_slot($1, 'pgoutput', false)")
        .bind(slot_name)
        .execute(&mut conn)
        .await
        .map_err(|e| anyhow!("postgres-cdc: create slot failed: {e}"))?;

    if drop_on_stop {
        debug!(
            "postgres-cdc: created replication slot '{slot_name}'; it is dropped again when the \
             route stops (temporary_slot: true)"
        );
    } else {
        warn!(
            "postgres-cdc: created PERMANENT replication slot '{slot_name}' — it retains WAL on \
             the server until consumed or explicitly dropped. Use temporary_slot: true for \
             ephemeral runs."
        );
    }
    Ok(())
}

/// A table a `capture_all`/`snapshot` backfill will page through.
#[derive(Debug, Clone)]
pub struct SnapshotTable {
    pub schema: String,
    pub table: String,
    /// The single-column primary key the backfill pages by.
    pub key: String,
    /// Whether `key` is an integer type, which source positions require.
    pub key_is_integer: bool,
}

impl SnapshotTable {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// The publication's tables with the primary key each backfill phase pages by.
///
/// A table without exactly one primary-key column cannot be paged safely, so it is an error
/// naming the table rather than a silently skipped or non-resumable read.
pub async fn snapshot_tables(
    url: &str,
    publication: &str,
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<Vec<SnapshotTable>> {
    let mut conn = control_conn(url, tls).await?;

    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT schemaname::text, tablename::text FROM pg_publication_tables          WHERE pubname = $1 ORDER BY schemaname, tablename",
    )
    .bind(publication)
    .fetch_all(&mut conn)
    .await
    .map_err(|e| anyhow!("postgres-cdc: listing publication tables failed: {e}"))?;

    if tables.is_empty() {
        return Err(anyhow!(
            "postgres-cdc: publication '{publication}' captures no tables, so there is \
             nothing to back fill"
        ));
    }

    let mut out = Vec::with_capacity(tables.len());
    for (schema, table) in tables {
        let keys: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.attname::text, t.typname::text              FROM pg_index i              JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)              JOIN pg_type t ON t.oid = a.atttypid              WHERE i.indrelid = format('%I.%I', $1, $2)::regclass AND i.indisprimary",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&mut conn)
        .await
        .map_err(|e| anyhow!("postgres-cdc: primary-key lookup for {schema}.{table} failed: {e}"))?;

        let [(key, typname)] = keys.as_slice() else {
            return Err(anyhow!(
                "postgres-cdc: cannot back fill {schema}.{table} — it has {} primary-key \
                 columns and paging needs exactly one. Spell the backfill out as a `sequence` \
                 input with an explicit `cursor_column` instead.",
                keys.len()
            ));
        };
        out.push(SnapshotTable {
            schema,
            table,
            key: key.clone(),
            key_is_integer: matches!(typname.as_str(), "int2" | "int4" | "int8"),
        });
    }
    Ok(out)
}

/// Drop the replication slot, waiting for the server to release it first.
/// `pg_drop_replication_slot` refuses to run while a walsender still holds the
/// slot, and the server releases it only shortly after the streaming socket
/// closes — so poll `pg_replication_slots.active` the same way
/// [`advance_slot_when_inactive`] does. A slot that is already gone is a no-op.
pub async fn drop_slot(
    url: &str,
    slot_name: &str,
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<()> {
    let mut conn = control_conn(url, tls).await?;
    if !wait_for_slot_release(&mut conn, slot_name).await? {
        return Ok(());
    }
    sqlx::query("SELECT pg_drop_replication_slot($1)")
        .bind(slot_name)
        .execute(&mut conn)
        .await
        .map_err(|e| anyhow!("postgres-cdc: drop slot failed: {e}"))?;
    debug!("postgres-cdc: dropped replication slot '{slot_name}'");
    Ok(())
}

/// Poll `pg_replication_slots.active` until the walsender lets go of the slot. Both
/// `pg_replication_slot_advance` and `pg_drop_replication_slot` refuse to run on an active
/// slot, and the server releases it only shortly after the streaming socket closes.
///
/// Returns `false` when the slot no longer exists (nothing left to do), `true` once it is
/// inactive — or after the budget runs out, letting the caller's statement produce the real
/// server error rather than a synthetic timeout.
async fn wait_for_slot_release(conn: &mut PgConnection, slot_name: &str) -> anyhow::Result<bool> {
    // ~5s: the stream is asked to stop just before this, but CopyDone plus the server's
    // walsender cleanup is a round trip, and this runs once per route stop.
    for _ in 0..200 {
        let active: Option<(Option<bool>,)> =
            sqlx::query_as("SELECT active FROM pg_replication_slots WHERE slot_name = $1")
                .bind(slot_name)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| anyhow!("postgres-cdc: slot status check failed: {e}"))?;
        match active {
            None => return Ok(false),
            Some((Some(false) | None,)) => return Ok(true),
            Some((Some(true),)) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    warn!("postgres-cdc: replication slot '{slot_name}' still active after 5s; proceeding anyway");
    Ok(true)
}

/// True when an error reports that the replication slot is gone (SQLSTATE 42704,
/// `undefined_object`). Reconnecting cannot recreate it, so the route must fail
/// permanently rather than retry on the reconnect interval forever.
pub fn is_missing_slot_error(msg: &str) -> bool {
    msg.contains("42704") || (msg.contains("replication slot") && msg.contains("does not exist"))
}

/// A publication table reference is a plain identifier or a single `schema.table`,
/// each part `[A-Za-z0-9_]`. Both are interpolated into DDL, so they are validated
/// (not bindable as parameters).
fn is_valid_pg_table_ref(s: &str) -> bool {
    match s.split_once('.') {
        Some((schema, table)) => {
            super::is_valid_pg_ident(schema) && super::is_valid_pg_ident(table)
        }
        None => super::is_valid_pg_ident(s),
    }
}

/// Split a table reference into `(schema, table)`; an unqualified name has no schema.
fn split_table_ref(s: &str) -> (Option<&str>, &str) {
    match s.split_once('.') {
        Some((schema, table)) => (Some(schema), table),
        None => (None, s),
    }
}

/// True for a Postgres `duplicate_object` (42710) error — e.g. a concurrent
/// `CREATE PUBLICATION` won the race between our existence check and create.
fn is_duplicate_object(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|c| c == "42710")
}

/// Ensure the publication exists, creating it if missing. With `tables` empty the
/// publication is `FOR ALL TABLES` (needs superuser); otherwise
/// `FOR TABLE <t1, t2, ...>` (needs ownership of each).
///
/// When the publication already exists and named `tables` are given, missing ones
/// are **added** (`ALTER PUBLICATION ... ADD TABLE`) so the config stays the source
/// of truth; tables are never dropped, so an operator's extra tables are left intact.
/// Publication and table names are identifiers (not bindable as parameters), so
/// they are validated as `[A-Za-z0-9_]` (optionally `schema.table`) before interpolation.
pub async fn ensure_publication(
    url: &str,
    publication: &str,
    tables: &[String],
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<()> {
    if !super::is_valid_pg_ident(publication) {
        return Err(anyhow!(
            "postgres-cdc: `publication` must be a non-empty [A-Za-z0-9_] identifier"
        ));
    }
    for t in tables {
        if !is_valid_pg_table_ref(t) {
            return Err(anyhow!(
                "postgres-cdc: publication table '{t}' must be a [A-Za-z0-9_] identifier \
                 (optionally schema-qualified as schema.table)"
            ));
        }
    }
    let mut conn = control_conn(url, tls).await?;

    let exists = sqlx::query_as::<_, (String,)>(
        "SELECT pubname::text FROM pg_publication WHERE pubname = $1",
    )
    .bind(publication)
    .fetch_optional(&mut conn)
    .await
    .map_err(|e| anyhow!("postgres-cdc: publication lookup failed: {e}"))?
    .is_some();

    if !exists {
        let sql = if tables.is_empty() {
            format!("CREATE PUBLICATION {publication} FOR ALL TABLES")
        } else {
            format!(
                "CREATE PUBLICATION {publication} FOR TABLE {}",
                tables.join(", ")
            )
        };
        // Identifiers were validated above, so the interpolated DDL is safe.
        match sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(&mut conn)
            .await
        {
            Ok(_) => warn!(
                "postgres-cdc: created publication '{publication}' — it defines which tables are \
                 captured; drop it with DROP PUBLICATION when no longer needed."
            ),
            // Lost the create race to a concurrent starter; the publication now exists.
            Err(e) if is_duplicate_object(&e) => {
                debug!("postgres-cdc: publication '{publication}' created concurrently")
            }
            Err(e) => return Err(anyhow!("postgres-cdc: create publication failed: {e}")),
        }
    }

    // Reconcile membership (add-only) when we manage a specific table list.
    if !tables.is_empty() {
        add_missing_publication_tables(&mut conn, publication, tables).await?;
    }
    Ok(())
}

/// Add any `tables` not already in the publication via `ALTER PUBLICATION ... ADD TABLE`.
/// An unqualified name is matched by table name in any schema (it resolves via `search_path`);
/// a `schema.table` reference is matched exactly. Never removes tables.
async fn add_missing_publication_tables(
    conn: &mut PgConnection,
    publication: &str,
    tables: &[String],
) -> anyhow::Result<()> {
    let current: Vec<(String, String)> = sqlx::query_as(
        "SELECT schemaname::text, tablename::text FROM pg_publication_tables WHERE pubname = $1",
    )
    .bind(publication)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| anyhow!("postgres-cdc: publication membership lookup failed: {e}"))?;

    for t in tables {
        let (schema, table) = split_table_ref(t);
        let present = current
            .iter()
            .any(|(s, tab)| tab == table && schema.map(|want| want == s).unwrap_or(true));
        if present {
            continue;
        }
        let sql = format!("ALTER PUBLICATION {publication} ADD TABLE {t}");
        match sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(&mut *conn)
            .await
        {
            Ok(_) => warn!("postgres-cdc: added table '{t}' to publication '{publication}'"),
            // Lost the race to a concurrent reconciler; the table is now a member.
            Err(e) if is_duplicate_object(&e) => {
                debug!("postgres-cdc: table '{t}' already in publication '{publication}'")
            }
            Err(e) => {
                return Err(anyhow!(
                    "postgres-cdc: add table '{t}' to publication failed: {e}"
                ))
            }
        }
    }
    Ok(())
}

/// Advance the slot's `confirmed_flush_lsn` to `lsn` while the slot is inactive
/// (before the stream is opened), so a resumed run skips already-persisted
/// changes. `pg_replication_slot_advance` never moves a slot backwards or past
/// the server's insert pointer, so a stale `lsn` is a safe no-op.
pub async fn advance_slot(
    url: &str,
    slot_name: &str,
    lsn: u64,
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<()> {
    if lsn == 0 {
        return Ok(());
    }
    let mut conn = control_conn(url, tls).await?;
    sqlx::query("SELECT pg_replication_slot_advance($1, $2::pg_lsn)")
        .bind(slot_name)
        .bind(format_lsn(lsn))
        .execute(&mut conn)
        .await
        .map_err(|e| anyhow!("postgres-cdc: advance slot failed: {e}"))?;
    debug!("postgres-cdc: advanced slot '{slot_name}' confirmed_flush_lsn to {lsn:#x}");
    Ok(())
}

/// Wait for the slot to become inactive, then advance its `confirmed_flush_lsn`
/// to `lsn`. Used on graceful teardown: the streaming connection must be stopped
/// first, because `pg_replication_slot_advance` refuses to run on an active slot.
/// The server releases the slot shortly after the walsender's socket closes, so
/// we poll `pg_replication_slots.active` briefly before advancing.
pub async fn advance_slot_when_inactive(
    url: &str,
    slot_name: &str,
    lsn: u64,
    tls: &crate::models::TlsConfig,
) -> anyhow::Result<()> {
    if lsn == 0 {
        return Ok(());
    }
    let mut conn = control_conn(url, tls).await?;
    if !wait_for_slot_release(&mut conn, slot_name).await? {
        return Ok(());
    }
    sqlx::query("SELECT pg_replication_slot_advance($1, $2::pg_lsn)")
        .bind(slot_name)
        .bind(format_lsn(lsn))
        .execute(&mut conn)
        .await
        .map_err(|e| anyhow!("postgres-cdc: advance slot failed: {e}"))?;
    debug!("postgres-cdc: advanced slot '{slot_name}' confirmed_flush_lsn to {lsn:#x} on shutdown");
    Ok(())
}

/// Open the logical replication stream. `pgwire-replication` handles TCP, TLS,
/// auth, `START_REPLICATION`, keepalives, and standby-status feedback.
pub async fn start_replication(
    config: &PostgresCdcConfig,
    start_lsn: Option<u64>,
) -> anyhow::Result<ReplicationClient> {
    let coords = parse_url(&config.url)?;
    let status_interval = Duration::from_millis(config.status_interval_ms.max(1));

    let cfg = ReplicationConfig::new(
        coords.host,
        coords.user,
        coords.password,
        coords.dbname,
        config.slot_name.clone(),
        config.publication.clone(),
    )
    .with_port(coords.port)
    .with_tls(map_tls(&config.tls))
    .with_start_lsn(Lsn::from_u64(start_lsn.unwrap_or(0)))
    .with_status_interval(status_interval)
    .with_wakeup_interval(status_interval);

    ReplicationClient::connect(cfg).await.map_err(|e| {
        let msg = format!("postgres-cdc: start_replication failed: {e}");
        // A missing slot cannot heal itself; surface it as permanent so the route
        // stops instead of reconnecting on the interval forever.
        if is_missing_slot_error(&msg) {
            anyhow::Error::new(crate::errors::ConsumerError::Permanent(anyhow!(msg)))
        } else {
            anyhow!(msg)
        }
    })
}
