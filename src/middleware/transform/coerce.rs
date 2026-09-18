//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::error::{ErrorKind, TransformError};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ty {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
    Null,
}

impl Ty {
    pub(super) fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "string" => Ty::String,
            "integer" => Ty::Integer,
            "number" => Ty::Number,
            "boolean" => Ty::Boolean,
            "object" => Ty::Object,
            "array" => Ty::Array,
            "null" => Ty::Null,
            _ => return None,
        })
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Ty::String => "string",
            Ty::Integer => "integer",
            Ty::Number => "number",
            Ty::Boolean => "boolean",
            Ty::Object => "object",
            Ty::Array => "array",
            Ty::Null => "null",
        }
    }

    pub(super) fn matches(self, v: &Value) -> bool {
        match (self, v) {
            (Ty::String, Value::String(_)) => true,
            (Ty::Integer, Value::Number(n)) => n.is_i64() || n.is_u64(),
            (Ty::Number, Value::Number(_)) => true,
            (Ty::Boolean, Value::Bool(_)) => true,
            (Ty::Object, Value::Object(_)) => true,
            (Ty::Array, Value::Array(_)) => true,
            (Ty::Null, Value::Null) => true,
            _ => false,
        }
    }
}

pub(super) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Applies the one safe coercion for `ty`, or fails. Never best-effort: a value that
/// cannot be converted losslessly is an error, not a silent substitution.
pub(super) fn coerce(
    value: &mut Value,
    ty: Ty,
    crumbs: &[Crumb<'_>],
) -> Result<(), TransformError> {
    let coerced = match (ty, &*value) {
        (Ty::Integer, Value::String(s)) => {
            let t = s.trim();
            t.parse::<i64>()
                .ok()
                .map(Value::from)
                .or_else(|| t.parse::<u64>().ok().map(Value::from))
        }
        (Ty::Number, Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        (Ty::Boolean, Value::String(s)) => match s.trim() {
            "true" | "1" => Some(Value::Bool(true)),
            "false" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        (Ty::String, Value::Number(n)) => Some(Value::String(n.to_string())),
        _ => None,
    };

    match coerced {
        Some(new_value) => {
            *value = new_value;
            Ok(())
        }
        None => Err(TransformError::new(
            render_path(crumbs),
            ErrorKind::Coercion,
            // Report only the source and target types, never the offending value itself:
            // the value may hold sensitive data and this error can surface in logs and in
            // pass-through error metadata. The rendered path already locates the field.
            format!("cannot coerce {} to {}", type_name(value), ty.name()),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Crumb<'a> {
    Key(&'a str),
    Index(usize),
}

pub(super) fn render_path(crumbs: &[Crumb<'_>]) -> String {
    let mut out = String::from("$");
    for crumb in crumbs {
        match crumb {
            Crumb::Key(k) => {
                out.push('.');
                out.push_str(k);
            }
            Crumb::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    out
}
