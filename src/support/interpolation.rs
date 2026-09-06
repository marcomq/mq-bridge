//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Placeholder interpolation for template bodies (currently used by the `static`
//! endpoint). A template is **compiled once** at endpoint construction into a list
//! of literal/token segments; rendering a message never re-parses the template and
//! parses the payload JSON at most once (only when a `${payload:…}` token exists).
//!
//! ## Token syntax
//!
//! Tokens use the `${namespace:selector}` form (the same convention as the
//! ClickHouse `columns` mapping):
//!
//! | Token | Resolves to |
//! |-------|-------------|
//! | `${payload:a.b.c}` | nested field of the incoming JSON payload (dotted path; array indices allowed) |
//! | `${metadata:key}` | a metadata string value |
//! | `${message:id}` | the message id (canonical UUID string) |
//! | `${gen:uuid}` | a fresh UUID v7 |
//! | `${gen:now}` | current time, RFC3339 UTC |
//! | `${gen:timestamp}` | current time, Unix epoch milliseconds |
//! | `${gen:counter}` | a per-template monotonic counter (starts at 0) |
//! | `${gen:random(1,100)}` | a random integer in `[min, max]` |
//! | `${env:VAR}` | an environment variable, **resolved once at compile time** |
//!
//! To emit a literal `${…}` that is not interpolated, write `$${…}` (only the
//! `$${` sequence is special; a bare `$$` is left untouched). Any `${…}` whose
//! namespace is not one of the above is also emitted verbatim, so existing bodies
//! that happen to contain `${…}` keep their meaning.
//!
//! ## Escaping
//!
//! The escape context is derived **once at compile time** from the body's
//! `content-type`. When it is a JSON type, resolved `payload`/`metadata`/`message`
//! values are JSON-string-escaped so external data can never break the surrounding
//! structure. Append `| raw` to a token (`${payload:x | raw}`) to splice it
//! verbatim instead. `gen`/`env` values are framework/operator-generated and are
//! always inserted verbatim.

use anyhow::{anyhow, bail, Context};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::canonical_message::{format_message_id, CanonicalMessage};

/// How resolved external values are escaped before being spliced into the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeMode {
    /// No escaping: values are inserted verbatim (plain text / unknown content type).
    None,
    /// JSON string escaping: safe to splice into a quoted string in a JSON body.
    Json,
}

impl EscapeMode {
    fn from_content_type(content_type: Option<&str>) -> Self {
        match content_type {
            Some(ct) if ct.to_ascii_lowercase().contains("json") => EscapeMode::Json,
            _ => EscapeMode::None,
        }
    }
}

/// A dynamic value generated fresh on every render (independent of the message).
#[derive(Debug, Clone)]
enum Gen {
    Uuid,
    Now,
    Timestamp,
    Counter,
    Random(i64, i64),
}

/// Where a token's value comes from.
#[derive(Debug, Clone)]
enum Source {
    /// A dotted path into the incoming JSON payload (empty = whole payload).
    Payload(String),
    /// A metadata key.
    Metadata(String),
    /// The message id.
    MessageId,
    /// A per-render generated value.
    Gen(Gen),
}

#[derive(Debug, Clone)]
struct Token {
    source: Source,
    /// `| raw`: bypass the template's escape mode and splice verbatim.
    raw: bool,
}

#[derive(Debug)]
enum Segment {
    Literal(Box<[u8]>),
    Token(Token),
}

/// A body template compiled once, then rendered per message with no re-parsing.
#[derive(Debug)]
pub struct CompiledTemplate {
    segments: Vec<Segment>,
    escape: EscapeMode,
    /// True if any token reads the payload, so it is parsed once per render.
    needs_payload: bool,
    /// The path of the only `${payload:…}` token, when there is exactly one. Rendering then
    /// extracts that one field instead of materializing the whole document.
    single_payload_path: Option<String>,
    /// Backs `${gen:counter}`; shared across clones via the enclosing `Arc`.
    counter: AtomicU64,
    /// Sum of literal byte lengths, used to pre-size the render buffer.
    literal_len: usize,
}

impl CompiledTemplate {
    /// Compile `body` against the escape context implied by `content_type`.
    /// Returns an error for malformed tokens (unknown namespace fields, bad
    /// `gen`/`env` specs, unknown filters) so config mistakes fail at startup.
    pub fn compile(body: &str, content_type: Option<&str>) -> anyhow::Result<Self> {
        let escape = EscapeMode::from_content_type(content_type);
        let mut segments: Vec<Segment> = Vec::new();
        let mut lit = String::new();
        let mut needs_payload = false;

        let bytes = body.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        while i < n {
            // `$${` -> literal `${` (escape a token so it is not interpolated).
            // A bare `$$` is left alone, so existing bodies keep their `$$`.
            if bytes[i] == b'$' && i + 2 < n && bytes[i + 1] == b'$' && bytes[i + 2] == b'{' {
                lit.push_str("${");
                i += 3;
                continue;
            }
            // `${ ... }` -> maybe a token.
            if bytes[i] == b'$' && i + 1 < n && bytes[i + 1] == b'{' {
                if let Some(close) = body[i + 2..].find('}').map(|off| i + 2 + off) {
                    let inner = &body[i + 2..close];
                    match parse_token(inner)? {
                        Parsed::Literal(s) => lit.push_str(&s),
                        Parsed::Verbatim => lit.push_str(&body[i..=close]),
                        Parsed::Token(tok) => {
                            if matches!(tok.source, Source::Payload(_)) {
                                needs_payload = true;
                            }
                            if !lit.is_empty() {
                                segments.push(Segment::Literal(
                                    std::mem::take(&mut lit).into_bytes().into_boxed_slice(),
                                ));
                            }
                            segments.push(Segment::Token(tok));
                        }
                    }
                    i = close + 1;
                    continue;
                }
            }
            // Default: copy one UTF-8 char.
            let ch = body[i..].chars().next().unwrap();
            lit.push(ch);
            i += ch.len_utf8();
        }
        if !lit.is_empty() {
            segments.push(Segment::Literal(lit.into_bytes().into_boxed_slice()));
        }

        let literal_len = segments
            .iter()
            .map(|s| match s {
                Segment::Literal(b) => b.len(),
                Segment::Token(_) => 0,
            })
            .sum();

        let mut payload_paths = segments.iter().filter_map(|s| match s {
            Segment::Token(Token {
                source: Source::Payload(path),
                ..
            }) => Some(path.clone()),
            _ => None,
        });
        let single_payload_path = match (payload_paths.next(), payload_paths.next()) {
            (Some(path), None) => Some(path),
            _ => None,
        };

        Ok(Self {
            segments,
            escape,
            needs_payload,
            single_payload_path,
            counter: AtomicU64::new(0),
            literal_len,
        })
    }

    /// Whether this template contains any tokens at all. Callers can skip
    /// rendering entirely (send the body verbatim) when this is false.
    pub fn is_dynamic(&self) -> bool {
        self.segments.iter().any(|s| matches!(s, Segment::Token(_)))
    }

    /// Whether every token is derived from data that remains stable across message replays.
    pub fn has_only_replay_stable_tokens(&self) -> bool {
        self.segments.iter().all(|segment| match segment {
            Segment::Literal(_) => true,
            Segment::Token(token) => {
                matches!(token.source, Source::Payload(_) | Source::Metadata(_))
            }
        })
    }

    /// Render the template against an optional message. On the source side (no
    /// input message) `payload`/`metadata`/`message` tokens resolve to empty.
    pub fn render(&self, msg: Option<&CanonicalMessage>) -> Vec<u8> {
        self.render_inner(msg, false).unwrap_or_default()
    }

    /// Render, but `None` as soon as a token has no value for this message.
    ///
    /// Use this when the result is a key or an identity rather than a body. [`Self::render`]
    /// substitutes an empty string for a missing selector, so `"${payload:a}-${payload:b}"`
    /// still yields `"x-"` when `b` is absent — a non-empty value that every such message
    /// shares. Only a whole-template check catches that.
    pub fn render_resolved(&self, msg: Option<&CanonicalMessage>) -> Option<Vec<u8>> {
        self.render_inner(msg, true)
    }

    fn render_inner(&self, msg: Option<&CanonicalMessage>, strict: bool) -> Option<Vec<u8>> {
        let payload = self.parse_payload(msg);

        let mut out = Vec::with_capacity(self.literal_len + 16);
        for seg in &self.segments {
            match seg {
                Segment::Literal(b) => out.extend_from_slice(b),
                Segment::Token(tok) => {
                    let value = match self.resolve(tok, msg, &payload) {
                        Some(value) => value,
                        None if strict => return None,
                        None => String::new(),
                    };
                    if tok.raw || self.escape == EscapeMode::None {
                        out.extend_from_slice(value.as_bytes());
                    } else {
                        json_escape_into(&value, &mut out);
                    }
                }
            }
        }
        Some(out)
    }

    /// Parse as little of the payload as the template actually asks for.
    fn parse_payload(&self, msg: Option<&CanonicalMessage>) -> Payload {
        if !self.needs_payload {
            return Payload::None;
        }
        let Some(msg) = msg else {
            return Payload::None;
        };
        match &self.single_payload_path {
            Some(path) => Payload::Field(pick_path(&msg.payload, path)),
            None => match serde_json::from_slice(&msg.payload) {
                Ok(doc) => Payload::Doc(doc),
                Err(_) => Payload::None,
            },
        }
    }

    /// `None` when the token has nothing to resolve against — an absent payload path or
    /// metadata key, or no message at all. A token that resolves to a genuinely empty value
    /// is `Some("")`.
    fn resolve(
        &self,
        tok: &Token,
        msg: Option<&CanonicalMessage>,
        payload: &Payload,
    ) -> Option<String> {
        match &tok.source {
            Source::Payload(path) => match payload {
                Payload::Field(value) => value.as_ref().map(value_to_string),
                Payload::Doc(doc) => walk(doc, path).map(value_to_string),
                Payload::None => None,
            },
            Source::Metadata(key) => msg.and_then(|m| m.metadata.get(key)).cloned(),
            Source::MessageId => msg.map(|m| format_message_id(m.message_id)),
            Source::Gen(gen) => Some(match gen {
                Gen::Uuid => format_message_id(fast_uuid_v7::gen_id()),
                Gen::Now => rfc3339_utc_now(),
                Gen::Timestamp => unix_millis().to_string(),
                Gen::Counter => self.counter.fetch_add(1, Ordering::Relaxed).to_string(),
                Gen::Random(min, max) => {
                    let span = (*max as i128 - *min as i128 + 1) as u128;
                    (*min as i128 + (rand::random::<u64>() as u128 % span) as i128).to_string()
                }
            }),
        }
    }
}

enum Parsed {
    /// A resolved constant (e.g. an env var) to fold into the surrounding literal.
    Literal(String),
    /// Not a recognized token; emit the original `${…}` text verbatim.
    Verbatim,
    /// A per-message token.
    Token(Token),
}

/// Parse the text between `${` and `}` into a token, a folded literal, or a
/// verbatim marker.
fn parse_token(inner: &str) -> anyhow::Result<Parsed> {
    // Split off an optional `| filter`. Validate it only after the namespace is
    // recognized, so an unknown namespace stays verbatim even with a bogus filter.
    let (spec, filter) = match inner.split_once('|') {
        Some((spec, filter)) => (spec.trim(), Some(filter.trim())),
        None => (inner.trim(), None),
    };

    let (ns, selector) = match spec.split_once(':') {
        Some((ns, sel)) => (ns.trim(), sel.trim()),
        None => (spec, ""),
    };

    let source = match ns {
        "payload" => Source::Payload(selector.to_string()),
        "metadata" => Source::Metadata(selector.to_string()),
        "message" => match selector {
            "id" => Source::MessageId,
            other => bail!("unknown message field '${{message:{other}}}' (only 'id' is supported)"),
        },
        "gen" => Source::Gen(parse_gen(selector)?),
        "env" => {
            let value = std::env::var(selector).with_context(|| {
                format!("environment variable '{selector}' for '${{{inner}}}' is not set")
            })?;
            validate_filter(filter, inner)?;
            return Ok(Parsed::Literal(value));
        }
        // Unknown namespace: leave the text untouched for backward compatibility,
        // regardless of any filter.
        _ => return Ok(Parsed::Verbatim),
    };

    let raw = validate_filter(filter, inner)?;
    Ok(Parsed::Token(Token { source, raw }))
}

/// Validate the optional `| filter` for a recognized namespace. Only `raw` is
/// supported today; returns whether it was set.
fn validate_filter(filter: Option<&str>, inner: &str) -> anyhow::Result<bool> {
    match filter {
        None => Ok(false),
        Some("raw") => Ok(true),
        Some(other) => {
            bail!("unknown token filter '{other}' in '${{{inner}}}' (only 'raw' is supported)")
        }
    }
}

fn parse_gen(spec: &str) -> anyhow::Result<Gen> {
    match spec {
        "uuid" => Ok(Gen::Uuid),
        "now" => Ok(Gen::Now),
        "timestamp" => Ok(Gen::Timestamp),
        "counter" => Ok(Gen::Counter),
        _ => {
            let args = spec
                .strip_prefix("random")
                .map(str::trim)
                .and_then(|s| s.strip_prefix('('))
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| anyhow!("unknown gen token '${{gen:{spec}}}'"))?;
            let (min, max) = args
                .split_once(',')
                .ok_or_else(|| anyhow!("gen:random expects 'random(min,max)'"))?;
            let min: i64 = min
                .trim()
                .parse()
                .context("gen:random min is not an integer")?;
            let max: i64 = max
                .trim()
                .parse()
                .context("gen:random max is not an integer")?;
            if max < min {
                bail!("gen:random max ({max}) is less than min ({min})");
            }
            Ok(Gen::Random(min, max))
        }
    }
}

/// Milliseconds since the Unix epoch.
fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Current UTC time as an RFC3339 string (`YYYY-MM-DDTHH:MM:SSZ`), dependency-free.
fn rfc3339_utc_now() -> String {
    format_rfc3339((unix_millis() / 1000) as i64)
}

/// Format `secs` (Unix epoch seconds) as an RFC3339 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`). Pure, so it is unit-testable.
fn format_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days-since-Unix-epoch to a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Walk a dotted path into a JSON value. Empty path returns the whole value.
/// What a single render needs from the payload.
enum Payload {
    /// No payload token, no message, or the payload did not parse.
    None,
    /// The whole document, walked per token (two or more payload tokens).
    Doc(Value),
    /// The one field the template asked for, already extracted.
    Field(Option<Value>),
}

/// Deserialize only the value at `path`, skipping every other field.
///
/// Equivalent to `walk(&serde_json::from_slice(bytes)?, path)` — the picked subtree is built
/// as a normal `Value`, so rendering is byte-identical — but the rest of the document is
/// discarded as it is scanned instead of being allocated.
fn pick_path(bytes: &[u8], path: &str) -> Option<Value> {
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let picked = Pick(&segs).deserialize(&mut de).ok().flatten();
    // Keep `from_slice`'s strictness: trailing garbage is still a parse failure.
    de.end().ok()?;
    picked
}

#[derive(Clone, Copy)]
struct Pick<'a>(&'a [&'a str]);

impl<'de, 'a> DeserializeSeed<'de> for Pick<'a> {
    type Value = Option<Value>;

    fn deserialize<D: de::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        match self.0.split_first() {
            None => Value::deserialize(de).map(Some),
            Some((head, rest)) => de.deserialize_any(PickVisitor { head, rest }),
        }
    }
}

struct PickVisitor<'a> {
    head: &'a str,
    rest: &'a [&'a str],
}

// Scalars are left to the default `Visitor` methods, which error. `pick_path` maps that to
// `None`, which is what `walk` returns when a path runs into a scalar.
impl<'de, 'a> Visitor<'de> for PickVisitor<'a> {
    type Value = Option<Value>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object or array")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut found = None;
        while let Some(matched) = map.next_key_seed(KeyEq(self.head))? {
            if matched {
                // Last occurrence wins, matching how `Value` folds duplicate keys.
                found = map.next_value_seed(Pick(self.rest))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(found)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let want: Option<usize> = self.head.parse().ok();
        let mut found = None;
        let mut idx = 0usize;
        loop {
            if Some(idx) == want {
                match seq.next_element_seed(Pick(self.rest))? {
                    Some(value) => found = value,
                    None => break,
                }
            } else if seq.next_element::<IgnoredAny>()?.is_none() {
                break;
            }
            idx += 1;
        }
        Ok(found)
    }
}

/// Compares an object key against a wanted name without allocating it.
#[derive(Clone, Copy)]
struct KeyEq<'a>(&'a str);

impl<'de, 'a> DeserializeSeed<'de> for KeyEq<'a> {
    type Value = bool;

    fn deserialize<D: de::Deserializer<'de>>(self, de: D) -> Result<bool, D::Error> {
        de.deserialize_str(self)
    }
}

impl<'de, 'a> Visitor<'de> for KeyEq<'a> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
        Ok(v == self.0)
    }
}

fn walk<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        cur = match cur {
            Value::Object(map) => map.get(part)?,
            Value::Array(arr) => arr.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Stringify a resolved JSON value: strings unquoted, null as empty, everything
/// else via its JSON representation (numbers/bools bare, objects/arrays as JSON).
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Append `s` JSON-string-escaped (without surrounding quotes) to `out`.
fn json_escape_into(s: &str, out: &mut Vec<u8>) {
    // serde_json emits a fully-quoted string; strip the surrounding quotes.
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    let inner = &quoted.as_bytes()[1..quoted.len() - 1];
    out.extend_from_slice(inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_str(
        body: &str,
        content_type: Option<&str>,
        msg: Option<&CanonicalMessage>,
    ) -> String {
        let tpl = CompiledTemplate::compile(body, content_type).unwrap();
        String::from_utf8(tpl.render(msg)).unwrap()
    }

    fn msg_with(payload: &str, metadata: &[(&str, &str)]) -> CanonicalMessage {
        let mut m = CanonicalMessage::new(payload.as_bytes().to_vec(), None);
        for (k, v) in metadata {
            m.metadata.insert(k.to_string(), v.to_string());
        }
        m
    }

    /// The whole safety argument for `pick_path`: it must agree with parsing the document and
    /// walking it. A disagreement would silently change rendered dedup keys, and an existing
    /// on-disk dedup store would stop matching after an upgrade.
    #[test]
    fn pick_path_matches_a_full_parse_and_walk() {
        let payloads: [&[u8]; 22] = [
            br#"{"id":42,"name":"bob","amount":1.50,"created_at":"2026-01-01T00:00:00Z"}"#,
            br#"{"a":{"b":{"c":"deep"}},"z":1}"#,
            br#"{"a":{"b":["x","y","z"]}}"#,
            br#"{"dup":1,"dup":2}"#,
            "{\"esc\\\"key\":\"v\",\"s\":\"line\\nbreak é\"}".as_bytes(),
            br#"{"n":null,"t":true,"obj":{"k":[1,2]},"arr":[{"q":9}]}"#,
            br#"{"big":123456789012345678,"neg":-0.0,"exp":1e3}"#,
            "{\"unicode\":\"héllo ☃\"}".as_bytes(),
            // A key spelled with \u escapes: must still match the plain path "id".
            br#"{"\u0069\u0064":7}"#,
            // Keys where one is a prefix of another.
            br#"{"id":1,"identity":2,"i":3}"#,
            // Surrogate pair, escaped solidus, tab and a control char, all as \u escapes.
            br#"{"emoji":"\ud83d\ude00","sl":"a\/b","tab":"a\tb","ctl":"a\u0001b"}"#,
            br#"{"empty":{},"earr":[],"nested":{"a":{"a":{"a":1}}}}"#,
            br#"[10,20,30]"#,
            br#"42"#,
            br#""bare""#,
            br#"{"a":1} trailing"#,
            br#"{not json"#,
            b"",
            // Invalid UTF-8: JSON must be UTF-8, so both paths must reject these identically.
            &[b'{', b'"', b'a', b'"', b':', b'"', 0xff, 0xfe, b'"', b'}'],
            &[0xff, 0xfe, 0xfd],
            // Lone surrogate and a truncated escape.
            br#"{"a":"\ud83d"}"#,
            br#"{"a":"\u00"}"#,
        ];
        let paths = [
            "id",
            "name",
            "amount",
            "a",
            "a.b",
            "a.b.c",
            "a.b.1",
            "dup",
            "esc\"key",
            "s",
            "n",
            "t",
            "obj",
            "arr.0.q",
            "big",
            "neg",
            "exp",
            "unicode",
            "identity",
            "i",
            "emoji",
            "sl",
            "tab",
            "ctl",
            "empty",
            "earr",
            "empty.x",
            "earr.0",
            "nested.a.a.a",
            "0",
            "2",
            "9",
            "missing",
            "a.missing.x",
            "",
        ];

        for payload in payloads {
            for path in paths {
                let expected = serde_json::from_slice::<Value>(payload)
                    .ok()
                    .and_then(|doc| walk(&doc, path).cloned());
                let actual = pick_path(payload, path);
                assert_eq!(
                    actual,
                    expected,
                    "pick_path disagreed for payload {} and path {path:?}",
                    String::from_utf8_lossy(payload)
                );
            }
        }
    }

    /// A second payload token disables the single-field path; both spellings must render alike.
    #[test]
    fn one_and_two_payload_tokens_render_the_same_field() {
        let msg = msg_with(r#"{"id":42,"amount":1.50,"s":"a\"b"}"#, &[]);
        assert_eq!(render_str("${payload:id}", None, Some(&msg)), "42");
        assert_eq!(
            render_str("${payload:id}-${payload:amount}", None, Some(&msg)),
            "42-1.5"
        );
        // Escaping still applies to the extracted field in a JSON context.
        assert_eq!(
            render_str(
                r#"{"v":"${payload:s}"}"#,
                Some("application/json"),
                Some(&msg)
            ),
            r#"{"v":"a\"b"}"#
        );
    }

    #[test]
    fn no_tokens_is_verbatim_and_not_dynamic() {
        let tpl = CompiledTemplate::compile("plain body", None).unwrap();
        assert!(!tpl.is_dynamic());
        assert_eq!(String::from_utf8(tpl.render(None)).unwrap(), "plain body");
    }

    /// `render` cannot distinguish "field absent" from "field empty" — both come out as an
    /// empty substitution — so key/identity callers need `render_resolved` instead.
    #[test]
    fn render_resolved_rejects_any_unresolved_token() {
        let msg = msg_with(r#"{"tenant":"acme","blank":""}"#, &[]);

        let resolved = |body: &str| {
            CompiledTemplate::compile(body, None)
                .unwrap()
                .render_resolved(Some(&msg))
                .map(|out| String::from_utf8(out).unwrap())
        };

        assert_eq!(resolved("${payload:tenant}"), Some("acme".to_string()));
        // A present-but-empty value is resolved; it is the caller's business whether to use it.
        assert_eq!(resolved("${payload:blank}"), Some(String::new()));
        assert_eq!(resolved("${payload:missing}"), None);
        // The cases a plain emptiness check misses, because the render is non-empty.
        assert_eq!(resolved("${payload:tenant}-${payload:missing}"), None);
        assert_eq!(resolved("order-${payload:missing}"), None);
        assert_eq!(resolved("${metadata:absent}"), None);
        // `render` keeps substituting empty, so existing bodies are unaffected.
        assert_eq!(
            render_str("order-${payload:missing}", None, Some(&msg)),
            "order-"
        );
    }

    #[test]
    fn payload_nested_path_and_array_index() {
        let msg = msg_with(r#"{"a":{"b":["x","y"]}}"#, &[]);
        assert_eq!(render_str("${payload:a.b.1}", None, Some(&msg)), "y");
    }

    #[test]
    fn metadata_and_message_id() {
        let msg = msg_with("{}", &[("k", "v")]);
        assert_eq!(render_str("${metadata:k}", None, Some(&msg)), "v");
        let out = render_str("${message:id}", None, Some(&msg));
        assert_eq!(out, format_message_id(msg.message_id));
    }

    #[test]
    fn missing_fields_resolve_empty() {
        let msg = msg_with("{}", &[]);
        assert_eq!(
            render_str("[${payload:nope}][${metadata:nope}]", None, Some(&msg)),
            "[][]"
        );
    }

    #[test]
    fn json_escape_is_default_for_json_content_type() {
        let msg = msg_with(r#"{"name":"a\"b\nc"}"#, &[]);
        let out = render_str(
            r#"{"n":"${payload:name}"}"#,
            Some("application/json"),
            Some(&msg),
        );
        // The embedded quote and newline are escaped, keeping the body valid JSON.
        assert_eq!(out, r#"{"n":"a\"b\nc"}"#);
        assert!(serde_json::from_str::<Value>(&out).is_ok());
    }

    #[test]
    fn raw_filter_bypasses_escaping() {
        let msg = msg_with(r#"{"frag":{"k":1}}"#, &[]);
        let out = render_str(
            r#"{"x":${payload:frag | raw}}"#,
            Some("application/json"),
            Some(&msg),
        );
        assert_eq!(out, r#"{"x":{"k":1}}"#);
    }

    #[test]
    fn no_escape_without_json_content_type() {
        let msg = msg_with(r#"{"name":"a\"b"}"#, &[]);
        assert_eq!(render_str("${payload:name}", None, Some(&msg)), "a\"b");
    }

    #[test]
    fn dollar_dollar_brace_escapes_token() {
        assert_eq!(
            render_str("cost is $${payload:x}", None, None),
            "cost is ${payload:x}"
        );
    }

    #[test]
    fn bare_dollar_dollar_is_left_untouched() {
        // Only `$${` is an escape; a plain `$$` must survive (money, shell PID, …).
        assert_eq!(render_str("pay $$5 now", None, None), "pay $$5 now");
        assert_eq!(render_str("pid=$$", None, None), "pid=$$");
    }

    #[test]
    fn format_rfc3339_matches_known_timestamps() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap day and end-of-year boundary.
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn unknown_namespace_is_verbatim() {
        assert_eq!(
            render_str("${FOO} ${bar:baz}", None, None),
            "${FOO} ${bar:baz}"
        );
    }

    #[test]
    fn unknown_namespace_with_bogus_filter_is_verbatim() {
        // An unrecognized namespace renders unchanged even when it carries a
        // filter that would be rejected on a recognized namespace.
        assert_eq!(
            render_str("${bar:baz | nope}", None, None),
            "${bar:baz | nope}"
        );
    }

    #[test]
    fn gen_counter_increments_and_is_shared() {
        let tpl = CompiledTemplate::compile("${gen:counter}", None).unwrap();
        assert_eq!(String::from_utf8(tpl.render(None)).unwrap(), "0");
        assert_eq!(String::from_utf8(tpl.render(None)).unwrap(), "1");
        assert_eq!(String::from_utf8(tpl.render(None)).unwrap(), "2");
    }

    #[test]
    fn gen_random_within_range() {
        let tpl = CompiledTemplate::compile("${gen:random(5,7)}", None).unwrap();
        for _ in 0..100 {
            let v: i64 = String::from_utf8(tpl.render(None))
                .unwrap()
                .parse()
                .unwrap();
            assert!((5..=7).contains(&v), "value {v} out of range");
        }
    }

    #[test]
    fn gen_uuid_is_fresh_each_render() {
        let tpl = CompiledTemplate::compile("${gen:uuid}", None).unwrap();
        let a = String::from_utf8(tpl.render(None)).unwrap();
        let b = String::from_utf8(tpl.render(None)).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36); // canonical UUID string
    }

    #[test]
    fn env_is_resolved_at_compile_time() {
        // SAFETY: single-threaded test; set then read a unique var.
        unsafe { std::env::set_var("MQB_INTERP_TEST_VAR", "hello") };
        assert_eq!(
            render_str("${env:MQB_INTERP_TEST_VAR}", None, None),
            "hello"
        );
    }

    #[test]
    fn bad_gen_spec_errors_at_compile() {
        assert!(CompiledTemplate::compile("${gen:bogus}", None).is_err());
        assert!(CompiledTemplate::compile("${gen:random(3,1)}", None).is_err());
        assert!(CompiledTemplate::compile("${payload:x | bogus}", None).is_err());
        assert!(CompiledTemplate::compile("${message:nope}", None).is_err());
    }

    #[test]
    fn source_side_no_message_resolves_gen_only() {
        // Only gen/env populate when there is no input message.
        let out = render_str("id=${message:id} n=${gen:counter}", None, None);
        assert_eq!(out, "id= n=0");
    }
}

/// Template properties. `compile` runs on operator-supplied config, so the invariants that matter
/// are that literal bodies survive untouched and that no input reaches a panic instead of an error.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// A body with no `$` has no tokens, so it must render back byte-identical.
        #[test]
        fn a_literal_body_renders_back_unchanged(body in "[^$]{0,128}") {
            let template = CompiledTemplate::compile(&body, None).unwrap();
            prop_assert!(!template.is_dynamic());
            prop_assert_eq!(template.render(None), body.into_bytes());
        }

        /// `$${…}` is the escape for a body that wants a literal `${…}`.
        #[test]
        fn the_escape_sequence_yields_a_literal_token(inner in "[a-z]{1,8}") {
            let body = ["$${", &inner, "}"].concat();
            let template = CompiledTemplate::compile(&body, None).unwrap();
            prop_assert!(!template.is_dynamic());
            prop_assert_eq!(template.render(None), ["${", &inner, "}"].concat().into_bytes());
        }

        /// Malformed tokens must come back as `Err` at startup, never as a panic.
        #[test]
        fn compiling_arbitrary_input_never_panics(body in ".{0,128}") {
            for content_type in [None, Some("application/json"), Some("text/plain")] {
                if let Ok(template) = CompiledTemplate::compile(&body, content_type) {
                    let _ = template.render(None);
                }
            }
        }
    }
}
