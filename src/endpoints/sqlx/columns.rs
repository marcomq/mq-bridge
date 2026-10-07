//! `columns: auto`: write each top-level JSON field into the table column of the same name.

use super::{
    audited_sql, bind_key, bind_value, classify_sql_error, copy_in, open_copy_pool,
    positional_placeholder, push_copy_value, quote_ident_for, BindValue, MYSQL_COLUMN_TYPES_SQL,
    SQLITE_COLUMN_TYPES_SQL,
};
use crate::errors::InvalidConfig;
use crate::models::SqlxConfig;
use crate::outcomes::SentBatch;
use crate::traits::PublisherError;
use crate::CanonicalMessage;
use anyhow::anyhow;
use sqlx::postgres::{PgPool, PgPoolCopyExt};
use sqlx::{AnyPool, Row};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

/// Bind parameters per statement; below the limit of every supported driver.
pub(super) const MAX_BINDS: usize = 30_000;

/// `format_type` gives a name a cast accepts, without the length a cast would truncate to.
const PG_COLUMN_CASTS_SQL: &str =
    "SELECT a.attname::text AS name, format_type(a.atttypid, NULL) AS typname \
     FROM pg_attribute a \
     WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

struct TableColumn {
    name: String,
    ident: String,
    /// PostgreSQL only: the column type every bound value is cast to.
    cast: Option<String>,
    /// PostgreSQL array column: a JSON array is written as an array literal.
    array: bool,
    /// PostgreSQL `boolean`: has no cast from a number, so 0 and 1 are written as false and true.
    boolean: bool,
}

/// One record as the columns it names, in table order, and their values.
struct AutoRow {
    columns: Vec<usize>,
    values: Vec<BindValue>,
}

pub(super) struct AutoColumns {
    table: String,
    driver_name: String,
    columns: Vec<TableColumn>,
    by_name: HashMap<String, usize>,
    /// Lower-cased names, without the ones two columns share.
    by_lower_name: HashMap<String, usize>,
    key: Vec<usize>,
    /// Takes the fields without a column of their own, as one JSON object.
    extra: Option<usize>,
    copy_pool: Option<PgPool>,
    warned_unknown: AtomicBool,
}

fn invalid(message: String) -> anyhow::Error {
    anyhow!(InvalidConfig(anyhow!(message)))
}

fn split_table(table: &str, quote: char, default_schema: &str) -> (String, String) {
    match table.split_once('.') {
        Some((schema, name)) => (
            schema.trim_matches(quote).to_string(),
            name.trim_matches(quote).to_string(),
        ),
        None => (
            default_schema.to_string(),
            table.trim_matches(quote).to_string(),
        ),
    }
}

fn json_bind(value: serde_json::Value) -> BindValue {
    match value {
        serde_json::Value::Null => BindValue::Null,
        serde_json::Value::Bool(b) => BindValue::Bool(b),
        serde_json::Value::String(s) => BindValue::Text(s),
        // An integer beyond i64 goes as text: a float would round it.
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => BindValue::Int(i),
            (None, Some(f)) if !n.is_u64() => BindValue::Float(f),
            _ => BindValue::Text(n.to_string()),
        },
        nested => BindValue::Text(nested.to_string()),
    }
}

/// A JSON array as a PostgreSQL array literal: `{1,NULL,"a \"b\""}`.
fn pg_array_literal(items: &[serde_json::Value], out: &mut String) {
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            serde_json::Value::Null => out.push_str("NULL"),
            serde_json::Value::Bool(b) => out.push_str(if *b { "t" } else { "f" }),
            serde_json::Value::Number(n) => out.push_str(&n.to_string()),
            serde_json::Value::Array(nested) => pg_array_literal(nested, out),
            serde_json::Value::String(_) | serde_json::Value::Object(_) => {
                let text = match item {
                    serde_json::Value::String(s) => s.clone(),
                    object => object.to_string(),
                };
                out.push('"');
                for ch in text.chars() {
                    if matches!(ch, '"' | '\\') {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
}

impl AutoColumns {
    pub(super) async fn new(
        pool: &AnyPool,
        driver_name: &str,
        config: &SqlxConfig,
    ) -> anyhow::Result<Self> {
        let table = &config.table;
        if config.insert_query.is_some() {
            return Err(invalid(
                "`columns` and `insert_query` both say how a row is written; set only one".into(),
            ));
        }
        if config.auto_create_table {
            return Err(invalid(
                "`columns: auto` writes into an existing table; `auto_create_table` creates the single-`payload` queue table. Create the table, or drop one of the two".into(),
            ));
        }
        let (sql, binds) = match driver_name {
            "PostgreSQL" => (PG_COLUMN_CASTS_SQL, vec![table.clone()]),
            "MySQL" | "MariaDB" => {
                let (schema, name) = split_table(table, '`', "");
                (MYSQL_COLUMN_TYPES_SQL, vec![schema, name])
            }
            "SQLite" => {
                let (schema, name) = split_table(table, '"', "main");
                (SQLITE_COLUMN_TYPES_SQL, vec![name, schema])
            }
            other => {
                return Err(invalid(format!(
                    "`columns: auto` is not supported for {other}; use an `insert_query`"
                )))
            }
        };
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind(bind);
        }
        let rows = match query.fetch_all(pool).await {
            Ok(rows) => rows,
            // PostgreSQL: `regclass` rejects an unknown table (42P01).
            Err(e)
                if e.as_database_error()
                    .is_some_and(|d| d.code().as_deref() == Some("42P01")) =>
            {
                Vec::new()
            }
            Err(e) => {
                return Err(anyhow!(e).context(format!("reading the columns of table '{table}'")))
            }
        };
        if rows.is_empty() {
            return Err(invalid(format!(
                "table '{table}' does not exist; `columns: auto` writes into an existing table"
            )));
        }
        let mut columns = Vec::with_capacity(rows.len());
        for row in &rows {
            let name: String = row.try_get("name")?;
            let cast = match driver_name {
                "PostgreSQL" => Some(row.try_get::<String, _>("typname")?),
                _ => None,
            };
            columns.push(TableColumn {
                ident: quote_ident_for(driver_name, &name),
                name,
                array: cast.as_ref().is_some_and(|c| c.ends_with("[]")),
                boolean: cast.as_deref() == Some("boolean"),
                cast,
            });
        }

        let by_name: HashMap<String, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        let mut by_lower_name: HashMap<String, usize> = HashMap::new();
        let mut shared = Vec::new();
        for (i, column) in columns.iter().enumerate() {
            let lower = column.name.to_lowercase();
            if by_lower_name.insert(lower.clone(), i).is_some() {
                shared.push(lower);
            }
        }
        for lower in shared {
            by_lower_name.remove(&lower);
        }

        let mut key = Vec::new();
        let mut extra = None;
        let named = config
            .key
            .iter()
            .flat_map(|k| k.split(','))
            .map(|name| ("key", name.trim()))
            .chain(
                config
                    .extra_column
                    .iter()
                    .map(|name| ("extra_column", name.trim())),
            );
        for (option, name) in named {
            let index = by_name
                .get(name)
                .or_else(|| by_lower_name.get(&name.to_lowercase()));
            match index {
                Some(index) if option == "key" => key.push(*index),
                Some(index) => extra = Some(*index),
                None => {
                    return Err(invalid(format!(
                    "`{option}` column '{name}' is not a column of table '{table}' (columns: {})",
                    columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
                }
            }
        }

        let copy_pool = if config.bulk_copy {
            if driver_name != "PostgreSQL" {
                return Err(invalid(format!(
                    "bulk_copy is only supported for PostgreSQL (driver: {driver_name})."
                )));
            }
            if !key.is_empty() {
                return Err(invalid(
                    "bulk_copy cannot update existing rows (COPY has no ON CONFLICT); drop `key` or `bulk_copy`".into(),
                ));
            }
            Some(open_copy_pool(config).await?)
        } else {
            None
        };

        Ok(Self {
            table: table.clone(),
            driver_name: driver_name.to_string(),
            columns,
            by_name,
            by_lower_name,
            key,
            extra,
            copy_pool,
            warned_unknown: AtomicBool::new(false),
        })
    }

    fn column_names(&self) -> String {
        let names: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
        names.join(", ")
    }

    fn column_value(&self, index: usize, value: serde_json::Value) -> BindValue {
        let column = &self.columns[index];
        match value {
            serde_json::Value::Array(items) if column.array => {
                let mut literal = String::new();
                pg_array_literal(&items, &mut literal);
                BindValue::Text(literal)
            }
            other => match json_bind(other) {
                BindValue::Int(n @ (0 | 1)) if column.boolean => BindValue::Bool(n == 1),
                bound => bound,
            },
        }
    }

    /// The record's fields as column values. A field without a column goes into
    /// `extra_column`, or is left out.
    fn row(&self, message: &CanonicalMessage) -> Result<AutoRow, PublisherError> {
        let fields = match serde_json::from_slice(&message.payload) {
            Ok(serde_json::Value::Object(fields)) => fields,
            _ => {
                return Err(PublisherError::NonRetryable(anyhow!(
                    "`columns: auto` needs a JSON object as payload"
                )))
            }
        };
        // Per column: the value, and the field name when it matched only by case.
        let mut cells: Vec<(usize, serde_json::Value, Option<String>)> =
            Vec::with_capacity(fields.len());
        let mut unmapped = serde_json::Map::new();
        for (field, value) in fields {
            let exact = self.by_name.get(&field);
            let Some(&index) = exact.or_else(|| self.by_lower_name.get(&field.to_lowercase()))
            else {
                unmapped.insert(field, value);
                continue;
            };
            let loose = exact.is_none().then_some(field);
            match cells.iter_mut().find(|(i, ..)| *i == index) {
                None => cells.push((index, value, loose)),
                // The field with the column's exact name wins; the other one has no column.
                Some(cell) => match (loose, cell.2.take()) {
                    (None, Some(displaced)) => {
                        unmapped.insert(displaced, std::mem::replace(&mut cell.1, value));
                    }
                    (Some(field), earlier) => {
                        cell.2 = earlier;
                        unmapped.insert(field, value);
                    }
                    (None, None) => cell.1 = value,
                },
            }
        }

        if let (Some(index), false) = (self.extra, unmapped.is_empty()) {
            match cells.iter_mut().find(|(i, ..)| *i == index) {
                None => {
                    let collected = serde_json::Value::Object(std::mem::take(&mut unmapped));
                    cells.push((index, collected, None));
                }
                // The record fills the column itself: its own keys win over collected ones.
                Some((_, own, _)) => {
                    // A SQL source delivers a JSON column as text.
                    if let Some(parsed) = own.as_str().and_then(|s| serde_json::from_str(s).ok()) {
                        *own = parsed;
                    }
                    match own {
                        serde_json::Value::Object(own) => {
                            for (field, value) in std::mem::take(&mut unmapped) {
                                own.entry(field).or_insert(value);
                            }
                        }
                        serde_json::Value::Null => {
                            *own = serde_json::Value::Object(std::mem::take(&mut unmapped))
                        }
                        _ => {}
                    }
                }
            }
        }
        if cells.is_empty() {
            let fields: Vec<&str> = unmapped.keys().map(String::as_str).collect();
            return Err(PublisherError::NonRetryable(anyhow!(
                "no field of the record ({}) is a column of table '{}' (columns: {})",
                fields.join(", "),
                self.table,
                self.column_names()
            )));
        }
        if !unmapped.is_empty() && !self.warned_unknown.swap(true, Ordering::Relaxed) {
            let fields: Vec<&str> = unmapped.keys().map(String::as_str).collect();
            warn!(
                table = %self.table,
                "Fields without a column of the same name are not written: {}. Further occurrences are not logged.",
                fields.join(", ")
            );
        }

        cells.sort_by_key(|(index, ..)| *index);
        let mut columns = Vec::with_capacity(cells.len());
        let mut values = Vec::with_capacity(cells.len());
        for (index, value, _) in cells {
            let nested = value.is_object() || value.is_array();
            let value = self.column_value(index, value);
            // A nested value is JSON text, where the NUL is the escape `\u0000`.
            if matches!(&value, BindValue::Text(s) if s.contains('\0') || (nested && s.contains("\\u0000")))
            {
                return Err(PublisherError::NonRetryable(anyhow!(
                    "field '{}' contains a NUL character, which a SQL text or JSON column cannot store",
                    self.columns[index].name
                )));
            }
            columns.push(index);
            values.push(value);
        }
        Ok(AutoRow { columns, values })
    }

    /// PostgreSQL rejects a statement that updates one row twice: the last record of a key wins.
    fn last_per_key(&self, rows: Vec<AutoRow>) -> Vec<AutoRow> {
        if self.key.is_empty() || self.driver_name != "PostgreSQL" || rows.len() < 2 {
            return rows;
        }
        let key_of = |row: &AutoRow| -> Option<Vec<String>> {
            self.key
                .iter()
                .map(|k| {
                    let at = row.columns.iter().position(|c| c == k)?;
                    bind_key(&row.values[at])
                })
                .collect()
        };
        let mut last: HashMap<Vec<String>, usize> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            if let Some(key) = key_of(row) {
                last.insert(key, i);
            }
        }
        rows.into_iter()
            .enumerate()
            .filter(|(i, row)| key_of(row).is_none_or(|key| last.get(&key) == Some(i)))
            .map(|(_, row)| row)
            .collect()
    }

    fn column_list(&self, columns: &[usize]) -> String {
        let idents: Vec<&str> = columns
            .iter()
            .map(|c| self.columns[*c].ident.as_str())
            .collect();
        idents.join(", ")
    }

    /// The clause that turns the insert into an update of the row with the same key.
    fn upsert_clause(&self, columns: &[usize]) -> String {
        if self.key.is_empty() {
            return String::new();
        }
        let mysql = matches!(self.driver_name.as_str(), "MySQL" | "MariaDB");
        let updates: Vec<String> = columns
            .iter()
            .filter(|c| !self.key.contains(c))
            .map(|c| {
                let ident = &self.columns[*c].ident;
                match mysql {
                    true => format!("{ident} = VALUES({ident})"),
                    false => format!("{ident} = EXCLUDED.{ident}"),
                }
            })
            .collect();
        let first_key = &self.columns[self.key[0]].ident;
        match (mysql, updates.is_empty()) {
            (true, true) => format!(" ON DUPLICATE KEY UPDATE {first_key} = {first_key}"),
            (true, false) => format!(" ON DUPLICATE KEY UPDATE {}", updates.join(", ")),
            (false, true) => format!(" ON CONFLICT ({}) DO NOTHING", self.column_list(&self.key)),
            (false, false) => format!(
                " ON CONFLICT ({}) DO UPDATE SET {}",
                self.column_list(&self.key),
                updates.join(", ")
            ),
        }
    }

    fn insert_sql(&self, columns: &[usize], rows: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} ({}) VALUES ",
            self.table,
            self.column_list(columns)
        );
        let mut index = 1;
        for row in 0..rows {
            sql.push_str(if row > 0 { ", (" } else { "(" });
            for (i, column) in columns.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let placeholder = positional_placeholder(&self.driver_name, index);
                match &self.columns[*column].cast {
                    Some(cast) => sql.push_str(&format!("CAST({placeholder} AS {cast})")),
                    None => sql.push_str(&placeholder),
                }
                index += 1;
            }
            sql.push(')');
        }
        sql.push_str(&self.upsert_clause(columns));
        sql
    }

    /// Consecutive rows naming the same columns share a statement: a column a record does not
    /// name keeps its default on insert and its value on update.
    fn statements(&self, rows: Vec<AutoRow>) -> Vec<(Vec<usize>, Vec<AutoRow>)> {
        let mut runs: Vec<(Vec<usize>, Vec<AutoRow>)> = Vec::new();
        for row in rows {
            let per_statement = (MAX_BINDS / row.columns.len()).max(1);
            match runs.last_mut() {
                Some((columns, run)) if *columns == row.columns && run.len() < per_statement => {
                    run.push(row)
                }
                _ => runs.push((row.columns.clone(), vec![row])),
            }
        }
        runs
    }

    pub(super) async fn send(
        &self,
        pool: &AnyPool,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut rows = Vec::with_capacity(messages.len());
        let mut failed = Vec::new();
        for message in messages {
            match self.row(&message) {
                Ok(row) => rows.push(row),
                Err(e) => failed.push((message, e)),
            }
        }
        let statements = self.statements(rows);
        let retryable = |e: sqlx::Error| PublisherError::Retryable(anyhow!(e));

        if let Some(copy_pool) = &self.copy_pool {
            let mut copies = Vec::with_capacity(statements.len());
            for (columns, run) in statements {
                let stmt = format!(
                    "COPY {} ({}) FROM STDIN WITH (FORMAT text)",
                    self.table,
                    self.column_list(&columns)
                );
                let mut buf = String::new();
                for row in run {
                    for (i, value) in row.values.into_iter().enumerate() {
                        if i > 0 {
                            buf.push('\t');
                        }
                        push_copy_value(&mut buf, value);
                    }
                    buf.push('\n');
                }
                copies.push((stmt, buf));
            }
            match copies.as_slice() {
                [] => {}
                [(stmt, buf)] => {
                    let copier = copy_pool.copy_in_raw(stmt).await;
                    copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await?;
                }
                // Records of several shapes: one transaction, so a retry cannot write some twice.
                _ => {
                    let mut tx = copy_pool.begin().await.map_err(retryable)?;
                    for (stmt, buf) in &copies {
                        let copier = tx.copy_in_raw(stmt).await;
                        copy_in(copier.map_err(classify_sql_error)?, buf.as_bytes()).await?;
                    }
                    tx.commit().await.map_err(retryable)?;
                }
            }
            return Ok(SentBatch::from_failures(failed));
        }

        // One statement is atomic by itself; several need a transaction.
        let mut tx = match statements.len() {
            0 | 1 => None,
            _ => Some(pool.begin().await.map_err(retryable)?),
        };
        for (columns, run) in statements {
            let run = self.last_per_key(run);
            let sql = self.insert_sql(&columns, run.len());
            // Not cached on Postgres: a statement keeps the bind types of its first record.
            let mut query =
                sqlx::query(audited_sql(&sql)).persistent(self.driver_name != "PostgreSQL");
            for value in run.into_iter().flat_map(|row| row.values) {
                query = bind_value(query, value);
            }
            match &mut tx {
                Some(tx) => query.execute(&mut **tx).await,
                None => query.execute(pool).await,
            }
            .map_err(classify_sql_error)?;
        }
        if let Some(tx) = tx {
            tx.commit().await.map_err(retryable)?;
        }
        Ok(SentBatch::from_failures(failed))
    }
}

#[cfg(test)]
mod tests {
    use super::pg_array_literal;

    #[test]
    fn json_array_becomes_a_postgres_array_literal() {
        let items = serde_json::json!([1, null, true, "a \"b\" \\", [2.5], {"k": 1}]);
        let mut out = String::new();
        pg_array_literal(items.as_array().unwrap(), &mut out);
        assert_eq!(out, r#"{1,NULL,t,"a \"b\" \\",{2.5},"{\"k\":1}"}"#);
    }
}
