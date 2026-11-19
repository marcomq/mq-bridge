//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `lookup_query`: single, list and PostgreSQL batch-function lookups.

use super::*;

/// The raw `lookup_query` a PostgreSQL batch function is generated from.
pub(super) struct PgBatchLookup {
    pub(super) template: String,
    /// Set once the query turns out not to run inside a function; it then runs per message.
    pub(super) disabled: std::sync::atomic::AtomicBool,
    /// `CREATE` and name per token-type combination, built once.
    pub(super) functions: Mutex<PgFunctions>,
}

/// `CREATE` and name of a batch function, keyed by its token types.
pub(super) type PgFunctions = std::collections::HashMap<Vec<&'static str>, Arc<(String, String)>>;

/// Metadata key telling whether a `lookup_query` returned a row.
pub(super) const FOUND_KEY: &str = "sqlx.found";
/// First key of the advisory lock a batch function holds; the second is the query's hash, so
/// only batches of the same `lookup_query` wait for each other instead of deadlocking.
pub(super) const PG_BATCH_LOCK_CLASS: i32 = i32::from_be_bytes(*b"mqbr");

/// A parsed `lookup_query`: one query per message, or one `IN (…)` query per batch.
pub(super) enum SqlLookup {
    Single(String, Vec<ColumnSource>),
    List {
        query: ListQuery,
        source: ColumnSource,
    },
}

impl SqlLookup {
    pub(super) fn parse(sql: &str, driver_name: &str) -> anyhow::Result<Self> {
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
pub(super) fn lookup_writes(sql: &str) -> bool {
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
pub(super) fn bind_key(value: &BindValue) -> Option<String> {
    match value {
        BindValue::Null => None,
        BindValue::Int(i) => Some(i.to_string()),
        BindValue::Float(f) => serde_json::Number::from_f64(*f).map(|n| n.to_string()),
        BindValue::Bool(b) => Some(b.to_string()),
        BindValue::Text(s) => Some(s.clone()),
    }
}

/// A `lookup_query` row as JSON.
pub(super) fn row_json(row: &sqlx::any::AnyRow) -> Result<serde_json::Value, PublisherError> {
    let mut buf = Vec::new();
    JsonRowSchema::from_row(row).encode_row(row, &mut buf);
    serde_json::from_slice(&buf).map_err(|e| PublisherError::NonRetryable(e.into()))
}

/// A batch's bind values as one `jsonb` array, plus each token's PostgreSQL type (`null`
/// when null throughout). `None` when a token's type varies across the batch, or a string
/// holds a NUL, which `jsonb` cannot store.
pub(super) fn pg_batch_args(
    messages: &[&CanonicalMessage],
    sources: &[ColumnSource],
) -> Option<(Vec<&'static str>, String)> {
    let mut types: Vec<Option<&'static str>> = vec![None; sources.len()];
    let mut rows = Vec::with_capacity(messages.len());
    for message in messages {
        let payload = payload_fields(message, sources);
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
pub(super) fn pg_batch_function(
    template: &str,
    types: &[&str],
) -> anyhow::Result<(String, String)> {
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
pub(super) fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// SQLSTATE `3F000` (no `pg_temp` schema yet), or `42883` naming the batch function itself: a
/// connection that has not created it. A `42883` from inside the query is the message's error.
pub(super) fn pg_function_missing(e: &sqlx::Error, name: &str) -> bool {
    e.as_database_error()
        .is_some_and(|d| match d.code().as_deref() {
            Some("3F000") => true,
            Some("42883") => d.message().contains(name),
            _ => false,
        })
}

pub(super) enum PgBatchError {
    /// The query cannot run inside the generated function.
    Unsupported(anyhow::Error),
    Query(sqlx::Error),
    Decode(PublisherError),
}

/// What a failed batch-function call means for the batch.
pub(super) enum PgVerdict {
    /// Use the per-key transactions from now on.
    Disable(String),
    /// A message was rejected: redo the batch one message at a time.
    PerMessage,
    Fail(PublisherError),
}

pub(super) fn pg_verdict(e: PgBatchError) -> PgVerdict {
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

pub(super) fn lookup_error(e: sqlx::Error) -> PublisherError {
    if is_permanent_decode_error(&e) {
        PublisherError::NonRetryable(anyhow!(
            "lookup_query returned a column the SQL `Any` driver cannot decode: {e}. \
             Cast it in the query, e.g. `amount::text AS amount`."
        ))
    } else {
        classify_sql_error(e)
    }
}

impl SqlxPublisher {
    pub(super) fn pg_batch_active(&self) -> Option<&PgBatchLookup> {
        use std::sync::atomic::Ordering;
        self.pg_batch
            .as_ref()
            .filter(|pg| !pg.disabled.load(Ordering::Relaxed))
    }

    /// Runs `lookup_query` and answers with its first row, or an empty payload.
    pub(super) async fn lookup_one(
        &self,
        message: &CanonicalMessage,
    ) -> Result<Sent, PublisherError> {
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
        response.set_metadata(FOUND_KEY, found.to_string());
        Ok(Sent::Response(response))
    }

    /// Runs a writing lookup batch in order in one transaction that cannot deadlock: PostgreSQL's
    /// batch function holds an advisory lock per query, SQLite takes its write lock up front.
    /// `None` otherwise: one autocommit statement per message.
    pub(super) async fn lookup_writing_batch(
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
    pub(super) async fn lookup_redo(
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
    pub(super) async fn lookup_in_transaction(
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
    pub(super) async fn lookup_one_by_one(
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
    pub(super) async fn pg_batch_rows(
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
    pub(super) async fn call_pg_batch(
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
    pub(super) async fn lookup_list(
        &self,
        query: &ListQuery,
        source: &ColumnSource,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let values: Vec<BindValue> = messages
            .iter()
            .map(|m| resolve_source(m, source, &payload_fields(m, std::slice::from_ref(source))))
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
            if let Some(sql) = self.pg_any_sql(query, chunk.iter().map(|&i| &values[i])) {
                let literal = lookup_batch::pg_array_literal(
                    chunk.iter().filter_map(|&i| keys[i].as_deref()),
                );
                let rows = sqlx::query(audited_sql(&sql))
                    .bind(literal)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(lookup_error)?;
                push_json_rows(&rows, &mut records)?;
                continue;
            }
            let sql = query.expand(chunk.len(), |i| {
                positional_placeholder(&self.driver_name, i + 1)
            });
            // Small lists are cached (one round trip); large ones not, as a Postgres generic plan
            // made on a small table stays a seq scan. sqlx caches by SQL text alone, so the text
            // names the bind types: a statement prepared for ints is never reused for text.
            let persistent = chunk.len() <= 16;
            let sql = if persistent {
                let types: String = chunk.iter().map(|&i| bind_type_tag(&values[i])).collect();
                format!("/* {types} */ {sql}")
            } else {
                sql
            };
            let mut select = sqlx::query(audited_sql(&sql)).persistent(persistent);
            for &i in chunk {
                select = bind_value(select, values[i].clone());
            }
            let rows = select.fetch_all(&self.pool).await.map_err(lookup_error)?;
            push_json_rows(&rows, &mut records)?;
        }
        lookup_batch::answer(&keys, records, &query.key).map_err(PublisherError::NonRetryable)
    }

    /// On Postgres, `IN (…)` as `= ANY($1::type[])`: one cached statement and one round trip
    /// for any number of keys. Without a cast the keys must share a type.
    pub(super) fn pg_any_sql<'a>(
        &self,
        query: &ListQuery,
        mut values: impl Iterator<Item = &'a BindValue>,
    ) -> Option<String> {
        if self.driver_name != "PostgreSQL" {
            return None;
        }
        let first = bind_type_tag(values.next()?);
        if !values.all(|v| bind_type_tag(v) == first) && query.suffix.trim().is_empty() {
            return None;
        }
        query.pg_any(match first {
            'i' => "int8",
            'f' => "float8",
            'b' => "bool",
            _ => "text",
        })
    }
}

pub(super) fn push_json_rows(
    rows: &[sqlx::any::AnyRow],
    records: &mut Vec<serde_json::Value>,
) -> Result<(), PublisherError> {
    let Some(first) = rows.first() else {
        return Ok(());
    };
    let schema = JsonRowSchema::from_row(first);
    let mut buf = Vec::new();
    for row in rows {
        buf.clear();
        schema.encode_row(row, &mut buf);
        records.push(
            serde_json::from_slice(&buf).map_err(|e| PublisherError::NonRetryable(e.into()))?,
        );
    }
    Ok(())
}
