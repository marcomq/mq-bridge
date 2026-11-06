/// Top-level object fields borrowed from a JSON payload, with each value left as raw JSON.
pub(crate) struct RawPairs<'a>(pub(crate) Vec<(&'a str, &'a serde_json::value::RawValue)>);

impl<'de> serde::Deserialize<'de> for RawPairs<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PairVisitor;

        impl<'de> serde::de::Visitor<'de> for PairVisitor {
            type Value = RawPairs<'de>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<RawPairs<'de>, A::Error> {
                let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(8));
                while let Some(entry) =
                    map.next_entry::<&'de str, &'de serde_json::value::RawValue>()?
                {
                    pairs.push(entry);
                }
                Ok(RawPairs(pairs))
            }
        }

        deserializer.deserialize_map(PairVisitor)
    }
}

/// JSON being assembled: text copied as is, or an object whose fields are set by path.
enum Node {
    Raw(Vec<u8>),
    Object(Vec<(String, Node)>),
}

impl Node {
    /// Sets `json` at `path`. `Ok(false)` when an object on the way has escaped keys.
    fn insert(&mut self, path: &[String], json: Vec<u8>, who: &str) -> anyhow::Result<bool> {
        let Some((key, rest)) = path.split_first() else {
            *self = Node::Raw(json);
            return Ok(true);
        };
        if let Node::Raw(raw) = self {
            if raw.first() != Some(&b'{') {
                anyhow::bail!("{who}: cannot set '{key}': its parent is not an object");
            }
            let Ok(RawPairs(pairs)) = serde_json::from_slice::<RawPairs>(raw) else {
                return Ok(false);
            };
            let fields = pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), Node::Raw(v.get().as_bytes().to_vec())))
                .collect();
            *self = Node::Object(fields);
        }
        let Node::Object(fields) = self else {
            unreachable!("raw text was just expanded")
        };
        let at = match fields.iter().position(|(k, _)| k == key) {
            Some(at) => at,
            None => {
                fields.push((key.clone(), Node::Object(Vec::new())));
                fields.len() - 1
            }
        };
        fields[at].1.insert(rest, json, who)
    }

    fn write(&self, out: &mut Vec<u8>) -> anyhow::Result<()> {
        match self {
            Node::Raw(raw) => out.extend_from_slice(raw),
            Node::Object(fields) => {
                out.push(b'{');
                for (i, (key, node)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key)?;
                    out.push(b':');
                    node.write(out)?;
                }
                out.push(b'}');
            }
        }
        Ok(())
    }
}

/// Writes each JSON value at its non-empty path, copying every untouched field verbatim.
/// `who` names the middleware in errors.
pub(crate) fn write_at(
    payload: &[u8],
    values: Vec<(&[String], Vec<u8>)>,
    who: &str,
) -> anyhow::Result<Vec<u8>> {
    if values.is_empty() {
        return Ok(payload.to_vec());
    }
    if let Some(spliced) = splice(payload, &values, who)? {
        return Ok(spliced);
    }
    // Keys with escapes cannot be borrowed; those payloads take the full parse.
    let mut doc: serde_json::Value = serde_json::from_slice(payload)?;
    for (path, json) in values {
        insert_at(&mut doc, path, serde_json::from_slice(&json)?, who)?;
    }
    Ok(serde_json::to_vec(&doc)?)
}

/// A key that is already set keeps its place, as it does in a parsed `Value`.
fn splice(
    payload: &[u8],
    values: &[(&[String], Vec<u8>)],
    who: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(RawPairs(pairs)) = serde_json::from_slice::<RawPairs>(payload) else {
        return Ok(None);
    };
    let mut written: Vec<(&str, Node)> = Vec::with_capacity(values.len());
    for (path, json) in values {
        let (top, rest) = path.split_first().expect("path is non-empty");
        let at = match written.iter().position(|(key, _)| key == top) {
            Some(at) => at,
            None => {
                let node = match pairs.iter().rev().find(|(key, _)| key == top) {
                    Some((_, raw)) if !rest.is_empty() => Node::Raw(raw.get().as_bytes().to_vec()),
                    _ => Node::Object(Vec::new()),
                };
                written.push((top, node));
                written.len() - 1
            }
        };
        if !written[at].1.insert(rest, json.clone(), who)? {
            return Ok(None);
        }
    }

    let mut out = Vec::with_capacity(payload.len() + 128 * written.len());
    let mut placed = vec![false; written.len()];
    out.push(b'{');
    for (key, raw) in &pairs {
        match written.iter().position(|(top, _)| top == key) {
            Some(at) if placed[at] => continue,
            Some(at) => {
                placed[at] = true;
                serde_json::to_writer(&mut out, key)?;
                out.push(b':');
                written[at].1.write(&mut out)?;
            }
            None => {
                out.push(b'"');
                out.extend_from_slice(key.as_bytes());
                out.extend_from_slice(b"\":");
                out.extend_from_slice(raw.get().as_bytes());
            }
        }
        out.push(b',');
    }
    for ((key, node), placed) in written.iter().zip(placed) {
        if placed {
            continue;
        }
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        node.write(&mut out)?;
        out.push(b',');
    }
    out.pop();
    out.push(b'}');
    Ok(Some(out))
}

/// Writes `value` at the dotted, non-empty `path`, creating objects along the way.
fn insert_at(
    root: &mut serde_json::Value,
    path: &[String],
    value: serde_json::Value,
    who: &str,
) -> anyhow::Result<()> {
    use serde_json::{Map, Value};
    let (last, parents) = path.split_last().expect("path is non-empty");
    let mut cur = root;
    for key in parents {
        let Value::Object(map) = cur else {
            anyhow::bail!("{who}: cannot nest under '{key}': it is not an object");
        };
        cur = map
            .entry(key.as_str())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let Value::Object(map) = cur else {
        anyhow::bail!("{who}: cannot set '{last}': its parent is not an object");
    };
    map.insert(last.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dotted: &str) -> Vec<String> {
        dotted.split('.').map(str::to_string).collect()
    }

    /// The old way: parse, insert, serialize.
    fn parsed(payload: &str, values: &[(Vec<String>, &str)]) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(payload).unwrap();
        for (path, json) in values {
            insert_at(&mut doc, path, serde_json::from_str(json).unwrap(), "t").unwrap();
        }
        serde_json::to_string(&doc).unwrap()
    }

    #[test]
    fn splicing_matches_the_parsed_document_including_key_order() {
        type Case<'a> = (&'a str, Vec<(Vec<String>, &'a str)>);
        let cases: [Case; 6] = [
            (r#"{"a":1,"b":2}"#, vec![(path("c"), r#"{"v":1}"#)]),
            (r#"{"a":1,"c":0,"b":2}"#, vec![(path("c"), "null")]),
            (r#"{"a":1,"u":{"x":1},"b":2}"#, vec![(path("u.y"), "true")]),
            (r#"{"a":1}"#, vec![(path("p.q"), "1"), (path("p.r"), "2")]),
            (r#"{"a\"b":1,"c":0}"#, vec![(path("c"), "3")]),
            (r#"{}"#, vec![(path("x"), r#""s""#)]),
        ];
        for (payload, values) in cases {
            let owned = values
                .iter()
                .map(|(p, j)| (p.as_slice(), j.as_bytes().to_vec()));
            let spliced = write_at(payload.as_bytes(), owned.collect(), "t").unwrap();
            assert_eq!(
                String::from_utf8(spliced).unwrap(),
                parsed(payload, &values),
                "{payload}"
            );
        }
    }

    #[test]
    fn untouched_fields_are_copied_verbatim() {
        let payload = br#"{"n":1.50,"s":"a\/b"}"#;
        let out = write_at(payload, vec![(&path("x")[..], b"1".to_vec())], "t");
        assert_eq!(out.unwrap(), br#"{"n":1.50,"s":"a\/b","x":1}"#);
    }

    #[test]
    fn a_parent_that_is_not_an_object_fails_with_the_middleware_name() {
        for payload in [&br#"{"u":3}"#[..], b"[1]"] {
            let err = write_at(payload, vec![(&path("u.y")[..], b"1".to_vec())], "lookup");
            let err = err.unwrap_err().to_string();
            assert!(err.starts_with("lookup: cannot "), "{err}");
        }
    }
}
