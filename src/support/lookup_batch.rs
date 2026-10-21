//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Batched lookups. A lookup query whose single token sits inside a list — SQL `key IN (…)`,
//! MongoDB `"key": {"$in": [ … ]}` — answers a whole batch in one round trip: the list element
//! is repeated once per distinct key, and each returned record goes back to the requests whose
//! key it carries.

use anyhow::{anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;

/// Most distinct keys sent in one query; larger batches are split.
pub(crate) const MAX_KEYS_PER_QUERY: usize = 1000;

/// A lookup query in list form, split around the element that holds its only token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ListQuery {
    /// Up to and including the list's opening bracket.
    pub before: String,
    /// The element's text before its token, e.g. `toUInt64(`.
    pub prefix: String,
    /// The token itself, e.g. `${payload:id}`.
    pub token: String,
    /// The element's text after its token, e.g. `::bigint`.
    pub suffix: String,
    /// From the list's closing bracket on.
    pub after: String,
    /// The field or column the returned records are matched on.
    pub key: String,
}

impl ListQuery {
    /// The query with the element repeated for `n` keys; `slot(i)` renders the i-th value.
    pub fn expand(&self, n: usize, mut slot: impl FnMut(usize) -> String) -> String {
        let mut out = self.before.clone();
        for i in 0..n {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&self.prefix);
            out.push_str(&slot(i));
            out.push_str(&self.suffix);
        }
        out.push_str(&self.after);
        out
    }
}

/// Byte ranges of every `${…}` token in `text`.
fn tokens(text: &str) -> anyhow::Result<Vec<(usize, usize)>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find("${").map(|i| from + i) {
        let end = text[start..]
            .find('}')
            .map(|i| start + i + 1)
            .ok_or_else(|| anyhow!("unterminated token at '{}'", &text[start..]))?;
        found.push((start, end));
        from = end;
    }
    Ok(found)
}

/// The innermost `open` bracket enclosing `pos` whose preceding text satisfies `accept`, and its
/// matching `close`. Brackets inside quoted literals are not special-cased.
fn enclosing_list(
    text: &str,
    pos: usize,
    open: u8,
    close: u8,
    accept: impl Fn(&str) -> bool,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for i in (0..pos).rev() {
        if bytes[i] == close {
            depth += 1;
        } else if bytes[i] == open {
            if depth > 0 {
                depth -= 1;
                continue;
            }
            if accept(&text[..i]) {
                let mut depth = 0usize;
                for (j, &b) in bytes.iter().enumerate().skip(i + 1) {
                    if b == open {
                        depth += 1;
                    } else if b == close {
                        if depth == 0 {
                            return Some((i, j));
                        }
                        depth -= 1;
                    }
                }
                return None;
            }
        }
    }
    None
}

/// True when `element` has a comma outside any bracket, i.e. the list has several elements.
fn has_top_level_comma(element: &str) -> bool {
    let mut depth = 0i32;
    element.bytes().any(|b| {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => return true,
            _ => {}
        }
        false
    })
}

fn ends_with_in_keyword(text: &str) -> bool {
    let t = text.trim_end();
    t.len() >= 2
        && t[t.len() - 2..].eq_ignore_ascii_case("in")
        && !t[..t.len() - 2]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The column named left of `IN`: `c.id` and `"Id"` give `id` and `Id`.
fn column_before_in(text: &str) -> anyhow::Result<String> {
    let t = text.trim_end();
    let t = t[..t.len() - 2].trim_end();
    let start = t
        .rfind(|c: char| !(c.is_alphanumeric() || "_.$\"`[]".contains(c)))
        .map_or(0, |i| i + 1);
    let qualified = &t[start..];
    let column = qualified
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim_matches(|c| "\"`[]".contains(c));
    if column.is_empty() || column.eq_ignore_ascii_case("not") {
        bail!("the list form needs a plain column left of `IN (…)`, e.g. `WHERE id IN (${{payload:id}})`");
    }
    Ok(column.to_string())
}

/// A plain `LIMIT n` would cap the whole batch, not each key; `LIMIT n BY …` is per key.
fn has_plain_limit(sql: &str) -> bool {
    let words: Vec<&str> = sql.split_whitespace().collect();
    words.iter().enumerate().any(|(i, w)| {
        w.eq_ignore_ascii_case("limit")
            && words
                .get(i + 1)
                .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
            && !words
                .get(i + 2)
                .is_some_and(|b| b.eq_ignore_ascii_case("by"))
    })
}

/// True when `element` is a subquery (`SELECT …` / `WITH …`), not a value list.
fn starts_subquery(element: &str) -> bool {
    let t = element.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    ["select", "with"].iter().any(|kw| {
        t.get(..kw.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(kw))
            && !t[kw.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Parses a SQL lookup query. `Ok(None)` when it runs per message (the token is compared
/// directly); `Ok(Some(_))` when the only token sits inside `column IN (…)`.
pub(crate) fn parse_sql(sql: &str) -> anyhow::Result<Option<ListQuery>> {
    let found = tokens(sql)?;
    let list = found.iter().find_map(|&(start, end)| {
        enclosing_list(sql, start, b'(', b')', ends_with_in_keyword).map(|l| (start, end, l))
    });
    let Some((start, end, (open, close))) = list else {
        return Ok(None);
    };
    let element = &sql[open + 1..close];
    if starts_subquery(element) {
        return Ok(None);
    }
    if found.len() > 1 {
        bail!("a lookup query with `IN (${{…}})` must contain no other token");
    }
    if has_top_level_comma(element) {
        bail!(
            "`IN (…)` must hold just the one element with the token, e.g. `IN (${{payload:id}})`"
        );
    }
    if has_plain_limit(sql) {
        bail!("drop `LIMIT` from a lookup query with `IN (${{…}})`: it would cap the whole batch, not each key");
    }
    Ok(Some(ListQuery {
        key: column_before_in(&sql[..open])?,
        before: sql[..=open].to_string(),
        prefix: sql[open + 1..start].to_string(),
        token: sql[start..end].to_string(),
        suffix: sql[end..close].to_string(),
        after: sql[close..].to_string(),
    }))
}

/// The field of `"field": {"$in": [`, given the text up to that `[`.
fn in_field(before: &str) -> Option<String> {
    before
        .trim_end()
        .strip_suffix(':')
        .and_then(|b| b.trim_end().strip_suffix("\"$in\""))
        .and_then(|b| b.trim_end().strip_suffix('{'))
        .and_then(|b| b.trim_end().strip_suffix(':'))
        .and_then(|b| b.trim_end().strip_suffix('"'))
        .and_then(|b| b.rfind('"').map(|i| b[i + 1..].to_string()))
        .filter(|f| !f.is_empty())
}

/// Parses a MongoDB `find` template. `Ok(None)` when it runs per message; `Ok(Some(_))` when
/// the only token sits inside `"field": {"$in": [ … ]}`.
pub(crate) fn parse_mongodb(filter: &str) -> anyhow::Result<Option<ListQuery>> {
    let found = tokens(filter)?;
    let is_in = |before: &str| {
        before
            .trim_end()
            .strip_suffix(':')
            .is_some_and(|b| b.trim_end().ends_with("\"$in\""))
    };
    // The expression `{"$in": [value, array]}` (two elements, or under an operator such as
    // `$expr`) tests one value; it is not a key list and runs per message.
    let key_list = |(open, close): (usize, usize)| {
        !has_top_level_comma(&filter[open + 1..close])
            && !in_field(&filter[..open]).is_some_and(|f| f.starts_with('$'))
    };
    let list = found.iter().find_map(|&(start, end)| {
        enclosing_list(filter, start, b'[', b']', is_in)
            .filter(|&l| key_list(l))
            .map(|l| (start, end, l))
    });
    let Some((start, end, (open, close))) = list else {
        return Ok(None);
    };
    if found.len() > 1 {
        bail!("a `find` or `aggregate_query` with `$in: [${{…}}]` must contain no other token");
    }
    let field = in_field(&filter[..open])
        .ok_or_else(|| anyhow!("`$in: [${{…}}]` must be the whole condition of one field, e.g. {{\"_id\": {{\"$in\": [${{payload:id}}]}}}}"))?;
    Ok(Some(ListQuery {
        key: field,
        before: filter[..=open].to_string(),
        prefix: filter[open + 1..start].to_string(),
        token: filter[start..end].to_string(),
        suffix: filter[end..close].to_string(),
        after: filter[close..].to_string(),
    }))
}

/// The comparable form of a key: strings as-is, other scalars and objects as compact JSON, so
/// `"7" ` and `7` match. `None` for null.
pub(crate) fn key_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The value at the dotted `path` of a record.
pub(crate) fn field<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(record, |v, k| v.get(k))
}

/// Indices of the first request for each distinct, non-null key.
pub(crate) fn distinct(keys: &[Option<String>]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    keys.iter()
        .enumerate()
        .filter_map(|(i, k)| k.as_ref().filter(|k| seen.insert(*k)).map(|_| i))
        .collect()
}

/// One answer per request: the record whose `key` field equals the request's key. The first
/// record for a key wins. Keys match as exact strings (see [`key_of`]): the selected column must
/// render like the payload key, or its row counts as not found (e.g. `7.0` or a `::text` cast
/// of a padded value never matches `7`).
pub(crate) fn answer(
    keys: &[Option<String>],
    records: Vec<Value>,
    key: &str,
) -> anyhow::Result<Vec<Option<Value>>> {
    let mut by_key: HashMap<String, Value> = HashMap::with_capacity(records.len());
    for record in records {
        let k = field(&record, key)
            .ok_or_else(|| anyhow!("lookup: a returned record has no `{key}`; select it, it is how records are matched to messages"))?;
        if let Some(k) = key_of(k) {
            by_key.entry(k).or_insert(record);
        }
    }
    Ok(keys
        .iter()
        .map(|k| k.as_ref().and_then(|k| by_key.get(k).cloned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_compared_token_runs_per_message() {
        let q = "SELECT avg(amount) FROM p WHERE card = ${payload:card} ORDER BY ts DESC LIMIT 20";
        assert_eq!(parse_sql(q).unwrap(), None);
        assert_eq!(
            parse_sql("SELECT * FROM c WHERE id = lower(${payload:id})").unwrap(),
            None
        );
        assert_eq!(
            parse_sql("SELECT * FROM c WHERE id IN (SELECT cid FROM o WHERE ref = ${payload:ref})")
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_token_inside_in_is_a_list_query() {
        let q = parse_sql(
            "SELECT c.id, name FROM customers c WHERE c.id in (${payload:id}::bigint) ORDER BY id",
        )
        .unwrap()
        .unwrap();
        assert_eq!(q.key, "id");
        assert_eq!(q.token, "${payload:id}");
        assert_eq!(
            q.expand(2, |i| format!("${}", i + 1)),
            "SELECT c.id, name FROM customers c WHERE c.id in ($1::bigint, $2::bigint) ORDER BY id"
        );
    }

    #[test]
    fn a_wrapped_element_and_a_quoted_column_are_kept() {
        let q =
            parse_sql(r#"SELECT * FROM t WHERE "Id" IN (toUInt64(${payload:id})) LIMIT 1 BY "Id""#)
                .unwrap()
                .unwrap();
        assert_eq!(q.key, "Id");
        assert_eq!((q.prefix.as_str(), q.suffix.as_str()), ("toUInt64(", ")"));
    }

    #[test]
    fn list_queries_that_cannot_batch_are_rejected() {
        for q in [
            "SELECT * FROM c WHERE id IN (${payload:id}) AND t = ${payload:t}",
            "SELECT * FROM c WHERE id IN (${payload:id}, 3)",
            "SELECT * FROM c WHERE id IN (${payload:id}) LIMIT 1",
            "SELECT * FROM c WHERE (a, b) IN (${payload:id})",
            "SELECT * FROM c WHERE id NOT IN (${payload:id})",
        ] {
            assert!(parse_sql(q).is_err(), "{q}");
        }
    }

    #[test]
    fn a_mongodb_in_filter_is_a_list_query() {
        let q = parse_mongodb(r#"{"customer.id": {"$in": [${payload:id}]}, "active": true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(q.key, "customer.id");
        assert_eq!(
            q.expand(2, |i| i.to_string()),
            r#"{"customer.id": {"$in": [0, 1]}, "active": true}"#
        );
        assert_eq!(parse_mongodb(r#"{"_id": "${payload:id}"}"#).unwrap(), None);
        assert!(parse_mongodb(r#"{"$in": [${payload:id}]}"#).is_err());

        let pipeline =
            r#"[{"$match": {"k": {"$in": ["${payload:k}"]}}}, {"$group": {"_id": "$k"}}]"#;
        assert_eq!(parse_mongodb(pipeline).unwrap().unwrap().key, "k");

        // The `$in` expression tests one value and runs per message, tokens anywhere.
        for per_message in [
            r#"[{"$set": {"hit": {"$cond": [{"$in": ["${payload:tag}", "$tags"]}, 1, 0]}}}]"#,
            r#"{"$expr": {"$in": ["${payload:tag}", "$tags"]}, "k": "${payload:k}"}"#,
            r#"{"$expr": {"$in": ["${payload:tag}"]}}"#,
        ] {
            assert_eq!(parse_mongodb(per_message).unwrap(), None, "{per_message}");
        }
    }

    #[test]
    fn records_are_matched_by_key_across_types() {
        let keys = vec![
            Some("7".to_string()),
            None,
            Some("x".to_string()),
            Some("7".to_string()),
        ];
        let records = vec![
            json!({"id": 7, "n": "a"}),
            json!({"id": 7, "n": "b"}),
            json!({"id": "y"}),
        ];
        let answers = answer(&keys, records, "id").unwrap();
        assert_eq!(
            answers,
            vec![
                Some(json!({"id": 7, "n": "a"})),
                None,
                None,
                Some(json!({"id": 7, "n": "a"}))
            ]
        );
        assert_eq!(distinct(&keys), vec![0, 2]);
        assert!(answer(&keys, vec![json!({"n": 1})], "id").is_err());
    }
}
