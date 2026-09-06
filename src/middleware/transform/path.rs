//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::coerce::type_name;
use super::error::{ErrorKind, TransformError};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seg {
    Key(String),
    Index(usize),
}

/// A source path compiled once from `$.a.b[0]` into segments. Lookups walk borrowed
/// `Value` references and allocate nothing.
#[derive(Debug, Clone)]
pub(super) struct CompiledPath {
    pub(super) segs: Vec<Seg>,
    /// The original spec, kept for error messages.
    pub(super) spec: String,
}

impl CompiledPath {
    pub(super) fn parse(spec: &str) -> anyhow::Result<Self> {
        let trimmed = spec.trim();
        let body = trimmed.strip_prefix('$').unwrap_or(trimmed);
        let body = body.strip_prefix('.').unwrap_or(body);

        let mut segs = Vec::new();
        if !body.is_empty() {
            for part in body.split('.') {
                if part.is_empty() {
                    anyhow::bail!("empty segment in path '{spec}'");
                }
                let (name, mut brackets) = match part.find('[') {
                    Some(i) => (&part[..i], &part[i..]),
                    None => (part, ""),
                };
                if !name.is_empty() {
                    segs.push(Seg::Key(name.to_string()));
                }
                while !brackets.is_empty() {
                    let close = brackets
                        .find(']')
                        .ok_or_else(|| anyhow::anyhow!("unclosed '[' in path '{spec}'"))?;
                    let raw = &brackets[1..close];
                    let idx: usize = raw.parse().map_err(|_| {
                        anyhow::anyhow!("invalid array index '{raw}' in path '{spec}'")
                    })?;
                    segs.push(Seg::Index(idx));
                    brackets = &brackets[close + 1..];
                }
            }
        }
        Ok(Self {
            segs,
            spec: trimmed.to_string(),
        })
    }

    pub(super) fn get<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        let mut cur = root;
        for seg in &self.segs {
            cur = match seg {
                Seg::Key(k) => cur.get(k.as_str())?,
                Seg::Index(i) => cur.get(*i)?,
            };
        }
        Some(cur)
    }

    /// Moves the value out, leaving a `Null` in its place. Only sound when no other rule
    /// reads through this path — see [`paths_are_disjoint`].
    pub(super) fn take(&self, root: &mut Value) -> Option<Value> {
        let mut cur = root;
        for seg in &self.segs {
            cur = match seg {
                Seg::Key(k) => cur.get_mut(k.as_str())?,
                Seg::Index(i) => cur.get_mut(*i)?,
            };
        }
        Some(cur.take())
    }
}

// --- Mapping stage ---

#[derive(Debug)]
pub(super) struct CompiledRule {
    /// Output location, split on dots so `address.city` nests.
    pub(super) out: Vec<String>,
    pub(super) from: CompiledPath,
    pub(super) default: Option<Value>,
    pub(super) required: bool,
}

/// True when no rule's source path is a prefix of — or equal to — another's. That is what
/// lets the mapping stage move picked values out of the input instead of deep-cloning
/// them: taking one value cannot then empty out what another rule still has to read.
/// Quadratic, but this runs once per middleware over a handful of rules.
pub(super) fn paths_are_disjoint(rules: &[CompiledRule]) -> bool {
    !rules.iter().enumerate().any(|(i, a)| {
        rules[..i].iter().any(|b| {
            let shared = a.from.segs.len().min(b.from.segs.len());
            a.from.segs[..shared] == b.from.segs[..shared]
        })
    })
}

/// Writes `value` at `path`, creating intermediate objects as needed.
pub(super) fn insert_at(
    root: &mut Value,
    path: &[String],
    value: Value,
) -> Result<(), TransformError> {
    let (last, parents) = match path.split_last() {
        Some(split) => split,
        // Compilation rejects empty output keys, so this is unreachable in practice.
        None => return Ok(()),
    };
    let mut cur = root;
    for key in parents {
        let obj = match cur {
            Value::Object(map) => map,
            other => {
                return Err(TransformError::new(
                    format!("$.{}", path.join(".")),
                    ErrorKind::TypeMismatch,
                    format!(
                        "cannot nest under '{key}': it is already a {}",
                        type_name(other)
                    ),
                ))
            }
        };
        cur = obj
            .entry(key.as_str())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    match cur {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        other => Err(TransformError::new(
            format!("$.{}", path.join(".")),
            ErrorKind::TypeMismatch,
            format!("cannot set '{last}': parent is a {}", type_name(other)),
        )),
    }
}
