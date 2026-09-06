//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::coerce::{coerce, render_path, type_name, Crumb, Ty};
use super::compiled::Opts;
use super::error::{ErrorKind, TransformError};
use serde_json::Value;

/// The supported JSON Schema subset, pre-resolved so the hot path never inspects raw
/// schema JSON. `properties` is a sorted `Vec` rather than a map: it is only ever
/// iterated, and sorting keeps error reporting deterministic.
#[derive(Debug, Default)]
pub(super) struct CompiledSchema {
    pub(super) ty: Option<Ty>,
    nullable: bool,
    pub(super) properties: Vec<(String, CompiledSchema)>,
    pub(super) required: Vec<String>,
    pub(super) default: Option<Value>,
    pub(super) items: Option<Box<CompiledSchema>>,
    pub(super) enum_values: Option<Vec<Value>>,
    /// Set when `contentMediaType` names a JSON media type: the string is parsed in place.
    /// `Some(None)` parses without validating the result, `Some(Some(_))` also applies
    /// `contentSchema` to it.
    pub(super) content: Option<Option<Box<CompiledSchema>>>,
}

/// Whether `contentMediaType` names something we can parse as JSON. Covers the `+json`
/// structured suffix (`application/vnd.acme.order+json`) alongside `application/json`.
/// Parameters (`; charset=utf-8`) are stripped.
pub(super) fn is_json_media_type(media_type: &str) -> bool {
    let base = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    base == "application/json" || base == "text/json" || base.ends_with("+json")
}

impl CompiledSchema {
    pub(super) fn compile(schema: &Value) -> anyhow::Result<Self> {
        let obj = schema
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("schema must be a JSON object"))?;

        let mut ty = None;
        let mut nullable = obj
            .get("nullable")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match obj.get("type") {
            Some(Value::String(s)) => {
                ty = Some(
                    Ty::parse(s).ok_or_else(|| anyhow::anyhow!("unsupported schema type '{s}'"))?,
                );
            }
            // `["string", "null"]` is the JSON Schema way of spelling nullable.
            Some(Value::Array(types)) => {
                for entry in types {
                    let s = entry
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("schema 'type' entries must be strings"))?;
                    let parsed = Ty::parse(s)
                        .ok_or_else(|| anyhow::anyhow!("unsupported schema type '{s}'"))?;
                    if parsed == Ty::Null {
                        nullable = true;
                    } else if ty.is_none() {
                        ty = Some(parsed);
                    } else {
                        anyhow::bail!("schema 'type' lists more than one non-null type");
                    }
                }
            }
            Some(_) => anyhow::bail!("schema 'type' must be a string or array of strings"),
            None => {}
        }

        let mut properties = Vec::new();
        if let Some(props) = obj.get("properties") {
            let props = props
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("schema 'properties' must be an object"))?;
            for (name, sub) in props {
                properties.push((name.clone(), CompiledSchema::compile(sub)?));
            }
            properties.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let required = match obj.get("required") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow::anyhow!("schema 'required' entries must be strings"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
            Some(_) => anyhow::bail!("schema 'required' must be an array"),
            None => Vec::new(),
        };

        let items = match obj.get("items") {
            Some(sub) => Some(Box::new(CompiledSchema::compile(sub)?)),
            None => None,
        };

        let enum_values = match obj.get("enum") {
            Some(Value::Array(values)) => Some(values.clone()),
            Some(_) => anyhow::bail!("schema 'enum' must be an array"),
            None => None,
        };

        // Embedded JSON is opt-in per field, never implied by `coerce`: parsing a string as
        // a document is a different operation from widening `"42"` to `42`, and the JSON
        // Schema spec requires the consumer to ask for it. A media type we cannot decode
        // (or one paired with a `contentEncoding` we do not implement) is ignored like any
        // other unsupported keyword, leaving the string untouched.
        let encoded = obj.contains_key("contentEncoding");
        let content = match obj.get("contentMediaType") {
            Some(Value::String(mt)) if is_json_media_type(mt) && !encoded => {
                let inner = match obj.get("contentSchema") {
                    Some(sub) => Some(Box::new(CompiledSchema::compile(sub)?)),
                    None => None,
                };
                Some(inner)
            }
            Some(Value::String(_)) | None => None,
            Some(_) => anyhow::bail!("schema 'contentMediaType' must be a string"),
        };

        Ok(Self {
            ty,
            nullable,
            properties,
            required,
            default: obj.get("default").cloned(),
            items,
            enum_values,
            content,
        })
    }

    /// Coerce, fill defaults and validate in a single walk.
    ///
    /// `crumbs` is a reusable breadcrumb stack borrowed from the schema, so the error
    /// path is only materialised into a `String` when something actually fails.
    pub(super) fn apply<'s>(
        &'s self,
        value: &mut Value,
        crumbs: &mut Vec<Crumb<'s>>,
        opts: Opts,
    ) -> Result<(), TransformError> {
        // An empty string stands in for "no value" in CSV and many SQL exports, so it is
        // read as a null and then handled by `nullable` / `default` like any other null.
        let was_empty = opts.coerce_empty_as_null && value.as_str() == Some("");
        if was_empty {
            *value = Value::Null;
        }

        if value.is_null() {
            // A nullable null is explicitly allowed and needs no type/enum check.
            if self.nullable {
                return Ok(());
            }
            match (opts.apply_defaults, &self.default) {
                (true, Some(default)) => *value = default.clone(),
                // Reported directly rather than falling through to coercion, which would
                // render the far less helpful "cannot coerce null to integer".
                _ => {
                    if self.ty.is_some_and(|ty| ty != Ty::Null) {
                        // Naming the coercion, or the null would look like it came from the
                        // payload when the payload actually held `""`.
                        let cause = if was_empty {
                            "field is an empty string, read as null by coerce_empty_as_null, but is not nullable and has no default"
                        } else {
                            "field is null but is not nullable and has no default"
                        };
                        return Err(TransformError::new(
                            render_path(crumbs),
                            ErrorKind::TypeMismatch,
                            cause,
                        ));
                    }
                }
            }
        }

        if let Some(ty) = self.ty {
            if !ty.matches(value) {
                if !opts.coerce {
                    return Err(TransformError::new(
                        render_path(crumbs),
                        ErrorKind::TypeMismatch,
                        format!("expected {}, found {}", ty.name(), type_name(value)),
                    ));
                }
                coerce(value, ty, crumbs)?;
            }
        }

        if let Some(allowed) = &self.enum_values {
            if !allowed.contains(value) {
                return Err(TransformError::new(
                    render_path(crumbs),
                    ErrorKind::Enum,
                    // The rejected value is payload data; report only the allowed set,
                    // matching the type-only reporting in `coerce()`.
                    format!("value is not one of {}", Value::Array(allowed.clone())),
                ));
            }
        }

        // Decode embedded JSON before recursing: the decoded document, not the string that
        // carried it, is what `contentSchema` describes.
        if let Some(content_schema) = &self.content {
            if let Value::String(raw) = value {
                *value = serde_json::from_str(raw).map_err(|e| {
                    TransformError::new(
                        render_path(crumbs),
                        ErrorKind::Content,
                        format!("contentMediaType is JSON but the string does not parse: {e}"),
                    )
                })?;
                if let Some(schema) = content_schema {
                    // Same crumbs, so a failure inside reads as `$.payload.qty`.
                    schema.apply(value, crumbs, opts)?;
                }
                // `properties`/`items` here belong to the string, not to what it decoded to.
                return Ok(());
            }
        }

        match value {
            Value::Object(map) => {
                if opts.apply_defaults {
                    for (name, sub) in &self.properties {
                        if !map.contains_key(name) {
                            if let Some(default) = &sub.default {
                                map.insert(name.clone(), default.clone());
                            }
                        }
                    }
                }
                // Checked after defaults, so a default satisfies `required`.
                for name in &self.required {
                    if !map.contains_key(name) {
                        crumbs.push(Crumb::Key(name));
                        let path = render_path(crumbs);
                        crumbs.pop();
                        return Err(TransformError::new(
                            path,
                            ErrorKind::MissingRequired,
                            "required field is missing and no default is defined",
                        ));
                    }
                }
                for (name, sub) in &self.properties {
                    if let Some(field) = map.get_mut(name) {
                        crumbs.push(Crumb::Key(name));
                        let result = sub.apply(field, crumbs, opts);
                        crumbs.pop();
                        result?;
                    }
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = &self.items {
                    for (i, item) in items.iter_mut().enumerate() {
                        crumbs.push(Crumb::Index(i));
                        let result = item_schema.apply(item, crumbs, opts);
                        crumbs.pop();
                        result?;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// True when `apply` would leave `raw` — one field's verbatim JSON span — byte for
    /// byte as it is, so the field can be copied straight to the output and never has to
    /// become a `Value`. Deliberately conservative: anything with an obligation attached
    /// (`enum`, `contentMediaType`, sub-schemas, a default that a null could pull in)
    /// answers `false` and takes the normal path.
    pub(super) fn is_passthrough(&self, raw: &str) -> bool {
        if self.enum_values.is_some()
            || self.content.is_some()
            || self.default.is_some()
            || self.items.is_some()
            || !self.properties.is_empty()
            || !self.required.is_empty()
        {
            return false;
        }
        // Nothing declared: `apply` would only recurse, and there is nothing to recurse
        // into without `properties` or `items`.
        let Some(ty) = self.ty else {
            return true;
        };
        let bytes = raw.as_bytes();
        match (ty, bytes.first()) {
            (Ty::String, Some(b'"'))
            | (Ty::Object, Some(b'{'))
            | (Ty::Array, Some(b'['))
            | (Ty::Boolean, Some(b't' | b'f'))
            | (Ty::Null, Some(b'n')) => true,
            // Looking like a number is not enough for either numeric type: `Ty::matches`
            // runs against a `Value`, which holds integers as i64/u64 and everything else
            // as f64. A 24-digit id or `1e400` is well-formed JSON that no `Value` can
            // represent, and the normal path rejects it — so parse before waving it
            // through. This also rules out fractions and exponents for `integer`.
            (Ty::Integer, Some(b'-' | b'0'..=b'9')) => {
                raw.parse::<i64>().is_ok() || raw.parse::<u64>().is_ok()
            }
            (Ty::Number, Some(b'-' | b'0'..=b'9')) => raw.parse::<f64>().is_ok_and(f64::is_finite),
            _ => false,
        }
    }

    /// Rewrites a quoted scalar into its typed JSON token, writing straight from the raw
    /// span instead of building a `Value` for the field and serializing it back.
    ///
    /// Text-typed sources — CSV, and SQL exports that stringify everything — hand every
    /// column over as a string, so this is the coercion a typing schema performs on
    /// nearly every field of nearly every row.
    ///
    /// Returns `false` whenever it will not serve the field, *including* every failure,
    /// so an unconvertible value still reaches `apply` and gets its proper error. It
    /// writes nothing in that case.
    pub(super) fn coerce_scalar_raw(&self, raw: &str, opts: Opts, out: &mut Vec<u8>) -> bool {
        // Without `coerce` a mistyped field is an error, which only `apply` can phrase.
        // The rest mirrors `is_passthrough`: anything else declared means `apply` has
        // more to do than change this value's type.
        if !opts.coerce
            || self.enum_values.is_some()
            || self.content.is_some()
            || self.default.is_some()
            || self.items.is_some()
            || !self.properties.is_empty()
            || !self.required.is_empty()
        {
            return false;
        }

        // Only an unescaped string literal: the span between the quotes is then exactly
        // the text `coerce` would see, with no unescaping to do.
        let Some(text) = raw
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|text| !text.contains('\\'))
        else {
            return false;
        };
        let text = text.trim();

        // Each arm reproduces the matching rule in `coerce`, and writes through
        // `serde_json` so the token is formatted exactly as the general path formats it.
        let start = out.len();
        let written = match self.ty {
            Some(Ty::Integer) => {
                if let Ok(value) = text.parse::<i64>() {
                    serde_json::to_writer(&mut *out, &value).is_ok()
                } else if let Ok(value) = text.parse::<u64>() {
                    serde_json::to_writer(&mut *out, &value).is_ok()
                } else {
                    false
                }
            }
            Some(Ty::Number) => text
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .is_some_and(|number| serde_json::to_writer(&mut *out, &number).is_ok()),
            Some(Ty::Boolean) => match text {
                "true" | "1" => {
                    out.extend_from_slice(b"true");
                    true
                }
                "false" | "0" => {
                    out.extend_from_slice(b"false");
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if !written {
            out.truncate(start);
        }
        written
    }

    /// True when decoding an embedded JSON document is the *only* thing `apply` would do
    /// to `raw`. That decode is a string unescape followed by a well-formedness check,
    /// both of which work on bytes, so no `Value` has to be built for the document —
    /// which for a document of any size is the bulk of the field's cost.
    ///
    /// A `contentSchema` means the document is inspected afterwards and does not qualify.
    pub(super) fn is_plain_content_decode(&self, raw: &str) -> bool {
        matches!(self.content, Some(None))
            && matches!(self.ty, None | Some(Ty::String))
            && self.enum_values.is_none()
            && self.default.is_none()
            && self.items.is_none()
            && self.properties.is_empty()
            && self.required.is_empty()
            && raw.as_bytes().first() == Some(&b'"')
    }
}
