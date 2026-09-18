//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::coerce::{Crumb, Ty};
use super::error::{ErrorKind, TransformError};
use super::path::{insert_at, paths_are_disjoint, CompiledPath, CompiledRule, Seg};
use super::schema::CompiledSchema;
use super::TRANSFORM_ERROR_KEY;
use crate::middleware::raw_json::RawPairs;
use crate::models::{MappingRule, TransformErrorPolicy, TransformMiddleware};
use crate::CanonicalMessage;
use bytes::Bytes;
use serde_json::{Map, Value};
#[cfg(feature = "zen")]
use zen_expression::compiler::{FetchFastTarget, Opcode};
#[cfg(feature = "zen")]
use zen_expression::variable::Symbol;
#[cfg(feature = "zen")]
use zen_expression::{compile_expression, expression::Standard, Expression, Variable};

#[derive(Debug, Clone, Copy)]
pub(super) struct Opts {
    pub(super) coerce: bool,
    pub(super) apply_defaults: bool,
    pub(super) coerce_empty_as_null: bool,
}

// --- Compiled configuration ---

/// Everything derived from config, built once and shared by every message.
#[derive(Debug)]
pub(super) struct Compiled {
    /// Empty when no mapping stage is configured.
    pub(super) rules: Vec<CompiledRule>,
    #[cfg(feature = "zen")]
    expression: Option<Expression<Standard>>,
    #[cfg(feature = "zen")]
    expression_uses_metadata: bool,
    pub(super) schema: Option<CompiledSchema>,
    pub(super) opts: Opts,
    pub(super) on_error: TransformErrorPolicy,
    /// Decided once: whether a fast path may be tried at all.
    pub(super) fast_eligible: bool,
    /// Decided once: an expression with no mapping or schema stage beside it, which lets
    /// the payload be read straight into the engine's own `Variable`.
    #[cfg(feature = "zen")]
    expression_only: bool,
    /// `Some` when the mapping is a plain projection `project_fast` can serve; parallel
    /// to `rules`, which stays the source of truth for everything else about a rule.
    pub(super) fast_map: Option<Vec<FastMapRule>>,
    /// Decided once: whether picked values can be moved out of the input, see
    /// [`paths_are_disjoint`].
    pub(super) take_inputs: bool,
    /// Decided once: see `map_sorts_keys`.
    pub(super) sort_keys: bool,
}

/// A mapping rule reduced to what `project_fast` needs: the single top-level input key to
/// pick, and the output key already quoted and escaped, ready to write.
#[derive(Debug)]
pub(super) struct FastMapRule {
    from: String,
    out: Vec<u8>,
}

/// Whether `serde_json::Map` iterates its keys in sorted order.
///
/// `Map` is a `BTreeMap` (sorted) normally and an `IndexMap` (insertion order) when
/// anything in the build enables `serde_json/preserve_order` — a decision made by feature
/// unification, which this crate cannot see with `cfg`. `transform_fast` writes its keys
/// directly rather than through a `Map`, so it has to emit them the way the normal path
/// would, and the only dependable way to learn which `Map` is compiled in is to ask one.
/// Called once per middleware, never on the hot path.
pub(super) fn map_sorts_keys() -> bool {
    let mut probe = Map::new();
    probe.insert("b".to_string(), Value::Null);
    probe.insert("a".to_string(), Value::Null);
    probe.keys().next().is_some_and(|first| first == "a")
}

/// Whether the root schema is a plain object carrying no obligation of its own, which is
/// what lets a field-by-field rewrite stand in for parsing the whole payload. Root-level
/// `required` and defaults need to know which fields are *absent*, which copying spans
/// never learns, so they rule the shortcut out.
pub(super) fn fast_eligible(
    rules: &[CompiledRule],
    schema: Option<&CompiledSchema>,
    opts: Opts,
) -> bool {
    if !rules.is_empty() {
        return false;
    }
    let Some(schema) = schema else {
        return false;
    };
    matches!(schema.ty, None | Some(Ty::Object))
        && schema.enum_values.is_none()
        && schema.content.is_none()
        && schema.items.is_none()
        && schema.default.is_none()
        && schema.required.is_empty()
        && !(opts.apply_defaults && schema.properties.iter().any(|(_, s)| s.default.is_some()))
}

/// The mapping-only subset `project_fast` can serve: no schema, and every rule lifts one
/// top-level field to one output key with no default. The output is then a subset of the
/// input's top-level fields, which a span walk can assemble without building a `Value` at
/// all. Deeper source paths, nested output keys and defaults keep the general path.
pub(super) fn compile_projection(
    rules: &[CompiledRule],
    schema: Option<&CompiledSchema>,
) -> Option<Vec<FastMapRule>> {
    if rules.is_empty() || schema.is_some() {
        return None;
    }
    rules
        .iter()
        .map(|rule| {
            let [Seg::Key(from)] = rule.from.segs.as_slice() else {
                return None;
            };
            if rule.out.len() != 1 || rule.default.is_some() {
                return None;
            }
            Some(FastMapRule {
                from: from.clone(),
                out: serde_json::to_vec(&rule.out[0]).ok()?,
            })
        })
        .collect()
}

impl Compiled {
    pub(super) fn new(config: &TransformMiddleware) -> anyhow::Result<Self> {
        #[cfg(not(feature = "zen"))]
        if config.expression.is_some() {
            anyhow::bail!("transform middleware: 'expression' requires the 'zen' Cargo feature");
        }
        if config.schema.is_some() && config.schema_file.is_some() {
            anyhow::bail!("transform middleware: set either 'schema' or 'schema_file', not both");
        }

        let mut rules = Vec::with_capacity(config.mapping.len());
        for (out_key, rule) in &config.mapping {
            if out_key.is_empty() {
                anyhow::bail!("transform middleware: mapping output key must not be empty");
            }
            let out: Vec<String> = out_key.split('.').map(str::to_string).collect();
            if out.iter().any(String::is_empty) {
                anyhow::bail!("transform middleware: empty segment in output key '{out_key}'");
            }
            let (default, required) = match rule {
                MappingRule::Path(_) => (None, false),
                MappingRule::Detailed(d) => (d.default.clone(), d.required),
            };
            rules.push(CompiledRule {
                out,
                from: CompiledPath::parse(rule.path())?,
                default,
                required,
            });
        }
        // Deterministic order: config is a map, so iteration order is otherwise random
        // and overlapping keys ("a" and "a.b") would resolve inconsistently.
        rules.sort_by(|a, b| a.out.cmp(&b.out));

        // Read once, at startup. The hot path never touches the filesystem.
        let schema_value = match (&config.schema, &config.schema_file) {
            (Some(inline), _) => Some(inline.clone()),
            (_, Some(path)) => {
                let raw = std::fs::read_to_string(path).map_err(|e| {
                    anyhow::anyhow!("transform middleware: cannot read schema file '{path}': {e}")
                })?;
                Some(serde_json::from_str(&raw).map_err(|e| {
                    anyhow::anyhow!(
                        "transform middleware: schema file '{path}' is not valid JSON: {e}"
                    )
                })?)
            }
            _ => None,
        };
        let schema = match &schema_value {
            Some(v) => Some(CompiledSchema::compile(v)?),
            None => None,
        };

        let opts = Opts {
            coerce: config.coerce,
            apply_defaults: config.apply_defaults,
            coerce_empty_as_null: config.coerce_empty_as_null,
        };
        #[cfg(feature = "zen")]
        let expression = config
            .expression
            .as_deref()
            .map(compile_expression)
            .transpose()
            .map_err(|error| {
                anyhow::anyhow!("transform middleware: invalid expression: {error}")
            })?;
        #[cfg(feature = "zen")]
        let expression_uses_metadata = expression.as_ref().is_some_and(|expression| {
            expression.bytecode().iter().any(|opcode| match opcode {
                Opcode::FetchEnv(name) => name.as_ref() == "meta",
                Opcode::FetchFast(targets) => {
                    targets.iter().find_map(|target| match target {
                        FetchFastTarget::String(name) => Some(name.as_ref()),
                        _ => None,
                    }) == Some("meta")
                }
                _ => false,
            })
        });
        let fast_map = compile_projection(&rules, schema.as_ref());
        Ok(Self {
            fast_eligible: config.expression.is_none()
                && (fast_eligible(&rules, schema.as_ref(), opts) || fast_map.is_some()),
            #[cfg(feature = "zen")]
            expression_only: expression.is_some() && rules.is_empty() && schema.is_none(),
            take_inputs: paths_are_disjoint(&rules),
            fast_map,
            sort_keys: map_sorts_keys(),
            rules,
            #[cfg(feature = "zen")]
            expression,
            #[cfg(feature = "zen")]
            expression_uses_metadata,
            schema,
            opts,
            on_error: config.on_error,
        })
    }

    /// True when neither stage is configured; the message is then never parsed.
    pub(super) fn is_noop(&self) -> bool {
        #[cfg(feature = "zen")]
        let no_expression = self.expression.is_none();
        #[cfg(not(feature = "zen"))]
        let no_expression = true;
        self.rules.is_empty() && self.schema.is_none() && no_expression
    }

    fn apply_mapping(&self, input: &mut Value) -> Result<Value, TransformError> {
        let mut out = Value::Object(Map::new());
        for rule in &self.rules {
            // `input` is dropped as soon as this returns, so a value no other rule reads
            // through can be moved out rather than deep-cloned.
            let found = if self.take_inputs {
                rule.from.take(input)
            } else {
                rule.from.get(input).cloned()
            };
            let picked = match found {
                Some(found) => found,
                None => match &rule.default {
                    Some(default) => default.clone(),
                    None if rule.required => {
                        return Err(TransformError::new(
                            rule.from.spec.clone(),
                            ErrorKind::MissingRequired,
                            format!(
                                "required source field is missing (mapped to '{}')",
                                rule.out.join(".")
                            ),
                        ))
                    }
                    // Optional and absent: leave the output key out entirely.
                    None => continue,
                },
            };
            insert_at(&mut out, &rule.out, picked)?;
        }
        Ok(out)
    }

    /// Rewrites the payload field by field, copying the verbatim JSON span of every field
    /// the schema would not change and parsing only the ones that need work. Those go
    /// through the same `apply` as the normal path, so nesting, `items`, `enum` and
    /// `contentSchema` all behave identically — this decides *whether* a field is worth
    /// parsing, never *how* it is transformed.
    ///
    /// `None` means the payload's shape rules the shortcut out and the caller should fall
    /// back; `Some(Err(_))` is a real transform failure and must not be retried slowly.
    pub(super) fn transform_fast(
        &self,
        schema: &CompiledSchema,
        payload: &[u8],
    ) -> Option<Result<Vec<u8>, TransformError>> {
        // Not an object, or a key we cannot borrow because it carried escapes.
        let RawPairs(mut pairs) = serde_json::from_slice::<RawPairs>(payload).ok()?;

        // The output has to carry its keys the way the normal path's `Map` would order
        // them. Insertion order needs no work: it is the order they were just read in.
        if self.sort_keys {
            pairs.sort_by(|a, b| a.0.cmp(b.0));
        }

        // A `Value` parse collapses duplicate keys (last wins) where copying spans would
        // emit both, so those rare payloads go the normal way. Quadratic, but objects are
        // narrow and this runs once per message.
        if pairs
            .iter()
            .enumerate()
            .any(|(i, (key, _))| pairs[..i].iter().any(|(seen, _)| seen == key))
        {
            return None;
        }

        let mut out = Vec::with_capacity(payload.len() + payload.len() / 2);
        out.push(b'{');
        for (i, (key, raw)) in pairs.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'"');
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(b"\":");

            let sub = schema
                .properties
                .binary_search_by(|(name, _)| name.as_str().cmp(key))
                .ok()
                .map(|idx| &schema.properties[idx].1);

            // `coerce_empty_as_null` turns this field into a null, which both shortcuts
            // below would otherwise wave through as an ordinary string.
            let empty_string = self.opts.coerce_empty_as_null && raw.get() == "\"\"";

            match sub {
                // Embedded JSON with nothing to check afterwards: unescape the string and
                // emit the document it carried, without ever building it.
                Some(sub) if !empty_string && sub.is_plain_content_decode(raw.get()) => {
                    // serde_json does the unescaping, so `😀` and friends are
                    // handled exactly as the normal path handles them.
                    let text: std::borrow::Cow<'_, str> = match serde_json::from_str(raw.get()) {
                        Ok(text) => text,
                        Err(e) => {
                            return Some(Err(TransformError::new(
                                format!("$.{key}"),
                                ErrorKind::Parse,
                                format!("field is not valid JSON: {e}"),
                            )))
                        }
                    };
                    match serde_json::from_str::<&serde_json::value::RawValue>(&text) {
                        Ok(document) => out.extend_from_slice(document.get().as_bytes()),
                        Err(e) => {
                            return Some(Err(TransformError::new(
                                format!("$.{key}"),
                                ErrorKind::Content,
                                format!(
                                    "contentMediaType is JSON but the string does not parse: {e}"
                                ),
                            )))
                        }
                    }
                }
                // The schema has something to say about this field and the raw bytes do
                // not already satisfy it: parse just this field and transform it.
                Some(sub) if empty_string || !sub.is_passthrough(raw.get()) => {
                    // A text-typed column becomes its typed token straight from the span.
                    // `false` means the field is not a shape that serves, or the rewrite
                    // failed and the general path has to raise the error.
                    if !empty_string && sub.coerce_scalar_raw(raw.get(), self.opts, &mut out) {
                        continue;
                    }
                    let mut value: Value = match serde_json::from_str(raw.get()) {
                        Ok(value) => value,
                        Err(e) => {
                            return Some(Err(TransformError::new(
                                format!("$.{key}"),
                                ErrorKind::Parse,
                                format!("field is not valid JSON: {e}"),
                            )))
                        }
                    };
                    let mut crumbs = vec![Crumb::Key(key)];
                    if let Err(e) = sub.apply(&mut value, &mut crumbs, self.opts) {
                        return Some(Err(e));
                    }
                    if let Err(e) = serde_json::to_writer(&mut out, &value) {
                        return Some(Err(TransformError::new(
                            format!("$.{key}"),
                            ErrorKind::Parse,
                            format!("transformed value could not be serialized: {e}"),
                        )));
                    }
                }
                // Unmentioned by the schema, or already satisfying it.
                _ => out.extend_from_slice(raw.get().as_bytes()),
            }
        }
        out.push(b'}');
        Some(Ok(out))
    }

    /// Assembles a projection from the input's top-level spans: each picked field's
    /// verbatim JSON bytes are copied straight through. Fields the projection drops are
    /// never parsed — which for a dropped blob is the bulk of the message's cost — and
    /// nothing is cloned or turned into a `Value`.
    ///
    /// Keys come out sorted by output key, because `rules` is sorted that way and every
    /// output key is a single segment. That is what a `serde_json::Map` produces either
    /// way here — sorted (`BTreeMap`) or in insertion order (`IndexMap`, fed in that same
    /// order) — so unlike `transform_fast` this path needs no `sort_keys`.
    ///
    /// `None` means the payload's shape rules the shortcut out and the caller should fall
    /// back; `Some(Err(_))` is a real transform failure and must not be retried slowly.
    pub(super) fn project_fast(
        &self,
        fast_map: &[FastMapRule],
        payload: &[u8],
    ) -> Option<Result<Vec<u8>, TransformError>> {
        // Not an object, or a key we cannot borrow because it carried escapes.
        let RawPairs(pairs) = serde_json::from_slice::<RawPairs>(payload).ok()?;

        let mut out = Vec::with_capacity(payload.len());
        out.push(b'{');
        let mut first = true;
        for (rule, fast) in self.rules.iter().zip(fast_map) {
            // Searching from the back makes a duplicated key resolve to its last value,
            // the way collapsing the payload into a `Value` would.
            let picked = pairs
                .iter()
                .rev()
                .find(|(key, _)| *key == fast.from)
                .map(|(_, raw)| raw.get());
            let Some(raw) = picked else {
                if rule.required {
                    return Some(Err(TransformError::new(
                        rule.from.spec.clone(),
                        ErrorKind::MissingRequired,
                        format!(
                            "required source field is missing (mapped to '{}')",
                            rule.out.join(".")
                        ),
                    )));
                }
                // Optional and absent: leave the output key out entirely.
                continue;
            };
            if !first {
                out.push(b',');
            }
            first = false;
            out.extend_from_slice(&fast.out);
            out.push(b':');
            out.extend_from_slice(raw.as_bytes());
        }
        out.push(b'}');
        Some(Ok(out))
    }

    /// The expression on its own, with no mapping or schema stage beside it.
    ///
    /// Reading the payload straight into the engine's own `Variable` skips the
    /// intermediate `serde_json::Value` the general path builds only to convert away
    /// again, and lets `meta` be inserted without the deep clone that path needs.
    ///
    /// The result still travels home through `Value`: serializing a `Variable` directly
    /// would put its `Decimal` through `to_u64`, which truncates, so `100.5` would land
    /// as `100`.
    #[cfg(feature = "zen")]
    fn transform_expression_only(
        &self,
        message: &mut CanonicalMessage,
    ) -> Result<(), TransformError> {
        let expression = self
            .expression
            .as_ref()
            .expect("expression_only implies an expression");

        let input: Variable = serde_json::from_slice(&message.payload).map_err(|e| {
            TransformError::new(
                "$".to_string(),
                ErrorKind::Parse,
                format!("payload is not valid JSON: {e}"),
            )
        })?;
        let Some(object) = input.as_object() else {
            return Err(TransformError::new(
                "$".to_string(),
                ErrorKind::Expression,
                "expression requires a structured JSON object payload",
            ));
        };

        if self.expression_uses_metadata {
            let meta = message
                .metadata
                .iter()
                .map(|(key, value)| {
                    (
                        Symbol::from(key.as_str()),
                        Variable::String(value.as_str().into()),
                    )
                })
                .collect();
            object
                .borrow_mut()
                .insert(Symbol::from("meta"), Variable::from_object(meta));
        }
        drop(object);

        let evaluated = expression.evaluate(input).map_err(|error| {
            TransformError::new("$".to_string(), ErrorKind::Expression, error.to_string())
        })?;
        self.write_payload(message, &Value::from(evaluated))
    }

    /// Serializes the reshaped document back over the message's payload. Sized from the
    /// input rather than left at serde_json's 128-byte default: the output tracks the
    /// input closely, so this is usually the only allocation the write side makes.
    fn write_payload(
        &self,
        message: &mut CanonicalMessage,
        value: &Value,
    ) -> Result<(), TransformError> {
        let mut bytes = Vec::with_capacity(message.payload.len() + message.payload.len() / 2);
        serde_json::to_writer(&mut bytes, value).map_err(|e| {
            TransformError::new(
                "$".to_string(),
                ErrorKind::Parse,
                format!("transformed value could not be serialized: {e}"),
            )
        })?;
        message.payload = Bytes::from(bytes);
        Ok(())
    }

    /// Parses once, reshapes, serialises once.
    pub(super) fn transform(&self, message: &mut CanonicalMessage) -> Result<(), TransformError> {
        if self.fast_eligible {
            let fast = match (&self.schema, &self.fast_map) {
                (Some(schema), _) => self.transform_fast(schema, &message.payload),
                (None, Some(fast_map)) => self.project_fast(fast_map, &message.payload),
                (None, None) => None,
            };
            if let Some(result) = fast {
                message.payload = Bytes::from(result?);
                return Ok(());
            }
        }

        #[cfg(feature = "zen")]
        if self.expression_only {
            return self.transform_expression_only(message);
        }

        let mut input: Value = serde_json::from_slice(&message.payload).map_err(|e| {
            TransformError::new(
                "$".to_string(),
                ErrorKind::Parse,
                format!("payload is not valid JSON: {e}"),
            )
        })?;

        let mut value = if self.rules.is_empty() {
            input
        } else {
            self.apply_mapping(&mut input)?
        };

        #[cfg(feature = "zen")]
        if let Some(expression) = &self.expression {
            value.as_object().ok_or_else(|| {
                TransformError::new(
                    "$".to_string(),
                    ErrorKind::Expression,
                    "expression requires a structured JSON object payload",
                )
            })?;
            let mut expression_context;
            let expression_input = if self.expression_uses_metadata {
                expression_context = value.clone();
                let object = expression_context
                    .as_object_mut()
                    .expect("expression input was checked above");
                object.insert(
                    "meta".to_string(),
                    Value::Object(
                        message
                            .metadata
                            .iter()
                            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                            .collect(),
                    ),
                );
                &expression_context
            } else {
                &value
            };
            value = expression
                .evaluate(Variable::from(expression_input))
                .map(Value::from)
                .map_err(|error| {
                    TransformError::new("$".to_string(), ErrorKind::Expression, error.to_string())
                })?;
        }

        if let Some(schema) = &self.schema {
            let mut crumbs = Vec::new();
            schema.apply(&mut value, &mut crumbs, self.opts)?;
        }

        self.write_payload(message, &value)
    }

    /// Applies the configured policy to a failure. `Ok` keeps the message (annotated),
    /// `Err` rejects it.
    pub(super) fn handle_failure(
        &self,
        message: &mut CanonicalMessage,
        error: TransformError,
    ) -> Result<(), TransformError> {
        match self.on_error {
            TransformErrorPolicy::PassThrough => {
                message
                    .metadata
                    .insert(TRANSFORM_ERROR_KEY.to_string(), error.to_string());
                Ok(())
            }
            TransformErrorPolicy::Reject => Err(error),
        }
    }
}

#[cfg(all(test, feature = "zen"))]
mod tests {
    use super::*;
    use crate::models::TransformMiddleware;

    fn message(payload: &str, metadata: &[(&str, &str)]) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(payload.as_bytes().to_vec(), None);
        for (key, value) in metadata {
            message
                .metadata
                .insert((*key).to_string(), (*value).to_string());
        }
        message
    }

    /// The direct-to-`Variable` route must reproduce the general `Value` path byte for
    /// byte. Numbers are the reason this test exists: serializing a `Variable` straight
    /// out would put its `Decimal` through `to_u64`, which truncates, so `100.5` would
    /// silently land as `100`.
    #[test]
    fn the_expression_only_route_matches_the_value_path() {
        let payloads = [
            r#"{"a":1,"b":"x"}"#,
            r#"{"a":100.5,"b":"x"}"#,
            r#"{"a":-0.001,"b":"x"}"#,
            r#"{"a":1.0,"b":"x"}"#,
            r#"{"a":0,"b":"x"}"#,
            r#"{"a":0.1,"b":"x"}"#,
            r#"{"a":12345678901234,"b":"x"}"#,
            r#"{"a":-42,"b":"x"}"#,
            r#"{"a":1e3,"b":"x"}"#,
            r#"{"a":2.5000,"b":"x"}"#,
            r#"{"a":"7.25","b":"x"}"#,
            r#"{"a":[1,2.5],"b":"x"}"#,
            r#"{"a":{"c":3.75},"b":"x"}"#,
            r#"{"a":true,"b":null}"#,
            r#"{"a":null,"b":"héllo 世界"}"#,
            r#"{"a":1,"b":"quote\"and\\slash"}"#,
            // A payload carrying its own `meta`: both routes overwrite it with the
            // message metadata, and must agree on that.
            r#"{"a":1,"b":"x","meta":{"kind":"from-payload"}}"#,
            r#"{"a":1,"b":"x","meta":"not an object"}"#,
        ];
        let expressions = [
            "{value: a, other: b}",
            "{value: a, kind: meta.kind}",
            "{keys: keys($), value: a}",
        ];
        let metadata = &[("kind", "order")][..];

        for expression in expressions {
            let config = TransformMiddleware {
                expression: Some(expression.to_string()),
                ..Default::default()
            };
            let fast = Compiled::new(&config).expect("config compiles");
            assert!(
                fast.expression_only,
                "{expression} should take the fast route"
            );
            let mut general = Compiled::new(&config).expect("config compiles");
            general.expression_only = false;

            for payload in payloads {
                let mut left = message(payload, metadata);
                let mut right = message(payload, metadata);
                match (fast.transform(&mut left), general.transform(&mut right)) {
                    (Ok(()), Ok(())) => assert_eq!(
                        left.payload, right.payload,
                        "{expression} disagreed on {payload}"
                    ),
                    (Err(_), Err(_)) => {}
                    (left, right) => {
                        panic!("{expression} on {payload}: fast={left:?} general={right:?}")
                    }
                }
            }
        }
    }

    /// A payload that is not a JSON object is rejected the same way on both routes.
    #[test]
    fn the_expression_only_route_rejects_non_object_payloads_alike() {
        let config = TransformMiddleware {
            expression: Some("{value: a}".to_string()),
            ..Default::default()
        };
        let fast = Compiled::new(&config).unwrap();
        let mut general = Compiled::new(&config).unwrap();
        general.expression_only = false;

        for payload in [r#"[1,2]"#, r#""text""#, r#"7"#, r#"not json"#] {
            let mut left = message(payload, &[]);
            let mut right = message(payload, &[]);
            assert_eq!(
                fast.transform(&mut left).is_err(),
                general.transform(&mut right).is_err(),
                "{payload}"
            );
        }
    }
}
