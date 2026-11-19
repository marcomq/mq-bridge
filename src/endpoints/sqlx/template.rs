//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! SQL insert templates: `VALUES` tuples, placeholders and binding message fields.

use super::*;

/// Checks if a SQL query string contains a `(payload)` clause, ignoring case and whitespace.
/// Locate the top-level `VALUES` keyword that introduces the insert tuple.
///
/// Scans at parenthesis-depth 0 and returns the *first* such keyword, so a
/// `values` column inside the column list (which sits at depth 1) and a MySQL
/// `VALUES(col)` reference in an `ON DUPLICATE KEY UPDATE` suffix (which follows
/// the real keyword) are both ignored. Word-boundary checks avoid matching
/// substrings like `myvalues`. Returns `None` when no top-level keyword is
/// found, letting the caller fall back to iterative inserts.
pub(super) fn find_top_level_values(sql: &str) -> Option<usize> {
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

pub(super) fn contains_payload_clause(query: &str) -> bool {
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

pub(super) fn audited_sql(sql: &str) -> AssertSqlSafe<&str> {
    AssertSqlSafe(sql)
}

/// Where a single bound value in a token-based `insert_query` comes from.
/// Resolved per-row; never falls back between the two sources.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ColumnSource {
    /// `${metadata:<key>}` — `message.get_metadata(key)`, else NULL.
    Metadata(String),
    /// `${payload:<field>}` — top-level JSON field of the payload, else NULL.
    Payload(String),
}

/// A value ready to be bound, preserving JSON scalar type so strict dialects
/// (Postgres/MSSQL) accept numeric/bool columns instead of erroring on text.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum BindValue {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

/// Driver-specific positional placeholder for the given 1-based index.
pub(super) fn positional_placeholder(driver_name: &str, index: usize) -> String {
    match driver_name {
        "PostgreSQL" => format!("${}", index),
        "Microsoft SQL Server" => format!("@p{}", index),
        _ => "?".to_string(),
    }
}

/// True when `sql` still binds a value: an unrewritten `${…}` token, or a positional
/// placeholder in any of the driver spellings (`?`, `$N`, `@pN`).
pub(super) fn binds_after_values(sql: &str) -> bool {
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

/// A piece of a VALUES tuple: SQL kept verbatim, or the 1-based placeholder of a row.
#[derive(Debug, PartialEq)]
pub(super) enum TuplePart<'a> {
    Sql(&'a str),
    Bind(usize),
}

/// Splits `tuple` (including its parens) at its placeholders, so the batch path can repeat
/// it per row with the user's expressions intact. `None` when it cannot be repeated safely:
/// its placeholders are not exactly `1..=per_row`, or it holds SQL the scan does not follow
/// (dollar quoting, `E'…'` strings, comments).
pub(super) fn tuple_template<'a>(
    tuple: &'a str,
    driver_name: &str,
    per_row: usize,
) -> Option<Vec<TuplePart<'a>>> {
    let tuple = tuple.trim();
    if !(tuple.starts_with('(') && tuple.ends_with(')')) {
        return None;
    }
    let bytes = tuple.as_bytes();
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let mut parts = Vec::new();
    let mut binds = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        let (index, len) = match (bytes[i], driver_name) {
            (b'\'' | b'"', _) => {
                if bytes[i] == b'\'' && i > 0 && bytes[i - 1].eq_ignore_ascii_case(&b'e') {
                    return None;
                }
                let close = tuple[i + 1..].find(bytes[i] as char)?;
                i += close + 2;
                continue;
            }
            (b'-', _) if bytes.get(i + 1) == Some(&b'-') => return None,
            (b'/', _) if bytes.get(i + 1) == Some(&b'*') => return None,
            (b'$', "PostgreSQL") => match digits(i + 1) {
                0 => return None,
                n => (tuple[i + 1..i + 1 + n].parse().ok()?, 1 + n),
            },
            (b'@', "Microsoft SQL Server") if bytes.get(i + 1) == Some(&b'p') => {
                match digits(i + 2) {
                    0 => (0, 0),
                    n => (tuple[i + 2..i + 2 + n].parse().ok()?, 2 + n),
                }
            }
            (b'?', "PostgreSQL" | "Microsoft SQL Server") => (0, 0),
            (b'?', _) => (binds.len() + 1, 1),
            _ => (0, 0),
        };
        if len == 0 {
            i += 1;
            continue;
        }
        parts.push(TuplePart::Sql(&tuple[start..i]));
        parts.push(TuplePart::Bind(index));
        binds.push(index);
        i += len;
        start = i;
    }
    parts.push(TuplePart::Sql(&tuple[start..]));
    binds.sort_unstable();
    (binds == (1..=per_row).collect::<Vec<_>>()).then_some(parts)
}

/// Appends one row of `template`, its placeholders shifted by `offset`.
pub(super) fn render_tuple(
    template: &[TuplePart<'_>],
    driver_name: &str,
    offset: usize,
    out: &mut String,
) {
    for part in template {
        match part {
            TuplePart::Sql(sql) => out.push_str(sql),
            TuplePart::Bind(index) => {
                out.push_str(&positional_placeholder(driver_name, offset + index))
            }
        }
    }
}

/// Parse `${metadata:<key>}` / `${payload:<field>}` tokens out of an `insert_query`,
/// rewriting each into a driver-appropriate positional placeholder assigned a running
/// 1-based index in encounter order. Returns the rewritten query and the ordered
/// sources (`sources[i]` resolves the value for the i-th placeholder). An empty
/// `Vec` means the query had no tokens → legacy single-payload-bind mode.
pub(super) fn parse_insert_template(
    query: &str,
    driver_name: &str,
) -> anyhow::Result<(String, Vec<ColumnSource>)> {
    parse_template_with(query, |index| positional_placeholder(driver_name, index))
}

/// [`parse_insert_template`] with the text each 1-based token index is rewritten to.
pub(super) fn parse_template_with(
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

/// The payload fields `sources` read, parsed alone; `None` when none reads the payload.
pub(super) fn payload_fields(
    msg: &CanonicalMessage,
    sources: &[ColumnSource],
) -> Option<serde_json::Value> {
    let reads = |key: &str| {
        (sources.iter()).any(|s| matches!(s, ColumnSource::Payload(field) if field == key))
    };
    if !sources
        .iter()
        .any(|s| matches!(s, ColumnSource::Payload(_)))
    {
        return None;
    }
    pick_fields(&msg.payload, reads)
}

/// Resolve a `ColumnSource` for one message into a typed `BindValue`. `payload_json`
/// holds at least the payload fields the source reads (`None` if not valid JSON). No fallback: a
/// `Payload` source never consults metadata and vice versa; an unresolvable source
/// yields `Null`.
pub(super) fn resolve_source(
    msg: &CanonicalMessage,
    source: &ColumnSource,
    payload_json: &Option<serde_json::Value>,
) -> BindValue {
    match source {
        ColumnSource::Metadata(key) => match msg.get_metadata(key) {
            Some(v) => BindValue::Text(v.to_owned()),
            None => BindValue::Null,
        },
        ColumnSource::Payload(field) => match payload_json.as_ref().and_then(|v| v.get(field)) {
            Some(serde_json::Value::String(s)) => BindValue::Text(s.clone()),
            Some(serde_json::Value::Bool(b)) => BindValue::Bool(*b),
            Some(serde_json::Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    BindValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    if n.is_u64() {
                        static WARNED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            warn!(
                                "`${{payload:{field}}}` is an integer above the BIGINT range and binds as a rounded float. Use `columns: auto` to keep it exact. Further occurrences are not logged."
                            );
                        }
                    }
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

pub(super) type AnyQuery<'q> = sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments>;

/// Bind one typed value, mapping `Null` to a typed SQL NULL.
pub(super) fn bind_value(query: AnyQuery<'_>, value: BindValue) -> AnyQuery<'_> {
    match value {
        BindValue::Null => query.bind(None::<String>),
        BindValue::Int(i) => query.bind(i),
        BindValue::Float(f) => query.bind(f),
        BindValue::Bool(b) => query.bind(b),
        BindValue::Text(s) => query.bind(s),
    }
}

pub(super) fn bind_type_tag(value: &BindValue) -> char {
    match value {
        BindValue::Null => 'n',
        BindValue::Int(_) => 'i',
        BindValue::Float(_) => 'f',
        BindValue::Bool(_) => 'b',
        BindValue::Text(_) => 't',
    }
}

/// Reject a string carrying an embedded NUL. `text`/`varchar` cannot represent one on
/// PostgreSQL or MySQL, and the driver dropped the bind instead of failing — the row
/// landed with a silent SQL `NULL` while the route reported a clean success.
pub(super) fn reject_embedded_nul(
    source: &ColumnSource,
    value: &BindValue,
) -> Result<(), PublisherError> {
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

/// Parse the payload fields once, then resolve+bind every column source for one row.
pub(super) fn bind_message_sources<'q>(
    mut query: AnyQuery<'q>,
    msg: &CanonicalMessage,
    sources: &[ColumnSource],
) -> Result<AnyQuery<'q>, PublisherError> {
    let payload_json = payload_fields(msg, sources);
    for source in sources {
        let value = resolve_source(msg, source, &payload_json);
        reject_embedded_nul(source, &value)?;
        query = bind_value(query, value);
    }
    Ok(query)
}
