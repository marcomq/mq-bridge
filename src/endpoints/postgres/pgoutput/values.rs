//! Postgres type OID -> JSON Value mapping for text-mode tuple cells.
//!
//! Reference: <https://github.com/postgres/postgres/blob/master/src/include/catalog/pg_type.dat>
//! Only the OIDs that ship with every Postgres install are special-cased;
//! anything else falls back to a JSON string.
//!
//! Ported from faucet-stream (crates/source/postgres-cdc/src/pgoutput,
//! © the faucet-stream authors, dual-licensed Apache-2.0 OR MIT) and adapted
//! to mq-bridge's `anyhow` error type.

use anyhow::anyhow;
use base64::Engine;
use serde_json::Value;

// Selected pg_type built-in OIDs.
const OID_BOOL: u32 = 16;
const OID_BYTEA: u32 = 17;
const OID_INT2: u32 = 21;
const OID_INT4: u32 = 23;
const OID_INT8: u32 = 20;
const OID_FLOAT4: u32 = 700;
const OID_FLOAT8: u32 = 701;
const OID_NUMERIC: u32 = 1700;
const OID_JSON: u32 = 114;
const OID_JSONB: u32 = 3802;
// Types passed through as their Postgres ISO text form (stable under the
// default `DateStyle ISO` / `IntervalStyle`): date/time/timestamp/
// timestamptz/uuid/interval. Downstream consumers don't agree on a single
// binary encoding, and the text form is already ISO-8601-ish, so we keep
// the string verbatim rather than risk a lossy reformat.

/// Map a built-in Postgres *array* type OID to its element type OID, so a
/// `{...}` array literal can be decoded element-by-element instead of being
/// emitted as one opaque string (e.g. `int4[]` → `"{1,2,3}"`).
/// Returns `None` for non-array OIDs.
fn array_element_oid(array_oid: u32) -> Option<u32> {
    Some(match array_oid {
        1000 => OID_BOOL,    // _bool
        1001 => OID_BYTEA,   // _bytea
        1005 => OID_INT2,    // _int2
        1007 => OID_INT4,    // _int4
        1016 => OID_INT8,    // _int8
        1021 => OID_FLOAT4,  // _float4
        1022 => OID_FLOAT8,  // _float8
        1231 => OID_NUMERIC, // _numeric
        199 => OID_JSON,     // _json
        3807 => OID_JSONB,   // _jsonb
        1009 => 25,          // _text → text
        1015 => 1043,        // _varchar → varchar
        1014 => 1042,        // _bpchar → bpchar
        2951 => 2950,        // _uuid → uuid
        1115 => 1114,        // _timestamp
        1185 => 1184,        // _timestamptz
        1182 => 1082,        // _date
        1183 => 1083,        // _time
        _ => return None,
    })
}

/// Decode a text-encoded value with the given column type OID into JSON.
///
/// Unknown OIDs and decode failures both fall back to wrapping the raw text
/// in a JSON string — this is the safest default for a generic CDC
/// connector. We never panic on bad data; the only way this returns `Err` is
/// if a structurally promised invariant is violated (e.g. `OID_BYTEA` text
/// that doesn't start with `\x`).
pub fn text_to_json(type_oid: u32, text: &str) -> anyhow::Result<Value> {
    Ok(match type_oid {
        OID_BOOL => match text {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            other => {
                return Err(anyhow!("pgoutput: bool column has non-t/f text {other:?}"));
            }
        },
        OID_INT2 | OID_INT4 | OID_INT8 => {
            let n: i64 = text
                .parse()
                .map_err(|e| anyhow!("pgoutput: int (oid={type_oid}) parse {text:?}: {e}"))?;
            Value::from(n)
        }
        OID_FLOAT4 | OID_FLOAT8 => match text {
            // JSON has no NaN/Inf. Preserve them as strings so the
            // non-finite value is distinguishable from a SQL NULL, which
            // also maps to `Value::Null`.
            "NaN" => Value::String("NaN".into()),
            "Infinity" => Value::String("Infinity".into()),
            "-Infinity" => Value::String("-Infinity".into()),
            other => {
                let n: f64 = other
                    .parse()
                    .map_err(|e| anyhow!("pgoutput: float parse {text:?}: {e}"))?;
                serde_json::Number::from_f64(n)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        },
        OID_NUMERIC => Value::String(text.into()),
        OID_BYTEA => {
            // Postgres emits bytea in the server's `bytea_output` format: `hex`
            // (default, `\x`-prefixed) or `escape` (octal `\ooo` / `\\`).
            let bytes = match text.strip_prefix("\\x") {
                Some(hex) => hex_decode(hex)?,
                None => bytea_unescape(text)?,
            };
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        OID_JSON | OID_JSONB => serde_json::from_str(text)
            .map_err(|e| anyhow!("pgoutput: json/jsonb parse {text:?}: {e}"))?,
        other => {
            // One-dimensional arrays of a known scalar element type decode
            // into a JSON array; everything else (incl. multi-dimensional
            // arrays, ranges, composites, enums, and unmapped scalars) falls
            // back to the raw Postgres text string.
            match (array_element_oid(other), parse_pg_array(text)) {
                (Some(elem_oid), Some(elements)) => {
                    let mut out = Vec::with_capacity(elements.len());
                    for elem in elements {
                        match elem {
                            Some(s) => out.push(text_to_json(elem_oid, &s)?),
                            None => out.push(Value::Null),
                        }
                    }
                    Value::Array(out)
                }
                _ => Value::String(text.into()),
            }
        }
    })
}

/// Parse a one-dimensional Postgres array literal (`{a,b,"c,d",NULL}`) into a
/// vector of element texts, where `None` marks an unquoted `NULL`. Returns
/// `None` (caller falls back to the raw string) for anything this simple
/// parser doesn't handle: a non-`{...}`-delimited input or a nested
/// (multi-dimensional) array.
fn parse_pg_array(text: &str) -> Option<Vec<Option<String>>> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
        return None;
    }
    let inner = &text[1..text.len() - 1];
    if inner.is_empty() {
        return Some(Vec::new());
    }

    let mut out: Vec<Option<String>> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut quoted = false; // current element was quoted (so "NULL" != NULL)
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '\\' => {
                    // Escaped next char (\" or \\) is taken literally.
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                }
                '"' => in_quotes = false,
                _ => cur.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                quoted = true;
            }
            // A nested array — bail out, let the caller keep the raw string.
            '{' => return None,
            ',' => {
                out.push(finish_array_element(&cur, quoted));
                cur.clear();
                quoted = false;
            }
            _ => cur.push(c),
        }
    }
    if in_quotes {
        return None; // unterminated quote — malformed, fall back to string
    }
    out.push(finish_array_element(&cur, quoted));
    Some(out)
}

/// An unquoted `NULL` token is a SQL NULL; anything else (incl. a quoted
/// `"NULL"`) is the literal text.
fn finish_array_element(raw: &str, quoted: bool) -> Option<String> {
    if !quoted && raw == "NULL" {
        None
    } else {
        Some(raw.to_owned())
    }
}

fn hex_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(anyhow!("pgoutput: bytea hex has odd length: {s:?}"));
    }
    let nibble = |b: u8| -> anyhow::Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(anyhow!("pgoutput: bytea hex {s:?}: invalid hex digit")),
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

/// Decode Postgres `bytea_output = escape` text into raw bytes: `\\` is a
/// literal backslash, `\ooo` is an octal byte, and any other byte is literal.
fn bytea_unescape(s: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(d0 @ b'0'..=b'7') => {
                let (Some(d1 @ b'0'..=b'7'), Some(d2 @ b'0'..=b'7')) =
                    (bytes.get(i + 2).copied(), bytes.get(i + 3).copied())
                else {
                    return Err(anyhow!(
                        "pgoutput: bytea escape {s:?}: malformed octal sequence"
                    ));
                };
                let val = (u16::from(d0 - b'0') << 6)
                    | (u16::from(d1 - b'0') << 3)
                    | u16::from(d2 - b'0');
                let val = u8::try_from(val).map_err(|_| {
                    anyhow!("pgoutput: bytea escape {s:?}: octal value out of range")
                })?;
                out.push(val);
                i += 4;
            }
            _ => {
                return Err(anyhow!(
                    "pgoutput: bytea escape {s:?}: invalid escape sequence"
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bool_t_and_f() {
        assert_eq!(text_to_json(OID_BOOL, "t").unwrap(), json!(true));
        assert_eq!(text_to_json(OID_BOOL, "f").unwrap(), json!(false));
        assert!(text_to_json(OID_BOOL, "yes").is_err());
    }

    #[test]
    fn integer_types() {
        assert_eq!(text_to_json(OID_INT2, "32000").unwrap(), json!(32000));
        assert_eq!(text_to_json(OID_INT4, "-1").unwrap(), json!(-1));
        assert_eq!(
            text_to_json(OID_INT8, "9223372036854775807").unwrap(),
            json!(9223372036854775807_i64)
        );
        assert!(text_to_json(OID_INT4, "abc").is_err());
    }

    #[test]
    fn floats() {
        assert_eq!(text_to_json(OID_FLOAT8, "3.5").unwrap(), json!(3.5));
        // NaN/Inf are preserved as strings (distinct from a NULL → Value::Null).
        assert_eq!(text_to_json(OID_FLOAT8, "NaN").unwrap(), json!("NaN"));
        assert_eq!(
            text_to_json(OID_FLOAT4, "Infinity").unwrap(),
            json!("Infinity")
        );
        assert_eq!(
            text_to_json(OID_FLOAT8, "-Infinity").unwrap(),
            json!("-Infinity")
        );
    }

    #[test]
    fn int_array_decodes_to_json_array() {
        assert_eq!(text_to_json(1007, "{1,2,3}").unwrap(), json!([1, 2, 3]));
        assert_eq!(text_to_json(1007, "{}").unwrap(), json!([]));
        assert_eq!(text_to_json(1016, "{-9,0,9}").unwrap(), json!([-9, 0, 9]));
    }

    #[test]
    fn text_array_handles_quotes_nulls_and_commas() {
        assert_eq!(
            text_to_json(1009, r#"{a,"b,c",NULL,"NULL"}"#).unwrap(),
            json!(["a", "b,c", null, "NULL"])
        );
        // Escaped quote inside a quoted element.
        assert_eq!(
            text_to_json(1009, r#"{"he said \"hi\""}"#).unwrap(),
            json!(["he said \"hi\""])
        );
    }

    #[test]
    fn bool_array_decodes() {
        assert_eq!(
            text_to_json(1000, "{t,f,t}").unwrap(),
            json!([true, false, true])
        );
    }

    #[test]
    fn multidimensional_array_falls_back_to_string() {
        assert_eq!(
            text_to_json(1007, "{{1,2},{3,4}}").unwrap(),
            json!("{{1,2},{3,4}}")
        );
    }

    #[test]
    fn numeric_kept_as_string() {
        assert_eq!(
            text_to_json(OID_NUMERIC, "12345.67890").unwrap(),
            json!("12345.67890")
        );
    }

    #[test]
    fn bytea_base64() {
        // \xDEADBEEF -> base64 "3q2+7w=="
        assert_eq!(
            text_to_json(OID_BYTEA, "\\xDEADBEEF").unwrap(),
            json!("3q2+7w==")
        );
        assert!(text_to_json(OID_BYTEA, "\\xZZ").is_err()); // not hex
    }

    #[test]
    fn bytea_escape_format() {
        // bytea_output = escape: printable bytes literal, octal \ooo, and \\.
        // "\336\255\276\357" is the escape form of \xDEADBEEF -> "3q2+7w==".
        assert_eq!(
            text_to_json(OID_BYTEA, "\\336\\255\\276\\357").unwrap(),
            json!("3q2+7w==")
        );
        // Printable ASCII passes through: "abc" -> base64 "YWJj".
        assert_eq!(text_to_json(OID_BYTEA, "abc").unwrap(), json!("YWJj"));
        // A literal backslash is escaped as "\\".
        assert_eq!(text_to_json(OID_BYTEA, "a\\\\b").unwrap(), json!("YVxi"));
        // Malformed octal / dangling escape is a clear error, not raw text.
        assert!(text_to_json(OID_BYTEA, "\\12").is_err());
        assert!(text_to_json(OID_BYTEA, "\\z").is_err());
    }

    #[test]
    fn json_columns_parsed() {
        assert_eq!(
            text_to_json(OID_JSON, r#"{"a":1}"#).unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            text_to_json(OID_JSONB, r#"[1,2,3]"#).unwrap(),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn unknown_oid_falls_back_to_string() {
        assert_eq!(
            text_to_json(99999, "2026-05-17 12:34:56+00").unwrap(),
            json!("2026-05-17 12:34:56+00")
        );
    }

    #[test]
    fn int2_and_int8_arrays_decode() {
        // _int2 (1005) → int2 elements.
        assert_eq!(text_to_json(1005, "{1,2,3}").unwrap(), json!([1, 2, 3]));
        // _int8 (1016) is already covered elsewhere; assert _int2 negative too.
        assert_eq!(text_to_json(1005, "{-7,7}").unwrap(), json!([-7, 7]));
    }

    #[test]
    fn float_arrays_decode() {
        // _float4 (1021) and _float8 (1022) → JSON numbers.
        assert_eq!(text_to_json(1021, "{1.5,2.5}").unwrap(), json!([1.5, 2.5]));
        assert_eq!(
            text_to_json(1022, "{3.25,-4.75}").unwrap(),
            json!([3.25, -4.75])
        );
    }

    #[test]
    fn numeric_array_keeps_elements_as_strings() {
        // _numeric (1231) → each element stays a string (NUMERIC is OID_NUMERIC).
        assert_eq!(
            text_to_json(1231, "{1.10,2.20}").unwrap(),
            json!(["1.10", "2.20"])
        );
    }

    #[test]
    fn bytea_array_decodes_to_base64_elements() {
        // _bytea (1001) → element OID_BYTEA → base64. \xDEADBEEF → "3q2+7w==".
        assert_eq!(
            text_to_json(1001, "{\\xDEADBEEF}").unwrap(),
            json!(["3q2+7w=="])
        );
    }

    #[test]
    fn json_and_jsonb_arrays_decode_elements() {
        // _json (199) and _jsonb (3807) → element OID_JSON/JSONB → parsed JSON.
        // The element values are quoted so the embedded braces are not treated
        // as a nested array by the array parser.
        assert_eq!(
            text_to_json(199, r#"{"{\"a\":1}"}"#).unwrap(),
            json!([{"a": 1}])
        );
        assert_eq!(text_to_json(3807, r#"{"[1,2]"}"#).unwrap(), json!([[1, 2]]));
    }

    #[test]
    fn varchar_bpchar_uuid_arrays_decode_to_strings() {
        // _varchar (1015), _bpchar (1014), _uuid (2951) all map to string
        // element types and pass through verbatim.
        assert_eq!(text_to_json(1015, "{a,b}").unwrap(), json!(["a", "b"]));
        assert_eq!(text_to_json(1014, "{x,y}").unwrap(), json!(["x", "y"]));
        assert_eq!(
            text_to_json(2951, "{11111111-1111-1111-1111-111111111111}").unwrap(),
            json!(["11111111-1111-1111-1111-111111111111"])
        );
    }

    #[test]
    fn datetime_arrays_decode_to_strings() {
        // _timestamp (1115), _timestamptz (1185), _date (1182), _time (1183)
        // all map to text-passthrough element types. Commas-free literals so
        // the simple parser splits cleanly.
        assert_eq!(
            text_to_json(1115, r#"{"2026-05-17 12:34:56"}"#).unwrap(),
            json!(["2026-05-17 12:34:56"])
        );
        assert_eq!(
            text_to_json(1185, r#"{"2026-05-17 12:34:56+00"}"#).unwrap(),
            json!(["2026-05-17 12:34:56+00"])
        );
        assert_eq!(
            text_to_json(1182, "{2026-05-17}").unwrap(),
            json!(["2026-05-17"])
        );
        assert_eq!(
            text_to_json(1183, "{12:34:56}").unwrap(),
            json!(["12:34:56"])
        );
    }

    #[test]
    fn float_array_element_parse_error_propagates() {
        // A float array whose element is not parseable surfaces the float
        // parse error rather than falling back to a string.
        let err = text_to_json(1022, "{not_a_float}").unwrap_err();
        assert!(err.to_string().contains("float parse"), "{err}");
    }

    #[test]
    fn json_array_element_parse_error_propagates() {
        // A _json array whose element is not valid JSON surfaces the json
        // parse error.
        let err = text_to_json(199, "{notjson}").unwrap_err();
        assert!(err.to_string().contains("json/jsonb parse"), "{err}");
    }

    #[test]
    fn array_oid_with_non_brace_text_falls_back_to_string() {
        // An array OID (1007 _int4) whose text is not `{...}`-delimited makes
        // parse_pg_array return None, so text_to_json falls back to wrapping
        // the raw text in a string.
        assert_eq!(
            text_to_json(1007, "not-an-array").unwrap(),
            json!("not-an-array")
        );
    }

    #[test]
    fn array_with_unterminated_quote_falls_back_to_string() {
        // An unterminated quote makes parse_pg_array return None, so the whole
        // literal is kept as a raw string.
        let raw = r#"{"unterminated}"#;
        assert_eq!(text_to_json(1009, raw).unwrap(), json!(raw));
    }

    #[test]
    fn bytea_odd_length_hex_errors() {
        // \x followed by an odd number of hex digits is rejected.
        let err = text_to_json(OID_BYTEA, "\\xABC").unwrap_err();
        assert!(err.to_string().contains("odd length"), "{err}");
    }
}
