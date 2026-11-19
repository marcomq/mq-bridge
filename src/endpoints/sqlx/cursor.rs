//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// A cursor value, tracked per column and persisted as a tagged string.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SqlCursor {
    Int(i64),
    Text(String),
}

impl SqlCursor {
    pub(super) fn encode(&self) -> String {
        match self {
            SqlCursor::Int(n) => format!("int:{}", n),
            SqlCursor::Text(s) => format!("str:{}", s),
        }
    }

    pub(super) fn decode(s: &str) -> Option<SqlCursor> {
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
    render: ColumnRender,
}

/// How a column's value becomes JSON, beyond the native `Any` decode.
#[derive(Clone, Copy, PartialEq)]
enum ColumnRender {
    Native,
    /// The text value is already JSON and is embedded as it is.
    RawJson,
    /// Postgres text of the given type OID, rendered as the `postgres_cdc` stream does.
    #[cfg(feature = "postgres-cdc")]
    PgText(u32),
}

/// The column layout of a batch, resolved once from its first row.
pub(super) struct JsonRowSchema {
    columns: Vec<JsonColumn>,
}

impl JsonRowSchema {
    pub(super) fn from_row(row: &sqlx::any::AnyRow) -> Self {
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
                    render: ColumnRender::Native,
                }
            })
            .collect();
        Self { columns }
    }

    fn mark_renders(&mut self, row: &sqlx::any::AnyRow, renders: &[(String, ColumnRender)]) {
        if renders.is_empty() {
            return;
        }
        for (col, meta) in self.columns.iter_mut().zip(row.columns()) {
            col.render = renders
                .iter()
                .find(|(n, _)| n == meta.name())
                .map_or(ColumnRender::Native, |(_, r)| *r);
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
    pub(super) fn encode_row(&self, row: &sqlx::any::AnyRow, buf: &mut Vec<u8>) {
        buf.push(b'{');
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            buf.extend_from_slice(&col.key);
            match col.render {
                ColumnRender::Native => {}
                ColumnRender::RawJson => {
                    match row.try_get::<Option<&str>, _>(col.ordinal) {
                        Ok(Some(json)) if !json.is_empty() => {
                            buf.extend_from_slice(json.as_bytes())
                        }
                        _ => buf.extend_from_slice(b"null"),
                    }
                    continue;
                }
                #[cfg(feature = "postgres-cdc")]
                ColumnRender::PgText(oid) => {
                    match row.try_get::<Option<&str>, _>(col.ordinal) {
                        Ok(Some(text)) => {
                            let value = crate::endpoints::postgres::text_to_json(oid, text)
                                .unwrap_or_else(|_| text.into());
                            let _ = serde_json::to_writer(&mut *buf, &value);
                        }
                        _ => buf.extend_from_slice(b"null"),
                    }
                    continue;
                }
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
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a SQL identifier for `driver_name`. MySQL/MariaDB use backticks; everyone else uses
/// the SQL-standard double quote. Required for the checkpoint meta table: `last_value` is a
/// reserved word in MySQL 8 (the `LAST_VALUE` window function), so unquoted DDL is a syntax
/// error there — and a user-supplied table name may need quoting on any driver.
pub(super) fn quote_ident_for(driver_name: &str, name: &str) -> String {
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
pub(super) const SQLITE_COLUMN_TYPES_SQL: &str =
    "SELECT name AS name, LOWER(type) AS typname FROM pragma_table_info(?, ?) ORDER BY cid";

/// The columns a reader projects: `table_xinfo` adds the generated ones; `hidden = 1` is a
/// virtual table's hidden column.
const SQLITE_READ_COLUMN_TYPES_SQL: &str = "SELECT name AS name, LOWER(type) AS typname \
     FROM pragma_table_xinfo(?, ?) WHERE hidden <> 1 ORDER BY cid";

/// `$1::regclass` resolves the (optionally schema-qualified) table name against the current
/// search_path. `pg_attribute.attname`/`pg_type.typname` are Postgres `name`-typed columns,
/// which `Any` cannot decode directly (distinct from `text`), so cast them explicitly.
const PG_COLUMN_TYPES_SQL: &str = "SELECT a.attname::text AS name, t.typname::text AS typname, \
     a.atttypid::int8 AS oid \
     FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
     WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

/// The bind is the schema part of a `db.table` name, empty for an unqualified one — in which
/// case the connection's current database is used.
pub(super) const MYSQL_COLUMN_TYPES_SQL: &str =
    "SELECT COLUMN_NAME AS name, LOWER(DATA_TYPE) AS typname \
     FROM information_schema.columns \
     WHERE table_schema = COALESCE(NULLIF(?, ''), DATABASE()) AND table_name = ? \
     ORDER BY ORDINAL_POSITION";

/// Rejects `load_mode` on a MySQL/MariaDB table whose engine cannot roll back (MyISAM, MEMORY, ...).
/// A table that does not exist yet passes; it is created with the server's default engine.
pub(super) async fn ensure_transactional_engine(pool: &AnyPool, table: &str) -> anyhow::Result<()> {
    let (schema, name) = match table.split_once('.') {
        Some((s, t)) => (s.trim_matches('`'), t.trim_matches('`')),
        None => ("", table.trim_matches('`')),
    };
    let row: Option<(String, String)> = sqlx::query_as(audited_sql(
        "SELECT CAST(t.ENGINE AS CHAR), CAST(COALESCE(e.TRANSACTIONS, 'NO') AS CHAR) \
         FROM information_schema.TABLES t \
         LEFT JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE \
         WHERE t.TABLE_SCHEMA = COALESCE(NULLIF(?, ''), DATABASE()) AND t.TABLE_NAME = ? \
         AND t.ENGINE IS NOT NULL",
    ))
    .bind(schema.to_string())
    .bind(name.to_string())
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow!("Failed to look up the storage engine of '{table}': {e}"))?;
    match row {
        Some((engine, transactions)) if !transactions.eq_ignore_ascii_case("YES") => {
            Err(anyhow!(crate::errors::InvalidConfig(anyhow!(
                "`load_mode` needs a transactional table, but '{table}' uses the {engine} engine, \
                 which cannot roll back a failed load; convert it (e.g. to InnoDB) first"
            ))))
        }
        _ => Ok(()),
    }
}

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
) -> (String, Vec<(String, ColumnRender)>) {
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
                SQLITE_READ_COLUMN_TYPES_SQL,
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
    #[cfg(feature = "postgres-cdc")]
    let cdc = driver_name == "PostgreSQL" && crate::endpoints::postgres::cdc_values();
    let mut parts = Vec::with_capacity(rows.len());
    let mut renders = Vec::new();
    for row in &rows {
        let name: String = match row.try_get("name") {
            Ok(n) => n,
            Err(_) => return star(),
        };
        let typname: String = row.try_get("typname").unwrap_or_default();
        let ident = quote_ident_for(driver_name, &name);
        #[cfg(feature = "postgres-cdc")]
        if cdc
            && !matches!(
                typname.as_str(),
                "bool" | "int2" | "int4" | "int8" | "text" | "varchar" | "json" | "jsonb"
            )
        {
            let oid: i64 = row.try_get("oid").unwrap_or_default();
            parts.push(cast(&ident));
            renders.push((name, ColumnRender::PgText(oid as u32)));
            continue;
        }
        if is_safe(&typname) {
            parts.push(ident);
        } else if driver_name == "PostgreSQL" && matches!(typname.as_str(), "json" | "jsonb") {
            parts.push(cast(&ident));
            renders.push((name, ColumnRender::RawJson));
        } else if let Some(rendered) = pg_rfc3339(driver_name, &typname, &ident, timestamps) {
            parts.push(rendered);
        } else if driver_name == "PostgreSQL" && typname.starts_with('_') {
            // Array types: the text form is `{1,2}`, so render them as a JSON array.
            parts.push(format!("to_jsonb({ident})::text AS {ident}"));
            renders.push((name, ColumnRender::RawJson));
        } else {
            // Cast to a string type so `Any` can decode it; keep the column name via alias.
            parts.push(cast(&ident));
        }
    }
    (parts.join(", "), renders)
}

/// The projection for a Postgres timestamp column under `timestamps: rfc3339`.
pub(super) fn pg_rfc3339(
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

/// The type of a Postgres cursor column the projection renders as text (timestamps, `numeric`,
/// `uuid`, `date`), so a stored cursor can be compared with it. `None` for integer and text.
async fn pg_cursor_cast_type(
    pool: &AnyPool,
    driver_name: &str,
    config: &SqlxConfig,
    cursor_column: &str,
) -> Option<String> {
    if driver_name != "PostgreSQL" {
        return None;
    }
    let row = sqlx::query(
        "SELECT t.typname::text AS typname, a.atttypid::regtype::text AS pg_type \
         FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
         WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
         AND a.attname IN ($2, LOWER($2))",
    )
    .bind(config.table.clone())
    .bind(cursor_column.trim_matches('"').to_string())
    .fetch_optional(pool)
    .await
    .ok()??;
    let typname: String = row.try_get("typname").ok()?;
    if pg_typname_is_any_safe(&typname) {
        return None;
    }
    row.try_get("pg_type").ok()
}

/// A permanent (non-transient) failure: the `Any` driver cannot decode a column type, so
/// retrying the identical query will fail identically. Detected so the route fails fast with
/// a clear message instead of reconnecting on a 5s loop forever.
pub(super) fn is_permanent_decode_error(e: &sqlx::Error) -> bool {
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
    let pool = AnyPool::connect(url).await.with_context(|| {
        format!(
            "Failed to connect checkpoint store at '{}'",
            crate::support::redact::url_password(url)
        )
    })?;
    let driver_name = {
        let conn = pool.acquire().await?;
        let name = conn.backend_name().to_string();
        drop(conn);
        name
    };
    let meta_table = match table {
        Some(table) => table,
        None => default_meta_table(&pool, &driver_name, source_name).await?,
    };
    source_sql_checkpoint_store(pool, driver_name, meta_table, source_name, cursor_id).await
}

/// The default meta table for `source_name`. A name that sanitization changed gets a hash
/// suffix, unless a table under the unsuffixed name already holds cursors.
async fn default_meta_table(
    pool: &AnyPool,
    driver_name: &str,
    source_name: &str,
) -> anyhow::Result<String> {
    let legacy = crate::checkpoint::default_meta_name(source_name);
    let Some(unique) = crate::checkpoint::disambiguated_meta_name(source_name) else {
        return Ok(legacy);
    };
    let sql = match driver_name {
        "PostgreSQL" => {
            "SELECT COUNT(*) FROM pg_class \
             WHERE relname = $1 AND relkind IN ('r', 'p') AND pg_table_is_visible(oid)"
        }
        "MySQL" | "MariaDB" => {
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?"
        }
        "SQLite" => "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        _ => return Ok(legacy),
    };
    let existing: i64 = sqlx::query_scalar(audited_sql(sql))
        .bind(legacy.clone())
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow!("Failed to look up meta table '{legacy}': {e}"))?;
    Ok(if existing > 0 { legacy } else { unique })
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
            let name = match config.checkpoint_store {
                None => default_meta_table(&pool, &driver_name, &config.table).await?,
                Some(_) => name,
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
    position: Arc<Mutex<CursorPosition<SqlCursor>>>,
    run_position: Option<(crate::route::RunPositions, String)>,
    /// Columns the projection renders as JSON text (Postgres `json`/`jsonb` and arrays).
    column_renders: Vec<(String, ColumnRender)>,
    /// Page queries, built once: only the bound cursor and limit vary between polls.
    sql_first: String,
    sql_next: String,
    /// Unguarded `sql_first`/`sql_next` probes, set when the commit-order guard is on.
    sql_unguarded: Option<(String, String)>,
    hidden_rows_warned: bool,
    source_metadata: bool,
}

/// `age()` of the oldest transaction still in flight, as the 32-bit xid row `xmin`s compare to.
const PG_OLDEST_IN_FLIGHT_AGE: &str =
    "age((pg_snapshot_xmin(pg_current_snapshot())::text::bigint % 4294967296)::text::xid)";

/// Whether the table has an `xmin` to guard on; a view or a Postgres-compatible server may not.
async fn pg_xmin_readable(pool: &AnyPool, table: &str) -> bool {
    let probe =
        format!("SELECT 1 FROM {table} WHERE age(xmin) <= {PG_OLDEST_IN_FLIGHT_AGE} LIMIT 0");
    match sqlx::query(audited_sql(&probe)).fetch_all(pool).await {
        Ok(_) => true,
        Err(e) => {
            warn!(table = %table, error = %e, "SQLx cursor reader cannot read xmin; rows of transactions that commit out of cursor order may be skipped");
            false
        }
    }
}

/// Ends a page before the first row written by a transaction no older than the oldest open one
/// that has written: it may yet commit a lower cursor value, which `col > last` would skip.
/// Only the page window is checked, so the cost stays one page.
///
/// `ARRAY(...)` runs the window once as an InitPlan; its lowest value (a NULL first) cuts the
/// same rows as `< ALL` over the whole window. `CASE` hides the `ALL` from the row estimate,
/// which would otherwise favour a sort over the index or, as a SubPlan, trigger JIT per page.
fn pg_in_flight_cut(
    table: &str,
    cursor_column: &str,
    order_column: &str,
    lower: Option<&str>,
    limit: &str,
) -> String {
    let window_filter = lower
        .map(|lower| format!(" WHERE {cursor_column} > {lower}"))
        .unwrap_or_default();
    format!(
        "CASE WHEN {cursor_column} < ALL (ARRAY(SELECT mqb_w.mqb_c FROM (SELECT {cursor_column} \
         AS mqb_c, xmin AS mqb_x FROM {table}{window_filter} ORDER BY {order_column} ASC LIMIT \
         {limit}) mqb_w WHERE age(mqb_w.mqb_x) <= {PG_OLDEST_IN_FLIGHT_AGE} \
         ORDER BY mqb_w.mqb_c ASC NULLS FIRST LIMIT 1)) THEN true END"
    )
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
            // Starting over on a cursor that cannot be read would copy the whole table again.
            Some(cp) => match cp.load().await? {
                Some(s) => Some(SqlCursor::decode(&s).ok_or_else(|| {
                    crate::errors::InvalidConfig(anyhow!(
                        "the stored cursor '{s}' of table '{}' cannot be read; expected `int:<number>` or `str:<text>`. Set it again or reset the checkpoint (`mqb checkpoint set|reset`)",
                        config.table
                    ))
                })?),
                None => None,
            },
            None => run_position.as_ref().and_then(|(positions, key)| {
                let saved = positions.lock().unwrap().get(key).cloned();
                saved.and_then(|s| SqlCursor::decode(&s))
            }),
        };
        info!(table = %config.table, cursor_id = ?config.cursor_id, has_checkpoint = %last_value.is_some(), "SQLx cursor reader initialized");

        let (projection, column_renders) =
            build_cursor_projection(&pool, &driver_name, &config.table, config.timestamps).await;

        let lower_bound = positional_placeholder(&driver_name, 1);
        // Postgres has no `timestamptz > text`; the cursor is read as text, so cast it back.
        // The table prefix makes ORDER BY sort the column, not its text form in the projection.
        let (lower_bound, order_column) =
            match pg_cursor_cast_type(&pool, &driver_name, config, &cursor_column).await {
                Some(pg_type) => (
                    format!("CAST({lower_bound} AS {pg_type})"),
                    format!("{}.{cursor_column}", config.table),
                ),
                None => (lower_bound, cursor_column.clone()),
            };
        let commit_order_guard =
            driver_name == "PostgreSQL" && pg_xmin_readable(&pool, &config.table).await;
        let guard = |lower: Option<&str>, limit: &str, joiner: &str| {
            if commit_order_guard {
                let cut =
                    pg_in_flight_cut(&config.table, &cursor_column, &order_column, lower, limit);
                format!(" {joiner} {cut}")
            } else {
                String::new()
            }
        };
        let first_limit = positional_placeholder(&driver_name, 1);
        let sql_first = format!(
            "SELECT {0} FROM {1}{2} ORDER BY {3} ASC LIMIT {4}",
            projection,
            config.table,
            guard(None, &first_limit, "WHERE"),
            order_column,
            first_limit,
        );
        let next_limit = positional_placeholder(&driver_name, 2);
        let sql_next = format!(
            "SELECT {0} FROM {1} WHERE {2} > {3}{4} ORDER BY {5} ASC LIMIT {6}",
            projection,
            config.table,
            cursor_column,
            lower_bound,
            guard(Some(&lower_bound), &next_limit, "AND"),
            order_column,
            next_limit,
        );
        let sql_unguarded = commit_order_guard.then(|| {
            (
                format!("SELECT 1 FROM {} LIMIT {first_limit}", config.table),
                format!(
                    "SELECT 1 FROM {} WHERE {cursor_column} > {lower_bound} LIMIT {next_limit}",
                    config.table
                ),
            )
        });

        Ok(Self {
            pool,
            table: config.table.clone(),
            cursor_column,
            driver_name,
            sql_first,
            sql_next,
            sql_unguarded,
            hidden_rows_warned: false,
            backoff: PollBackoff::new(
                Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
                config.max_polling_interval_ms.map(Duration::from_millis),
            ),
            checkpoint,
            position: Arc::new(Mutex::new(CursorPosition {
                last: last_value,
                epoch: 0,
            })),
            run_position,
            column_renders,
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

    /// Whether an empty guarded page hides committed rows behind a transaction still open.
    async fn rows_hidden_by_open_transaction(
        &mut self,
        last: Option<&SqlCursor>,
    ) -> Result<bool, ConsumerError> {
        let Some((first, next)) = &self.sql_unguarded else {
            return Ok(false);
        };
        let sql = if last.is_some() { next } else { first };
        let rows = fetch_page(&self.pool, sql, last, 1)
            .await
            .map_err(classify_sql_consumer_error)?;
        if rows.is_empty() {
            return Ok(false);
        }
        if !self.hidden_rows_warned {
            self.hidden_rows_warned = true;
            warn!(table = %self.table, "SQLx cursor reader is waiting for an open transaction elsewhere on the server; committed rows after it are held back until it ends");
        }
        Ok(true)
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

        message.set_metadata("mqb.src.sqlx_table", self.table.clone());
        message.set_metadata("mqb.src.sqlx_cursor", value.to_string());
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

        loop {
            let (last, epoch) = {
                let position = self.position.lock().unwrap();
                (position.last.clone(), position.epoch)
            };
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
                // Committed rows held back by the guard are not drained; keep polling for them.
                if self.rows_hidden_by_open_transaction(last.as_ref()).await? {
                    continue;
                }
                // A nack rewound the cursor during the wait: read again from there.
                if self.position.lock().unwrap().epoch != epoch {
                    continue;
                }
                self.hidden_rows_warned = false;
                return Ok(ReceivedBatch {
                    messages: Vec::new(),
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                });
            }
            // Rows arrived: return to the base polling interval.
            self.backoff.reset();
            self.hidden_rows_warned = false;

            // Resolve column positions and JSON keys once; every row in this batch shares the
            // same query/column order. Value *kinds* are still read per row (SQLite types
            // values, not columns).
            let cursor_col = resolve_cursor_column(&rows[0], &self.cursor_column);
            let mut schema = JsonRowSchema::from_row(&rows[0]);
            schema.mark_renders(&rows[0], &self.column_renders);

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
                cursors.push(cursor);
                messages.push(msg);
            }
            {
                let mut position = self.position.lock().unwrap();
                // A nack rewound the cursor while this page was read: read from there instead.
                if position.epoch != epoch {
                    continue;
                }
                // Advance optimistically so the next page continues past these rows; rolled
                // back in commit if a row is not acked.
                position.last = cursors.last().cloned();
            }
            trace!(count = messages.len(), "Received batch of SQLx cursor rows");

            let checkpoint = self.checkpoint.clone();
            let run_position = self.run_position.clone();
            let mut unsettled = Unsettled {
                position: self.position.clone(),
                resume_from: last,
                epoch,
                settled: false,
            };
            let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
                Box::pin(async move {
                    let boundary = unsettled.settle(&cursors, &dispositions);
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

            return Ok(ReceivedBatch { messages, commit });
        }
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = super::pool_health(&self.pool).await;
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
