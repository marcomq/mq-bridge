//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! What a plugin's configuration schema is, and how a URI maps onto it.
//!
//! A plugin describes its `config` object as a JSON Schema. The host reads it
//! for two things: rendering a form, and turning a URI into configuration with
//! the right types. Both come from one document on purpose — a field renamed in
//! a form description but not in a URI mapping is a bug nobody notices.
//!
//! # URI positions
//!
//! Where a field sits in a URI is a per-property annotation, `x-mqb-uri`. JSON
//! Schema reserves the `x-` prefix for annotations, so a validator ignores it.
//!
//! | `x-mqb-uri` | Gets | Example from `rp://user@host:9092/orders?group=g` |
//! |---|---|---|
//! | `subscheme` | the scheme's part after `+` | nothing; see below |
//! | `origin` | scheme, userinfo, host and port | `rp://user@host:9092` |
//! | `url` | everything before `?` | `rp://user@host:9092/orders` |
//! | `path` | the path, without its leading `/` | `orders` |
//! | `query` | the query parameter of the same name (the default) | `g` |
//!
//! Each position but `query` may be claimed by at most one property. Anything
//! unannotated is a query parameter, which is also what a plugin that describes
//! nothing gets.
//!
//! # Compound schemes
//!
//! A plugin that is a gateway to a family of protocols reaches them through a
//! `plugin+protocol` scheme, the spelling `git+ssh` and `postgresql+psycopg2`
//! made familiar. `subscheme` is the part after the `+`, and the rest of the URI
//! then describes the inner protocol rather than the plugin, so `origin` and
//! `url` are handed over carrying the inner scheme:
//!
//! | From `rp+mqtt://host:1883/orders` | |
//! |---|---|
//! | `subscheme` | `mqtt` |
//! | `origin` | `mqtt://host:1883` |
//! | `url` | `mqtt://host:1883/orders` |
//! | `path` | `orders` |
//!
//! A scheme cannot hold `_` ([RFC 3986] &sect;3.1), so a plugin whose protocol
//! names do is the one that maps `-` back onto them.
//!
//! [RFC 3986]: https://www.rfc-editor.org/rfc/rfc3986#section-3.1
//!
//! ```json
//! {
//!   "type": "object",
//!   "properties": {
//!     "url":        { "type": "string",  "x-mqb-uri": "origin" },
//!     "topic":      { "type": "string",  "x-mqb-uri": "path" },
//!     "group":      { "type": "string",  "default": "mq-bridge" },
//!     "batch_size": { "type": "integer", "default": 100 }
//!   },
//!   "required": ["url", "topic"]
//! }
//! ```
//!
//! # Types
//!
//! A URI carries only text, so the declared `type` is what makes
//! `?batch_size=100` the number `100` rather than the string `"100"`. Without a
//! schema every value stays a string, which is why a plugin with a non-string
//! config field is unusable from a URI until it describes one.
//!
//! # Validation
//!
//! A schema is read and mapped, never enforced, unless
//! [`VALIDATE_VAR`] says otherwise — see [`validation_enabled`].

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context};
use serde_json::{Map, Value};

/// The per-property annotation naming a field's place in a URI.
pub const URI_ANNOTATION: &str = "x-mqb-uri";

/// Schema-level flag: read undeclared query values as `true`/`false`, integers
/// or decimals instead of text.
pub const INFER_SCALARS_ANNOTATION: &str = "x-mqb-uri-infer-scalars";

/// Where a field's value comes from in a URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UriPosition {
    /// What the scheme carries after a `+`, in a `plugin+protocol` scheme.
    Subscheme,
    /// Scheme, userinfo, host and port: everything before the path.
    Origin,
    /// Everything before the query string.
    Url,
    /// The path, with its leading `/` removed.
    Path,
    /// A query parameter of the same name. The default, so never written out.
    Query,
}

impl UriPosition {
    fn parse(text: &str) -> anyhow::Result<Self> {
        match text {
            "subscheme" => Ok(Self::Subscheme),
            "origin" => Ok(Self::Origin),
            "url" => Ok(Self::Url),
            "path" => Ok(Self::Path),
            "query" => Ok(Self::Query),
            other => bail!(
                "`{URI_ANNOTATION}: {other}` is not a URI position; \
                 expected subscheme, origin, url, path or query"
            ),
        }
    }
}

/// The scalar kinds a URI's text can be turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldType {
    Integer,
    Number,
    Boolean,
    /// A comma-separated list. Its element type is kept separately, in
    /// [`UriSchema::items`], so this stays a flat `Copy` enum.
    Array,
    /// Anything else, including an undeclared or ambiguous type.
    Text,
}

impl FieldType {
    /// Reads `type`, tolerating the `["integer", "null"]` spelling an optional
    /// field gets. A union of two real types is ambiguous, so it stays text.
    fn of(property: &Value) -> Self {
        let named = |name: &str| match name {
            "integer" => Self::Integer,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            _ => Self::Text,
        };
        match property.get("type") {
            Some(Value::String(name)) => named(name),
            Some(Value::Array(names)) => {
                let mut real = names.iter().filter(|n| n.as_str() != Some("null"));
                match (real.next(), real.next()) {
                    (Some(Value::String(name)), None) => named(name),
                    _ => Self::Text,
                }
            }
            _ => Self::Text,
        }
    }

    fn coerce(self, field: &str, raw: &str, item: Self) -> anyhow::Result<Value> {
        let expected = match self {
            Self::Text => return Ok(Value::String(raw.to_string())),
            Self::Array => {
                let items = raw
                    .split(',')
                    .filter(|entry| !entry.is_empty())
                    .map(|entry| item.coerce(field, entry, Self::Text));
                return Ok(Value::Array(items.collect::<anyhow::Result<_>>()?));
            }
            Self::Integer => {
                if let Ok(number) = raw.parse::<i64>() {
                    return Ok(Value::from(number));
                }
                "an integer"
            }
            Self::Number => {
                if let Ok(number) = raw.parse::<f64>() {
                    return serde_json::Number::from_f64(number)
                        .map(Value::Number)
                        .ok_or_else(|| anyhow!("`{field}={raw}` is not a finite number"));
                }
                "a number"
            }
            Self::Boolean => match raw {
                "true" | "1" | "yes" => return Ok(Value::Bool(true)),
                "false" | "0" | "no" => return Ok(Value::Bool(false)),
                _ => "true or false",
            },
        };
        bail!("`{field}={raw}` is not {expected}, which is what this endpoint expects")
    }
}

/// A plugin's configuration schema, reduced to what a URI needs from it.
///
/// [`Default`] is the mapping for a plugin that describes nothing: no field has
/// a declared type, and the whole URI up to the query string becomes `url`.
#[derive(Debug, Clone, Default)]
pub struct UriSchema {
    subscheme: Option<String>,
    origin: Option<String>,
    url: Option<String>,
    path: Option<String>,
    types: HashMap<String, FieldType>,
    /// Element type of each field that is a list, keyed the same way.
    items: HashMap<String, FieldType>,
    /// Whether an undeclared field's value is guessed rather than kept as text.
    infer_scalars: bool,
}

impl UriSchema {
    /// Reads the positions and types out of a validated schema.
    ///
    /// Call [`validate`] first: a position this build cannot read is ignored
    /// here, so an unchecked schema maps silently rather than loudly.
    pub fn from_schema(schema: &Value) -> Self {
        let mut mapping = Self {
            infer_scalars: schema
                .get(INFER_SCALARS_ANNOTATION)
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..Self::default()
        };
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return mapping;
        };
        for (field, property) in properties {
            let declared = FieldType::of(property);
            mapping.types.insert(field.clone(), declared);
            if declared == FieldType::Array {
                if let Some(items) = property.get("items") {
                    mapping.items.insert(field.clone(), FieldType::of(items));
                }
            }
            let position = property
                .get(URI_ANNOTATION)
                .and_then(Value::as_str)
                .and_then(|text| UriPosition::parse(text).ok());
            let slot = match position {
                Some(UriPosition::Subscheme) => &mut mapping.subscheme,
                Some(UriPosition::Origin) => &mut mapping.origin,
                Some(UriPosition::Url) => &mut mapping.url,
                Some(UriPosition::Path) => &mut mapping.path,
                Some(UriPosition::Query) | None => continue,
            };
            slot.get_or_insert_with(|| field.clone());
        }
        mapping
    }

    /// Whether any field claimed a position outside the query string.
    fn positions_a_field(&self) -> bool {
        self.subscheme.is_some()
            || self.origin.is_some()
            || self.url.is_some()
            || self.path.is_some()
    }

    /// Turns a URI into a configuration object.
    ///
    /// Query parameters are applied last, so `?url=...` still overrides whatever
    /// the URI's own shape produced — the escape hatch for a transport whose
    /// address a URI cannot spell.
    pub fn config_from_uri(&self, uri: &str) -> anyhow::Result<Map<String, Value>> {
        let parsed = url::Url::parse(uri).with_context(|| {
            let shown = crate::support::redact::url_password(uri);
            format!("`{shown}` is not a URI")
        })?;
        let before_query = uri.split('?').next().unwrap_or(uri);
        // Everything after the `+` describes the inner protocol, so that is the
        // scheme the address is handed over with.
        let inner = subscheme_of(parsed.scheme());

        let mut config = Map::new();
        if self.positions_a_field() {
            if let (Some(field), Some(inner)) = (&self.subscheme, inner) {
                config.insert(field.clone(), self.coerce(field, inner)?);
            }
            if let Some(field) = self
                .origin
                .as_ref()
                .filter(|_| !parsed.authority().is_empty())
            {
                let scheme = inner.unwrap_or_else(|| parsed.scheme());
                let origin = format!("{scheme}://{}", parsed.authority());
                config.insert(field.clone(), Value::String(origin));
            }
            if let Some(field) = &self.url {
                let url = match inner {
                    Some(inner) => format!("{inner}{}", &before_query[parsed.scheme().len()..]),
                    None => before_query.to_string(),
                };
                config.insert(field.clone(), Value::String(url));
            }
            if let Some(field) = &self.path {
                let path = parsed.path().trim_start_matches('/');
                if !path.is_empty() {
                    config.insert(field.clone(), self.coerce(field, path)?);
                }
            }
        } else {
            // No schema, or one that positions nothing: the address is the URI.
            config.insert("url".into(), Value::String(before_query.to_string()));
        }

        for (key, value) in parsed.query_pairs() {
            let coerced = self.coerce(&key, &value)?;
            config.insert(key.into_owned(), coerced);
        }
        Ok(config)
    }

    /// Reads one value as the field's declared type. Undeclared fields stay
    /// text unless the schema opted in to [`INFER_SCALARS_ANNOTATION`].
    fn coerce(&self, field: &str, raw: &str) -> anyhow::Result<Value> {
        let Some(declared) = self.types.get(field).copied() else {
            return Ok(if self.infer_scalars {
                infer_scalar(raw)
            } else {
                Value::String(raw.to_string())
            });
        };
        let item = self.items.get(field).copied().unwrap_or(FieldType::Text);
        declared.coerce(field, raw, item)
    }
}

/// Guesses a scalar from text. Leading zeros and non-decimal spellings stay
/// text, so an id like `007` or a version like `1.0.0` survives unchanged.
fn infer_scalar(raw: &str) -> Value {
    match raw {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let leading_zero = digits.len() > 1 && digits.starts_with('0') && !digits.starts_with("0.");
    let decimal = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1
        && !digits.starts_with('.')
        && !digits.ends_with('.');
    if !decimal || leading_zero {
        return Value::String(raw.to_string());
    }
    if let Ok(number) = raw.parse::<i64>() {
        return Value::from(number);
    }
    raw.parse::<f64>()
        .ok()
        .filter(|_| raw.contains('.'))
        .and_then(serde_json::Number::from_f64)
        .map_or_else(|| Value::String(raw.to_string()), Value::Number)
}

/// The protocol a `plugin+protocol` scheme names, if it names one.
fn subscheme_of(scheme: &str) -> Option<&str> {
    scheme
        .split_once('+')
        .map(|(_, inner)| inner)
        .filter(|inner| !inner.is_empty())
}

/// How a URI maps onto the configuration of whichever endpoint is registered
/// under `name`.
///
/// A factory that describes nothing — and a name nothing is registered under —
/// gets [`UriSchema::default`], the mapping that predates configuration schemas,
/// so a caller needs no branch of its own. Loaded plugins and statically linked
/// extensions answer through the same registry, so both benefit.
pub fn endpoint_uri_schema(name: &str) -> UriSchema {
    crate::extensions::get_endpoint_factory(name)
        .and_then(|factory| factory.config_schema())
        .map(|schema| UriSchema::from_schema(&flatten(&schema)))
        .unwrap_or_default()
}

/// Environment variable switching on configuration validation.
pub const VALIDATE_VAR: &str = "MQB_PLUGIN_VALIDATE_CONFIG";

/// Whether a custom endpoint's configuration is checked against its own schema
/// before the endpoint is opened.
///
/// **Off by default.** The schema belongs to the endpoint, not to the route, and
/// one more expressive than [`validate_config`] can check would reject
/// configuration the endpoint itself accepts. Turn it on while writing a plugin,
/// or to turn a confusing rejection from inside an endpoint into a message that
/// names the field.
pub fn validation_enabled() -> bool {
    validation_enabled_from(std::env::var(VALIDATE_VAR).ok().as_deref())
}

fn validation_enabled_from(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "on" | "yes")
    )
}

/// Checks a custom endpoint's configuration against the schema its factory
/// declared, when [`validation_enabled`].
///
/// A factory that declares nothing, and a schema needing more of JSON Schema
/// than [`validate_config`] covers, both pass: an endpoint is not made
/// unusable by how it chose to describe itself.
pub fn check_endpoint_config(name: &str, config: &Value) -> anyhow::Result<()> {
    if !validation_enabled() {
        return Ok(());
    }
    let Some(schema) =
        crate::extensions::get_endpoint_factory(name).and_then(|factory| factory.config_schema())
    else {
        return Ok(());
    };
    match validate_config(&flatten(&schema), config) {
        Ok(()) => Ok(()),
        Err(Unenforceable(why)) => {
            tracing::warn!(
                endpoint = name,
                %why,
                "{VALIDATE_VAR} is set but this endpoint's schema cannot be checked; \
                 passing its configuration through",
            );
            Ok(())
        }
        Err(Invalid(error)) => Err(error.context(format!("endpoint `{name}` configuration"))),
    }
}

/// Why [`validate_config`] said no.
#[derive(Debug)]
pub enum ConfigRejected {
    /// The configuration does not match the schema.
    Invalid(anyhow::Error),
    /// The schema needs more of JSON Schema than the host can check. The
    /// author's problem, not the user's, so a caller enforcing a schema it did
    /// not write should pass the configuration through.
    Unenforceable(anyhow::Error),
}

use ConfigRejected::{Invalid, Unenforceable};

impl std::fmt::Display for ConfigRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid(error) => write!(f, "{error:#}"),
            Unenforceable(error) => write!(f, "schema cannot be checked: {error:#}"),
        }
    }
}

impl std::error::Error for ConfigRejected {}

/// Checks `config` against `schema`, whatever [`VALIDATE_VAR`] says.
///
/// Covers `type`, `required`, `enum`, `items` and nested `properties` — the
/// subset the `transform` middleware already validates against — plus unknown
/// top-level fields when the schema sets `additionalProperties: false`.
pub fn validate_config(schema: &Value, config: &Value) -> Result<(), ConfigRejected> {
    let check =
        crate::middleware::transform::JsonSchemaCheck::compile(schema).map_err(Unenforceable)?;
    reject_unknown_fields(schema, config).map_err(Invalid)?;
    check.check(config).map_err(Invalid)
}

/// A misspelled field is the mistake worth catching, and the subset above cannot
/// see it: an unknown property is simply one the schema says nothing about.
fn reject_unknown_fields(schema: &Value, config: &Value) -> anyhow::Result<()> {
    if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
        return Ok(());
    }
    let (Some(properties), Some(config)) = (
        schema.get("properties").and_then(Value::as_object),
        config.as_object(),
    ) else {
        return Ok(());
    };
    let unknown: Vec<&str> = config
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let known = properties.keys().cloned().collect::<Vec<_>>().join(", ");
    bail!(
        "unknown field{} {}; this endpoint takes {known}",
        if unknown.len() == 1 { "" } else { "s" },
        unknown
            .iter()
            .map(|field| format!("`{field}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Checks that a schema is one the host can actually use.
///
/// Rejects at load time what would otherwise surface as a silently wrong form
/// or a URI field that goes nowhere. It is not a JSON Schema validator: a
/// document this accepts may still describe the plugin's configuration badly.
pub fn validate(schema: &Value) -> anyhow::Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("a configuration schema must be a JSON object"))?;
    if let Some(declared) = object.get("type") {
        if declared.as_str() != Some("object") {
            bail!("a configuration schema must describe an object, not {declared}");
        }
    }
    if let Some(flag) = object.get(INFER_SCALARS_ANNOTATION) {
        if !flag.is_boolean() {
            bail!("`{INFER_SCALARS_ANNOTATION}` must be true or false, not {flag}");
        }
    }
    let Some(properties) = object.get("properties") else {
        return Ok(());
    };
    let properties = properties
        .as_object()
        .ok_or_else(|| anyhow!("`properties` must be an object"))?;

    let mut claimed: HashMap<UriPosition, &str> = HashMap::new();
    for (field, property) in properties {
        if !property.is_object() {
            bail!("property `{field}` must be an object");
        }
        let Some(annotation) = property.get(URI_ANNOTATION) else {
            continue;
        };
        let text = annotation
            .as_str()
            .ok_or_else(|| anyhow!("`{URI_ANNOTATION}` on `{field}` must be a string"))?;
        let position = UriPosition::parse(text).with_context(|| format!("property `{field}`"))?;
        if position == UriPosition::Query {
            continue;
        }
        if let Some(first) = claimed.insert(position, field) {
            bail!("`{first}` and `{field}` both claim the {text} of a URI; only one may");
        }
    }
    Ok(())
}

/// How deep [`flatten`] follows references before giving up on a cyclic schema.
const MAX_REF_DEPTH: usize = 16;

/// Rewrites a schema into the flat form a host reads: local `$ref`s are replaced
/// by what they point to, and a `oneOf`/`anyOf` of `const`s becomes an `enum`.
///
/// That is the shape `#[derive(JsonSchema)]` gives an enum field, so a plugin can
/// derive its schema without hand-writing one per enum. A schema that is already
/// flat comes back unchanged. Only the definitions a cycle still points at are kept.
pub fn flatten(schema: &Value) -> Value {
    let mut flat = flatten_node(schema, schema, &mut Vec::new());
    for key in ["$defs", "definitions"] {
        let Some(Value::Object(definitions)) = flat.as_object_mut().and_then(|o| o.remove(key))
        else {
            continue;
        };
        let prefix = format!("#/{key}/");
        let mut pending = Vec::new();
        references(&flat, &prefix, &mut pending);
        let mut kept = Map::new();
        while let Some(name) = pending.pop() {
            if kept.contains_key(&name) {
                continue;
            }
            if let Some(definition) = definitions.get(&name) {
                references(definition, &prefix, &mut pending);
                kept.insert(name, definition.clone());
            }
        }
        if !kept.is_empty() {
            flat[key] = Value::Object(kept);
        }
    }
    flat
}

/// Collects the definition names `node` refers to under `prefix`.
fn references(node: &Value, prefix: &str, names: &mut Vec<String>) {
    match node {
        Value::Object(object) => {
            for (key, value) in object {
                match value.as_str().filter(|_| key == "$ref") {
                    // A pointer into a definition still needs the whole definition.
                    Some(reference) => names.extend(
                        reference
                            .strip_prefix(prefix)
                            .and_then(|rest| rest.split('/').next())
                            .map(|name| name.replace("~1", "/").replace("~0", "~")),
                    ),
                    None => references(value, prefix, names),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| references(i, prefix, names)),
        _ => {}
    }
}

/// `path` holds the references being expanded; one already on it is a cycle and stays a `$ref`.
fn flatten_node<'a>(node: &'a Value, root: &'a Value, path: &mut Vec<&'a str>) -> Value {
    let object = match node {
        Value::Object(object) => object,
        Value::Array(items) => {
            return Value::Array(items.iter().map(|i| flatten_node(i, root, path)).collect())
        }
        other => return other.clone(),
    };
    let mut out = Map::new();
    let target = object
        .get("$ref")
        .and_then(Value::as_str)
        .filter(|reference| path.len() < MAX_REF_DEPTH && !path.contains(reference))
        .and_then(|reference| Some((reference, local_definition(root, reference)?)));
    if let Some((reference, definition)) = target {
        path.push(reference);
        if let Value::Object(resolved) = flatten_node(definition, root, path) {
            out = resolved;
        }
        path.pop();
    }
    for (key, value) in object {
        if key == "$ref" && target.is_some() {
            continue;
        }
        // A sibling of `$ref`, such as the field's own description, wins.
        out.insert(key.clone(), flatten_node(value, root, path));
    }
    for key in ["oneOf", "anyOf"] {
        let Some(constants) = out.get(key).and_then(constants_of) else {
            continue;
        };
        if constants.iter().all(Value::is_string) {
            out.entry("type").or_insert_with(|| Value::from("string"));
        }
        out.remove(key);
        out.insert("enum".to_string(), Value::Array(constants));
    }
    Value::Object(out)
}

/// The values of a union whose every branch is a `const`, else `None`.
fn constants_of(union: &Value) -> Option<Vec<Value>> {
    let branches = union.as_array().filter(|branches| !branches.is_empty())?;
    branches.iter().map(|b| b.get("const").cloned()).collect()
}

fn local_definition<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    ["/$defs/", "/definitions/"]
        .iter()
        .any(|prefix| pointer.starts_with(prefix))
        .then(|| root.pointer(pointer))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connect_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "x-mqb-uri": "origin" },
                "topic": { "type": "string", "x-mqb-uri": "path" },
                "group": { "type": "string" },
                "batch_size": { "type": "integer" },
                "tls": { "type": "boolean" },
                "acks": { "type": ["number", "null"] },
                "brokers": { "type": "array" },
                "partitions": { "type": "array", "items": { "type": "integer" } },
            },
            "required": ["url", "topic"],
        })
    }

    #[test]
    fn a_plugin_that_describes_nothing_keeps_the_whole_address_as_the_url() {
        let config = UriSchema::default()
            .config_from_uri("rp://host:9092/orders?group=g")
            .expect("map the uri");

        assert_eq!(config["url"], json!("rp://host:9092/orders"));
        assert_eq!(config["group"], json!("g"));
    }

    /// The point of the annotation: without it `orders` is only reachable as a
    /// query parameter, whatever the transport's own conventions are.
    #[test]
    fn an_annotated_schema_splits_the_address_from_the_path() {
        let config = UriSchema::from_schema(&connect_schema())
            .config_from_uri("rp://user@host:9092/orders?group=g")
            .expect("map the uri");

        assert_eq!(config["url"], json!("rp://user@host:9092"));
        assert_eq!(config["topic"], json!("orders"));
        assert_eq!(config["group"], json!("g"));
    }

    #[test]
    fn a_uri_that_cannot_be_parsed_is_reported_without_its_password() {
        let error = UriSchema::default()
            .config_from_uri("rp://user:hunter2@ho st:9092/orders")
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("rp://user:***@"), "{text}");
    }

    #[test]
    fn a_declared_type_is_what_makes_a_query_parameter_a_number() {
        let config = UriSchema::from_schema(&connect_schema())
            .config_from_uri(
                "rp://host/t?batch_size=100&tls=true&acks=0.5&brokers=a,b&partitions=0,1",
            )
            .expect("map the uri");

        assert_eq!(config["batch_size"], json!(100));
        assert_eq!(config["tls"], json!(true));
        assert_eq!(config["acks"], json!(0.5));
        assert_eq!(config["brokers"], json!(["a", "b"]));
        assert_eq!(config["partitions"], json!([0, 1]));
    }

    /// Passing `"abc"` on to a plugin expecting an integer buries the mistake in
    /// whatever the plugin's deserializer says; naming the field here does not.
    #[test]
    fn a_value_that_is_not_the_declared_type_is_rejected_by_name() {
        let error = UriSchema::from_schema(&connect_schema())
            .config_from_uri("rp://host/t?batch_size=plenty")
            .map(|_| ())
            .expect_err("`plenty` is not an integer");
        let message = format!("{error:#}");

        assert!(message.contains("batch_size"), "{message}");
        assert!(message.contains("integer"), "{message}");
    }

    #[test]
    fn a_query_parameter_still_overrides_the_position_it_would_have_had() {
        let config = UriSchema::from_schema(&connect_schema())
            .config_from_uri("rp://host/orders?url=rp://elsewhere:9092")
            .expect("map the uri");

        assert_eq!(config["url"], json!("rp://elsewhere:9092"));
        assert_eq!(config["topic"], json!("orders"));
    }

    #[test]
    fn a_schema_with_no_annotation_maps_like_no_schema_at_all() {
        let schema = json!({
            "type": "object",
            "properties": { "url": { "type": "string" }, "batch_size": { "type": "integer" } },
        });
        let config = UriSchema::from_schema(&schema)
            .config_from_uri("rp://host/orders?batch_size=7")
            .expect("map the uri");

        assert_eq!(config["url"], json!("rp://host/orders"));
        assert_eq!(config["batch_size"], json!(7));
    }

    #[test]
    fn a_missing_path_leaves_the_field_to_the_plugin_s_own_defaulting() {
        let config = UriSchema::from_schema(&connect_schema())
            .config_from_uri("rp://host:9092")
            .expect("map the uri");

        assert_eq!(config["url"], json!("rp://host:9092"));
        assert!(!config.contains_key("topic"), "{config:?}");
    }

    /// A gateway plugin's schema: the scheme names both the plugin and the
    /// protocol it should reach, the way `git+ssh` and `postgresql+psycopg2` do.
    fn gateway_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "connector": { "type": "string", "x-mqb-uri": "subscheme" },
                "address": { "type": "string", "x-mqb-uri": "origin" },
                "topic": { "type": "string", "x-mqb-uri": "path" },
            },
        })
    }

    #[test]
    fn a_compound_scheme_names_the_protocol_and_hands_the_address_over_with_it() {
        let config = UriSchema::from_schema(&gateway_schema())
            .config_from_uri("rp+mqtt://host:1883/orders?client_id=reader")
            .expect("map the uri");

        assert_eq!(config["connector"], json!("mqtt"));
        assert_eq!(config["address"], json!("mqtt://host:1883"));
        assert_eq!(config["topic"], json!("orders"));
        assert_eq!(config["client_id"], json!("reader"));
    }

    #[test]
    fn a_claimed_url_is_handed_over_carrying_the_inner_scheme_too() {
        let schema = json!({
            "type": "object",
            "properties": {
                "connector": { "type": "string", "x-mqb-uri": "subscheme" },
                "url": { "type": "string", "x-mqb-uri": "url" },
            },
        });

        let config = UriSchema::from_schema(&schema)
            .config_from_uri("rp+amqp://user@host:5672/jobs?tls=true")
            .expect("map the uri");

        assert_eq!(config["connector"], json!("amqp"));
        assert_eq!(config["url"], json!("amqp://user@host:5672/jobs"));
    }

    #[test]
    fn undeclared_query_values_stay_text_unless_the_schema_opts_in() {
        let uri = "rp://host/t?flag=true&count=100&ratio=0.5&id=007&version=1.0.0&name=x&big=99999999999999999999";
        let open = json!({ "type": "object", "properties": {} });
        let config = UriSchema::from_schema(&open)
            .config_from_uri(uri)
            .expect("map the uri");
        assert_eq!(config["flag"], json!("true"));
        assert_eq!(config["count"], json!("100"));

        let mut inferring = open;
        inferring[INFER_SCALARS_ANNOTATION] = json!(true);
        validate(&inferring).expect("a valid schema");
        let config = UriSchema::from_schema(&inferring)
            .config_from_uri(uri)
            .expect("map the uri");
        assert_eq!(config["flag"], json!(true));
        assert_eq!(config["count"], json!(100));
        assert_eq!(config["ratio"], json!(0.5));
        assert_eq!(config["id"], json!("007"));
        assert_eq!(config["version"], json!("1.0.0"));
        assert_eq!(config["name"], json!("x"));
        assert_eq!(config["big"], json!("99999999999999999999"));
    }

    #[test]
    fn a_non_boolean_infer_scalars_flag_is_refused() {
        let schema = json!({ "type": "object", INFER_SCALARS_ANNOTATION: "yes" });
        assert!(validate(&schema).is_err());
    }

    #[test]
    fn an_empty_authority_leaves_the_origin_field_unset() {
        let config = UriSchema::from_schema(&gateway_schema())
            .config_from_uri("connect+generate://?count=100")
            .expect("map the uri");

        assert_eq!(config["connector"], json!("generate"));
        assert!(!config.contains_key("address"), "{config:?}");
        assert!(!config.contains_key("topic"), "{config:?}");
    }

    /// Without the `+` there is no protocol to name, and the rest of the URI is
    /// read exactly as it was before compound schemes existed.
    #[test]
    fn a_plain_scheme_leaves_the_subscheme_field_unset() {
        let config = UriSchema::from_schema(&gateway_schema())
            .config_from_uri("rp://host:1883/orders")
            .expect("map the uri");

        assert!(!config.contains_key("connector"), "{config:?}");
        assert_eq!(config["address"], json!("rp://host:1883"));
    }

    /// The whole point of claiming the subscheme: a schema that positions only
    /// it must not also get the legacy whole-URI-as-`url` field, which no
    /// gateway plugin has anywhere to put.
    #[test]
    fn claiming_only_the_subscheme_still_suppresses_the_pre_schema_mapping() {
        let schema = json!({
            "type": "object",
            "properties": { "connector": { "type": "string", "x-mqb-uri": "subscheme" } },
        });

        let config = UriSchema::from_schema(&schema)
            .config_from_uri("rp+mqtt://host:1883?client_id=reader")
            .expect("map the uri");

        assert_eq!(config["connector"], json!("mqtt"));
        assert!(!config.contains_key("url"), "{config:?}");
    }

    #[test]
    fn a_usable_schema_validates() {
        validate(&connect_schema()).expect("the annotated schema is usable");
        validate(&gateway_schema()).expect("a compound-scheme schema is usable");
        validate(&json!({ "type": "object" })).expect("describing no property is usable");
    }

    #[test]
    fn two_fields_cannot_claim_the_same_part_of_a_uri() {
        let schema = json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "x-mqb-uri": "path" },
                "queue": { "type": "string", "x-mqb-uri": "path" },
            },
        });

        let error = validate(&schema)
            .map(|_| ())
            .expect_err("one path cannot fill two fields");

        assert!(format!("{error:#}").contains("path"), "{error:#}");
    }

    #[test]
    fn a_misspelled_position_is_refused_at_load_time() {
        let schema = json!({
            "type": "object",
            "properties": { "topic": { "type": "string", "x-mqb-uri": "pathname" } },
        });

        let error = validate(&schema)
            .map(|_| ())
            .expect_err("`pathname` is not a position");
        let message = format!("{error:#}");

        assert!(message.contains("topic"), "{message}");
        assert!(message.contains("pathname"), "{message}");
    }

    // ------------------------------------------------------- validation

    #[test]
    fn validation_is_off_unless_it_is_switched_on() {
        assert!(!validation_enabled_from(None));
        assert!(!validation_enabled_from(Some("0")));
        assert!(!validation_enabled_from(Some("")));
        assert!(validation_enabled_from(Some("1")));
        assert!(validation_enabled_from(Some(" TRUE ")));
    }

    fn strict_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "topic": { "type": "string" },
                "batch_size": { "type": "integer" },
                "mode": { "type": "string", "enum": ["earliest", "latest"] },
            },
            "required": ["topic"],
        })
    }

    #[test]
    fn configuration_matching_the_schema_passes() {
        validate_config(
            &strict_schema(),
            &json!({ "topic": "orders", "batch_size": 100, "mode": "earliest" }),
        )
        .expect("this is what the schema describes");
    }

    /// The mistake worth catching, and the one the type checks cannot see.
    #[test]
    fn a_misspelled_field_is_named_along_with_the_ones_that_exist() {
        let error = validate_config(&strict_schema(), &json!({ "topic": "o", "topci": "x" }))
            .expect_err("`topci` is not a field");
        let message = format!("{error}");

        assert!(message.contains("topci"), "{message}");
        assert!(message.contains("batch_size"), "{message}");
        assert!(matches!(error, Invalid(_)), "{message}");
    }

    #[test]
    fn a_wrong_type_a_missing_field_and_a_bad_enum_are_each_refused() {
        for config in [
            json!({ "topic": "o", "batch_size": "plenty" }),
            json!({ "batch_size": 1 }),
            json!({ "topic": "o", "mode": "sometime" }),
        ] {
            let error =
                validate_config(&strict_schema(), &config).expect_err("the schema rules this out");
            assert!(matches!(error, Invalid(_)), "{error} for {config}");
        }
    }

    /// A schema the host cannot check is the author's problem, so it has to be
    /// distinguishable — a caller enforcing someone else's schema passes it through.
    #[test]
    fn a_schema_beyond_the_checked_subset_is_unenforceable_not_invalid() {
        let schema = json!({
            "type": "object",
            "properties": { "auth": { "type": "cursed" } },
        });

        let error = validate_config(&schema, &json!({ "auth": "x" }))
            .expect_err("`cursed` is not a JSON Schema type");

        assert!(matches!(error, Unenforceable(_)), "{error}");
    }

    /// Without `additionalProperties: false` an unlisted field is simply one the
    /// schema says nothing about, and a partial schema stays useful.
    #[test]
    fn an_unlisted_field_passes_a_schema_that_does_not_close_itself() {
        let mut schema = strict_schema();
        schema
            .as_object_mut()
            .unwrap()
            .remove("additionalProperties");

        validate_config(&schema, &json!({ "topic": "o", "extra": "kept" }))
            .expect("an open schema accepts more than it lists");
    }

    #[test]
    fn a_schema_that_does_not_describe_an_object_is_refused() {
        assert!(validate(&json!([1, 2])).is_err());
        assert!(validate(&json!({ "type": "string" })).is_err());
        assert!(validate(&json!({ "type": "object", "properties": 7 })).is_err());
    }

    /// The schema `#[derive(JsonSchema)]` gives a struct with a documented enum field.
    fn derived_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": {
                    "description": "How an upsert is written.",
                    "$ref": "#/$defs/WriteMethod",
                    "default": "replace",
                },
                "compression": { "$ref": "#/$defs/Compression" },
            },
            "$defs": {
                "WriteMethod": {
                    "description": "The enum's own text.",
                    "oneOf": [
                        { "type": "string", "const": "replace", "description": "Replace." },
                        { "type": "string", "const": "update", "description": "Merge." },
                    ],
                },
                "Compression": { "type": "string", "enum": ["none", "gzip"] },
            },
        })
    }

    #[test]
    fn a_derived_enum_is_flattened_to_the_form_a_host_reads() {
        let flat = flatten(&derived_schema());

        assert_eq!(
            flat,
            json!({
                "type": "object",
                "properties": {
                    "method": {
                        "description": "How an upsert is written.",
                        "type": "string",
                        "enum": ["replace", "update"],
                        "default": "replace",
                    },
                    "compression": { "type": "string", "enum": ["none", "gzip"] },
                },
            })
        );
        validate_config(&flat, &json!({ "method": "update" })).expect("a listed value");
        assert!(validate_config(&flat, &json!({ "method": "merge" })).is_err());
    }

    #[test]
    fn a_flat_schema_is_left_as_it_is() {
        let schema = strict_schema();
        assert_eq!(flatten(&schema), schema);
    }

    #[test]
    fn a_cyclic_or_unknown_reference_is_kept_rather_than_followed_forever() {
        let schema = json!({
            "type": "object",
            "properties": {
                "node": { "$ref": "#/$defs/Node" },
                "other": { "$ref": "#/$defs/Missing" },
            },
            "$defs": { "Node": { "properties": { "next": { "$ref": "#/$defs/Node" } } } },
        });
        let flat = flatten(&schema);
        assert_eq!(flat["properties"]["other"]["$ref"], "#/$defs/Missing");
        assert_eq!(
            flat["properties"]["node"],
            json!({ "properties": { "next": { "$ref": "#/$defs/Node" } } })
        );
        // The reference left behind still resolves.
        assert!(flat.pointer("/$defs/Node/properties/next").is_some());
        assert!(flat.pointer("/$defs/Missing").is_none());
    }

    #[test]
    fn a_reference_into_a_definition_resolves_and_keeps_the_definition_a_cycle_needs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "left": { "$ref": "#/$defs/Pair/properties/left" },
                "loop": { "$ref": "#/$defs/Node/properties/next" },
            },
            "$defs": {
                "Pair": { "properties": { "left": { "type": "integer" } } },
                "Node": { "properties": { "next": { "$ref": "#/$defs/Node/properties/next" } } },
            },
        });
        let flat = flatten(&schema);
        assert_eq!(flat["properties"]["left"], json!({ "type": "integer" }));
        assert_eq!(
            flat["properties"]["loop"]["$ref"],
            "#/$defs/Node/properties/next"
        );
        assert!(flat.pointer("/$defs/Node/properties/next").is_some());
        assert!(flat.pointer("/$defs/Pair").is_none());
    }
}
