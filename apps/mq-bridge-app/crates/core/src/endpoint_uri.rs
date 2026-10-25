//! Endpoint URIs (`kafka://host?topic=t|retry`) to endpoint configs, the form
//! `mqb --from/--to` takes. [`crate::endpoint_catalog`] holds the scheme table.

use crate::copy_pipeline;
use crate::endpoint_catalog;
use anyhow::Context;

/// Maps an endpoint URI to an mq-bridge [`Endpoint`](mq_bridge::models::Endpoint): the scheme
/// selects the endpoint, and `?param=a&next=b` query parameters set its config.
///
/// Query keys that match a *scalar* field of the target endpoint's config struct
/// become endpoint config (e.g. `table`, `insert_query`, `subject`,
/// `delete_after_read`), and an object-typed field like `tls` takes a JSON
/// literal (`?tls={"required":true,...}`); any other query param stays on the
/// connection URL, so driver options such as `sslmode`, `replicaSet` or
/// `tls=true` pass through unchanged. `file` URIs map the path to the `path` field. For `nats`, the
/// dominant target field `subject` may also be given as the URL path
/// (`nats://host:4222/orders`) as an alternative to `?subject=orders` (the query
/// form wins if both are present); redis is excluded because a redis URL path is
/// the database number, not the stream.
///
/// MongoDB is pinned to a non-destructive source here: when neither `consume` nor
/// the deprecated `change_stream` is given, `consume` is set to `capture_all`
/// (read existing documents, then watch), which needs a replica set. Pass
/// `?consume=snapshot` for a one-shot read of a standalone mongod, or
/// `?consume=delete` for the destructive queue-drain mode.
///
/// Escaped mode: pass the full connection string percent-encoded as `?url=...`
/// to use it verbatim (e.g. `mongodb://_/?url=<encoded>&collection=orders`); its
/// own `?a=b` options are then never re-interpreted as config, which is the
/// escape hatch for any driver option that collides with a config field name.
/// Parses a `--from`/`--to` value into an endpoint, including any middlewares.
///
/// The value is the endpoint URI optionally followed by `|`-separated middleware
/// specs, applied in the order written:
/// `postgres://host/db?table=src|retry?max_attempts=5|metrics`.
/// A literal `|` inside the URI itself (e.g. in a password) must be written
/// percent-encoded as `%7C`.
///
/// Structural endpoints take their nested endpoints as query params that are
/// themselves endpoint URIs — `fanout:?mirror=<uri>&to=<uri>`,
/// `request:?to=<uri>&forward_to=<uri>`, `switch:?metadata_key=k&case.v=<uri>`
/// or `switch:?when=<expr>&to=<uri>`,
/// plus the argument-free `response:` and `null:`. A nested URI only needs
/// percent-encoding when it carries `&`, `#` or `|` of its own.
pub fn endpoint_from_uri(uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    let mut parts = uri.split('|');
    let base = parts.next().unwrap_or(uri);
    let mut endpoint = base_endpoint_from_uri(base)?;
    for spec in parts {
        endpoint
            .middlewares
            .push(middleware_from_spec(spec).with_context(|| {
                format!(
                    "invalid middleware '{}' in '{shown}'",
                    copy_pipeline::redact_uri(spec)
                )
            })?);
    }
    Ok(endpoint)
}

fn nested_endpoint(
    key: &str,
    value: &str,
    outer: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    endpoint_from_uri(value).with_context(|| {
        format!(
            "invalid '{key}' endpoint '{}' in '{}'",
            copy_pipeline::redact_uri(value),
            copy_pipeline::redact_uri(outer)
        )
    })
}

/// Wraps a branch so it can neither answer the caller nor fail the message for
/// the other branches: `request` discards the response, and forwards the
/// original rather than erroring when the branch is down.
fn discarding_request(branch: mq_bridge::models::Endpoint) -> mq_bridge::models::Endpoint {
    use mq_bridge::models::{Endpoint, EndpointType, RequestForwardConfig};

    Endpoint::new(EndpointType::Request(RequestForwardConfig {
        to: Box::new(branch),
        forward_to: Box::new(Endpoint::new(EndpointType::Null)),
    }))
}

/// An `http`/`websocket` **source** is a server, so its `url` is a listen
/// address — but a URI needs a scheme to select the endpoint at all, and the
/// driver rejects one as part of an address. `https` and `wss` ask for a TLS
/// listener; the certificate comes from `tls.cert_file` and `tls.key_file`.
pub fn make_listen_address(endpoint: &mut mq_bridge::models::Endpoint) -> anyhow::Result<()> {
    use mq_bridge::models::EndpointType;

    let (url, tls) = match &mut endpoint.endpoint_type {
        EndpointType::Http(config) => (&mut config.url, &mut config.tls),
        EndpointType::WebSocket(config) => (&mut config.url, &mut config.tls),
        _ => return Ok(()),
    };
    for (prefix, secure) in [
        ("https://", true),
        ("wss://", true),
        ("http://", false),
        ("ws://", false),
    ] {
        if let Some(rest) = url.strip_prefix(prefix) {
            *url = rest.trim_end_matches('/').to_string();
            if secure {
                tls.required = true;
            }
            break;
        }
    }
    Ok(())
}

/// Builds a middleware from a `name` / `name?param=value&...` spec.
///
/// Built-ins are read off the `Middleware` enum's own schema, so a new variant
/// needs no change here. Params are the variant's config fields, coerced to
/// their type; an endpoint-typed field (`dlq`'s `endpoint`, `lookup`'s `from`)
/// takes an endpoint URI, and an object/array field a JSON literal. A string
/// variant (`id`, `filter`) takes the whole percent-decoded query as its value.
/// Any other name is a registered or installed custom middleware.
fn middleware_from_spec(spec: &str) -> anyhow::Result<mq_bridge::models::Middleware> {
    let (name, query) = match spec.split_once('?') {
        Some((name, query)) => (name.trim(), query),
        None => (spec.trim(), ""),
    };
    // An underscore is awkward to type in a shell-quoted URI, so `-` is accepted
    // as well (`weak-join` == `weak_join`).
    let tag = name.replace('-', "_");
    let root = serde_json::to_value(schemars::schema_for!(mq_bridge::models::Middleware))?;

    if let Some(variant) = middleware_variant(&root, &tag) {
        let value =
            if accepts_bare_string(&root, variant) && !query_sets_fields(&root, variant, query) {
                let raw = percent_encoding::percent_decode_str(query)
                    .decode_utf8()
                    .with_context(|| {
                        format!(
                            "middleware spec '{}' is not valid UTF-8",
                            copy_pipeline::redact_uri(spec)
                        )
                    })?;
                serde_json::Value::String(raw.into_owned())
            } else {
                serde_json::Value::Object(middleware_params(spec, query, Some((&root, variant)))?)
            };
        return serde_json::from_value(serde_json::json!({ tag.as_str(): value })).with_context(
            || {
                format!(
                    "could not build a '{tag}' middleware from '{}'",
                    copy_pipeline::redact_uri(spec)
                )
            },
        );
    }

    let Some((name, declared)) = custom_middleware(name, &tag)? else {
        crate::plugins::discover_installed_plugins();
        let mut known = middleware_tags(&root);
        known.extend(mq_bridge::extensions::middleware_config_schemas().into_keys());
        anyhow::bail!(
            "unsupported middleware '{tag}'. Supported middlewares: {}. A name may also be a \
             middleware loaded with --plugin or installed on the plugin search path ({}). \
             Run `mqb list` to see each one",
            known.join(", "),
            mq_bridge::plugin::search_path_hint(name),
        );
    };
    let config = match declared {
        Some(schema) => middleware_params(spec, query, Some((&schema, &schema)))?,
        None => middleware_params(spec, query, None)?,
    };
    Ok(mq_bridge::models::Middleware::Custom {
        name,
        config: serde_json::Value::Object(config),
    })
}

/// Whether a variant can be written as a bare string: a string variant (`id`)
/// or a string-or-map one (`filter`).
pub(crate) fn accepts_bare_string(root: &serde_json::Value, variant: &serde_json::Value) -> bool {
    let variant = variant
        .get("$ref")
        .and_then(|r| r.as_str())
        .and_then(|r| resolve_ref(root, r))
        .unwrap_or(variant);
    if matches!(field_type(root, variant), FieldType::StringLike) {
        return true;
    }
    ["anyOf", "oneOf"].iter().any(|key| {
        variant
            .get(key)
            .and_then(|a| a.as_array())
            .is_some_and(|members| {
                members
                    .iter()
                    .any(|m| m.get("type").and_then(|t| t.as_str()) == Some("string"))
            })
    })
}

/// Whether every `&`-separated part of `query` sets a config field of `variant`,
/// which selects the map form of a string-or-map variant.
fn query_sets_fields(root: &serde_json::Value, variant: &serde_json::Value, query: &str) -> bool {
    let mut fields = std::collections::HashMap::new();
    collect_props(
        root,
        variant,
        &mut fields,
        &mut std::collections::HashSet::new(),
    );
    !query.is_empty()
        && query.split('&').all(|part| {
            part.split_once('=')
                .is_some_and(|(key, _)| fields.contains_key(key))
        })
}

/// The schema of the `Middleware` variant tagged `tag`, if the enum has one.
pub(crate) fn middleware_variant<'a>(
    root: &'a serde_json::Value,
    tag: &str,
) -> Option<&'a serde_json::Value> {
    root["oneOf"]
        .as_array()?
        .iter()
        .find_map(|variant| variant.get("properties")?.get(tag))
}

/// Every tag the `Middleware` enum accepts, for the unsupported-middleware error.
pub(crate) fn middleware_tags(root: &serde_json::Value) -> Vec<String> {
    root["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|variant| variant.get("properties")?.as_object())
        .flat_map(|properties| properties.keys().cloned())
        .collect()
}

/// A middleware registered under `name` or its `_` spelling, loading an
/// installed plugin when none is. Yields the name it answers to and its schema.
fn custom_middleware(
    name: &str,
    tag: &str,
) -> anyhow::Result<Option<(String, Option<serde_json::Value>)>> {
    let registered = |candidate: &str| {
        mq_bridge::extensions::get_middleware_factory(candidate).map(|factory| {
            let schema = factory.config_schema();
            let flat = schema
                .as_ref()
                .map(mq_bridge::support::config_schema::flatten);
            (candidate.to_string(), flat)
        })
    };
    if let Some(found) = registered(name).or_else(|| registered(tag)) {
        return Ok(Some(found));
    }
    for candidate in [name, tag] {
        if mq_bridge::plugin::discover_middleware_plugin(candidate)
            .with_context(|| format!("middleware '{candidate}'"))?
            .is_some()
        {
            return Ok(registered(candidate));
        }
    }
    // One library may provide several names, so its file need not be named for this one.
    crate::plugins::discover_installed_plugins();
    Ok(registered(name).or_else(|| registered(tag)))
}

/// Reads a middleware spec's query params into its config object, typed by
/// `schema` (`(root, node)`). Without one, nothing says a param is a number,
/// so each stays a string rather than turning an id like `0123` into one.
fn middleware_params(
    spec: &str,
    query: &str,
    schema: Option<(&serde_json::Value, &serde_json::Value)>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut fields = std::collections::HashMap::new();
    let mut endpoints = std::collections::HashSet::new();
    if let Some((root, node)) = schema {
        collect_props(
            root,
            node,
            &mut fields,
            &mut std::collections::HashSet::new(),
        );
        endpoints = endpoint_fields(root, node);
    }

    let mut config = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let (k, v) = (k.into_owned(), v.into_owned());
        let value = if endpoints.contains(&k) {
            let endpoint = endpoint_from_uri(&v).with_context(|| {
                format!(
                    "invalid '{k}' endpoint '{}' in '{}'",
                    copy_pipeline::redact_uri(&v),
                    copy_pipeline::redact_uri(spec)
                )
            })?;
            serde_json::to_value(endpoint)?
        } else {
            match fields.get(&k).copied() {
                // A known object/array field must be a JSON literal; a value that
                // doesn't parse is a user error worth naming, the same way
                // `base_endpoint_from_uri` handles its object fields, rather than
                // a silent fallback to a string that serde rejects later.
                Some(FieldType::Object) => serde_json::from_str(&v).with_context(|| {
                    format!(
                        "query param '{k}' in middleware spec '{}' expects a JSON literal, got '{}'",
                        copy_pipeline::redact_uri(spec),
                        copy_pipeline::redact_param(&k, &v, None)
                    )
                })?,
                None if schema.is_none() => serde_json::Value::String(v),
                // An unknown field (`None`) is passed through for serde to reject
                // by name.
                None => serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v)),
                Some(ty) => coerce_scalar(v, ty),
            }
        };
        config.insert(k, value);
    }
    Ok(config)
}

/// The fields of `node` that hold an `Endpoint` (possibly optional or boxed),
/// which a spec writes as an endpoint URI.
fn endpoint_fields(
    root: &serde_json::Value,
    node: &serde_json::Value,
) -> std::collections::HashSet<String> {
    let is_endpoint = |schema: &serde_json::Value| {
        let names = |s: &serde_json::Value| {
            s.get("$ref").and_then(serde_json::Value::as_str) == Some("#/$defs/Endpoint")
        };
        names(schema)
            || ["anyOf", "oneOf"].iter().any(|key| {
                schema
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|members| members.iter().any(names))
            })
    };
    let node = node
        .get("$ref")
        .and_then(serde_json::Value::as_str)
        .and_then(|reference| resolve_ref(root, reference))
        .unwrap_or(node);
    node.get("properties")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, schema)| is_endpoint(schema))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The extension endpoints this build compiled in, for the unsupported-scheme
/// message, so it never names one this binary does not have.
fn extension_schemes() -> String {
    let mut names = Vec::new();
    if cfg!(feature = "pulsar") {
        names.push("pulsar".to_string());
    }
    #[cfg(feature = "http-bulk")]
    for name in mq_bridge::endpoints::http_bulk::preset_names() {
        // As a scheme, which cannot hold `_`.
        let name = name.replace('_', "-");
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return String::new();
    }
    format!(" ({})", names.join(", "))
}

/// Builds the endpoint for a scheme that names a registered factory: a named one such as
/// `qdrant` as its own type, any other as `custom`.
///
/// The factory owns its config shape, so the mapping comes from the JSON Schema
/// it declares: which field takes the address, which takes the path, and what
/// type each query param is read as. A factory that declares nothing keeps the
/// mapping that predates schemas — `url` is the URI up to the query and every
/// query param is a string, since guessing a type there would silently turn an
/// id like `0123` into a number.
fn custom_endpoint_from_uri(name: &str, uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    let config = mq_bridge::plugin::endpoint_uri_schema(name)
        .config_from_uri(uri)
        .with_context(|| format!("endpoint '{name}' in URI '{shown}'"))?;

    // The endpoint's own key; one that is no built-in type parses as `custom`.
    serde_json::from_value(serde_json::json!({ name: config }))
        .with_context(|| format!("endpoint '{name}' in URI '{shown}'"))
}

/// The plugin a `plugin+component` scheme names. Built-in schemes are matched
/// before this is consulted, so none of them can be shadowed by it.
/// The factory a scheme names when none is registered under the scheme itself.
fn scheme_factory(scheme: &str) -> String {
    match plugin_of(scheme) {
        "openai" => "openai_chat".to_string(),
        plugin => plugin.replace('-', "_"),
    }
}

fn plugin_of(scheme: &str) -> &str {
    scheme.split_once('+').map_or(scheme, |(plugin, _)| plugin)
}

pub(crate) fn http_bulk_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use serde_json::Value;

    let mut text = None;
    let mut url = None;
    for (key, value) in parsed.query_pairs() {
        let slot = match key.as_ref() {
            "config" | "config_file" => &mut text,
            "url" => &mut url,
            other => anyhow::bail!(
                "unsupported query param '{other}' in http-bulk URI '{shown}'. Supported: config_file, config, url"
            ),
        };
        if slot.is_some() {
            anyhow::bail!("http-bulk URI '{shown}' gives '{key}' or its alternative twice");
        }
        *slot = Some(match key.as_ref() {
            "config_file" => std::fs::read_to_string(value.as_ref())
                .with_context(|| format!("failed to read http-bulk config_file '{value}'"))?,
            _ => value.into_owned(),
        });
    }
    let Some(text) = text else {
        anyhow::bail!(
            "http-bulk URI '{shown}' needs 'config_file=<path>' or 'config=<YAML or JSON>' holding the http_bulk fields"
        );
    };
    let mut config: Value = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("http-bulk config in URI '{shown}' is not valid YAML or JSON"))?;
    // A recipe copied with its `http_bulk:` key is taken as well.
    if let Some(inner) = config.as_object_mut().filter(|map| map.len() == 1)
        && let Some(inner) = inner
            .remove("http_bulk")
            .or_else(|| inner.remove("http-bulk"))
    {
        config = inner;
    }
    if let (Some(url), Some(fields)) = (url, config.as_object_mut()) {
        fields.insert("url".into(), Value::String(url));
    }
    let endpoint_type = serde_json::from_value(serde_json::json!({ "http_bulk": config }))
        .with_context(|| format!("could not build an 'http_bulk' endpoint from URI '{shown}'"))?;
    Ok(mq_bridge::models::Endpoint::new(endpoint_type))
}

/// `null:`, a sink that discards everything (any trailing content ignored).
pub(crate) fn null_endpoint(_: &url::Url, _: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    use mq_bridge::models::{Endpoint, EndpointType};
    Ok(Endpoint::new(EndpointType::Null))
}

/// A source that endlessly produces a fixed message (config-only load
/// generator) or a sink. Body from `?body=`, or read a file with
/// `?body_file=`. `raw=true` sends the body verbatim (no JSON re-encode) —
/// use it so a generated JSON row is the payload as-is. Any other query
/// param becomes message metadata.
pub(crate) fn static_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    use std::collections::HashMap;
    let mut body: Option<String> = None;
    let mut raw = false;
    let mut metadata: HashMap<String, String> = HashMap::new();
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "body" => body = Some(v.into_owned()),
            "body_file" => {
                body = Some(
                    std::fs::read_to_string(v.as_ref())
                        .with_context(|| format!("failed to read static body_file '{}'", v))?,
                );
            }
            "raw" => raw = v == "true",
            _ => {
                metadata.insert(k.into_owned(), v.into_owned());
            }
        }
    }
    let mut cfg = serde_json::Map::new();
    cfg.insert(
        "body".into(),
        serde_json::Value::String(body.unwrap_or_default()),
    );
    cfg.insert("raw".into(), serde_json::Value::Bool(raw));
    cfg.insert("metadata".into(), serde_json::to_value(metadata)?);
    let mut tagged = serde_json::Map::new();
    tagged.insert("static".into(), serde_json::Value::Object(cfg));
    let endpoint_type: EndpointType = serde_json::from_value(serde_json::Value::Object(tagged))
        .with_context(|| format!("could not build a 'static' endpoint from URI '{shown}'"))?;
    Ok(Endpoint::new(endpoint_type))
}

/// `response:` — replies to the caller; needs an input with a reply channel.
pub(crate) fn response_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    if let Some((key, _)) = parsed.query_pairs().next() {
        anyhow::bail!(
            "unsupported query param '{key}' in response URI '{shown}'. Response takes no query parameters"
        );
    }
    Ok(Endpoint::new(EndpointType::Response(Default::default())))
}

/// `fanout:?mirror=<uri>&to=<uri>`, in the order written. Only a `to`
/// branch can answer the caller.
pub(crate) fn fanout_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    let mut branches = Vec::new();
    for (key, value) in parsed.query_pairs() {
        let mirrored = match key.as_ref() {
            "to" => false,
            "mirror" => true,
            other => anyhow::bail!(
                "unsupported query param '{other}' in fanout URI '{shown}'. Use 'to=<uri>' for a branch that may answer, 'mirror=<uri>' for one whose response and failures are discarded"
            ),
        };
        let branch = nested_endpoint(&key, &value, uri)?;
        branches.push(if mirrored {
            discarding_request(branch)
        } else {
            branch
        });
    }
    if branches.is_empty() {
        anyhow::bail!(
            "fanout URI '{shown}' has no branches. Add at least one 'to=<uri>' or 'mirror=<uri>'"
        );
    }
    Ok(Endpoint::new(EndpointType::Fanout(branches)))
}

/// `request:?to=<uri>&forward_to=<uri>`; without a `forward_to` the
/// response is discarded.
pub(crate) fn request_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    let mut to = None;
    let mut forward_to = None;
    for (key, value) in parsed.query_pairs() {
        let slot = match key.as_ref() {
            "to" => &mut to,
            "forward_to" => &mut forward_to,
            other => anyhow::bail!(
                "unsupported query param '{other}' in request URI '{shown}'. Supported: to, forward_to"
            ),
        };
        if slot.is_some() {
            anyhow::bail!("duplicate query param '{key}' in request URI '{shown}'");
        }
        *slot = Some(Box::new(nested_endpoint(&key, &value, uri)?));
    }
    let Some(to) = to else {
        anyhow::bail!("request URI '{shown}' needs a 'to=<uri>' endpoint to send to");
    };
    Ok(Endpoint::new(EndpointType::Request(
        mq_bridge::models::RequestForwardConfig {
            to,
            forward_to: forward_to.unwrap_or_else(|| Box::new(Endpoint::new(EndpointType::Null))),
        },
    )))
}

/// Value lookup: `switch:?metadata_key=<key>&case.<value>=<uri>&default=<uri>`.
/// Predicates:   `switch:?when=<expression>&to=<uri>&…&default=<uri>`, first
/// match wins, so `when`/`to` pairs keep the order they were written in. An
/// expression goes in the *value*, where an `=` needs no escaping — but a
/// literal `&` still splits the query, so write `and`/`or` rather than
/// `&&`/`||` (the engine accepts both spellings anyway).
pub(crate) fn switch_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    let mut metadata_key = None;
    let mut cases = std::collections::BTreeMap::new();
    let mut when: Vec<mq_bridge::models::SwitchCase> = Vec::new();
    let mut condition: Option<String> = None;
    let mut default = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "metadata_key" => metadata_key = Some(value.into_owned()),
            "when" => {
                if let Some(previous) = condition.replace(value.into_owned()) {
                    anyhow::bail!(
                        "switch URI '{shown}' has 'when={previous}' with no 'to=<uri>' after it"
                    );
                }
            }
            "to" => {
                let Some(condition) = condition.take() else {
                    anyhow::bail!(
                        "switch URI '{shown}' has a 'to={}' that no 'when=<expression>' precedes",
                        copy_pipeline::redact_uri(&value)
                    );
                };
                when.push(mq_bridge::models::SwitchCase {
                    condition,
                    to: nested_endpoint(&key, &value, uri)?,
                });
            }
            "default" => default = Some(nested_endpoint(&key, &value, uri)?),
            case if case.starts_with("case.") => {
                cases.insert(
                    case["case.".len()..].to_string(),
                    nested_endpoint(&key, &value, uri)?,
                );
            }
            other => anyhow::bail!(
                "unsupported query param '{other}' in switch URI '{shown}'. Supported: metadata_key, case.<value>=<uri>, when=<expression> with to=<uri>, default=<uri>"
            ),
        }
    }
    if let Some(dangling) = condition {
        anyhow::bail!("switch URI '{shown}' has 'when={dangling}' with no 'to=<uri>' after it");
    }
    // The two modes differ in cost, not just spelling, so mixing them
    // would hide which one a message actually took.
    if !when.is_empty() && (metadata_key.is_some() || !cases.is_empty()) {
        anyhow::bail!(
            "switch URI '{shown}' mixes both modes. Use either 'metadata_key=<key>' with 'case.<value>=<uri>', or 'when=<expression>' with 'to=<uri>'"
        );
    }
    if when.is_empty() {
        if metadata_key.is_none() {
            anyhow::bail!(
                "switch URI '{shown}' needs a 'metadata_key=<key>' to branch on, or 'when=<expression>&to=<uri>' predicates"
            );
        }
        if cases.is_empty() {
            anyhow::bail!("switch URI '{shown}' has no cases. Add 'case.<value>=<uri>'");
        }
    }
    Ok(Endpoint::new(EndpointType::Switch(
        mq_bridge::models::SwitchConfig {
            metadata_key: metadata_key.unwrap_or_default(),
            cases: cases.into_iter().collect(),
            when,
            default: default.map(Box::new),
        },
    )))
}

/// In-process channel. Topic is the host (+path): `memory://my-topic`.
/// `?capacity=`, `?subscribe_mode=` are recognised; other params ignored.
pub(crate) fn memory_endpoint(
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};
    let host = parsed.host_str().unwrap_or("");
    let path = parsed.path().trim_matches('/');
    let topic = if path.is_empty() {
        host.to_string()
    } else if host.is_empty() {
        path.to_string()
    } else {
        format!("{host}/{path}")
    };
    if topic.is_empty() {
        anyhow::bail!("memory URI '{shown}' must include a topic, e.g. memory://my-topic");
    }
    let mut cfg = serde_json::Map::new();
    cfg.insert("topic".into(), serde_json::Value::String(topic));
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "capacity" => {
                if let Ok(n) = v.parse::<u64>() {
                    cfg.insert("capacity".into(), serde_json::Value::from(n));
                }
            }
            "subscribe_mode" => {
                cfg.insert(
                    "subscribe_mode".into(),
                    serde_json::Value::Bool(v == "true"),
                );
            }
            // An in-process channel has no connection URL to carry driver
            // options, so anything else would just be discarded.
            other => anyhow::bail!(
                "unrecognised query param '{other}' in memory URI '{shown}': only 'capacity' and 'subscribe_mode' are supported"
            ),
        }
    }
    let mut tagged = serde_json::Map::new();
    tagged.insert("memory".into(), serde_json::Value::Object(cfg));
    let endpoint_type: EndpointType = serde_json::from_value(serde_json::Value::Object(tagged))
        .with_context(|| format!("could not build a 'memory' endpoint from URI '{shown}'"))?;
    Ok(Endpoint::new(endpoint_type))
}

/// The endpoint for a scheme no built-in claims: one registered by an extension,
/// loaded with `--plugin`, or installed on the plugin search path.
fn extension_endpoint_from_uri(
    scheme: &str,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    // In `plugin+component://` the plugin is the part before the `+`; the full
    // URI still goes to the factory, whose schema reads the component.
    if mq_bridge::extensions::get_endpoint_factory(plugin_of(scheme)).is_some() {
        return custom_endpoint_from_uri(plugin_of(scheme), uri);
    }
    // A scheme cannot hold `_`, so `openai-chat://` names `openai_chat`; `openai://` is its
    // short form.
    if mq_bridge::extensions::get_endpoint_factory(&scheme_factory(scheme)).is_some() {
        return custom_endpoint_from_uri(&scheme_factory(scheme), uri);
    }
    // A URI names the endpoint before any route is built, so an
    // installed plugin is searched for here as well as in the engine.
    let plugin = plugin_of(scheme);
    if mq_bridge::plugin::discover_endpoint_plugin(plugin)
        .with_context(|| format!("endpoint scheme '{scheme}' in URI '{shown}'"))?
        .is_some()
    {
        return custom_endpoint_from_uri(plugin, uri);
    }
    // One library may provide several names, so its file need not be named for this one.
    crate::plugins::discover_installed_plugins();
    for name in [plugin_of(scheme).to_string(), scheme_factory(scheme)] {
        if mq_bridge::extensions::get_endpoint_factory(&name).is_some() {
            return custom_endpoint_from_uri(&name, uri);
        }
    }
    let (schemes, structural) = endpoint_catalog::scheme_names();
    anyhow::bail!(
        "unsupported endpoint scheme '{scheme}' in URI '{shown}'. Supported schemes: {}, and the structural {}. A scheme may also name an endpoint registered by an extension{}, loaded with --plugin, or installed on the plugin search path ({}). Run `mqb list` to see each one",
        schemes.join(", "),
        structural.join(", "),
        extension_schemes(),
        mq_bridge::plugin::search_path_hint(plugin),
    )
}

fn base_endpoint_from_uri(uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use anyhow::bail;
    use mq_bridge::models::{Endpoint, EndpointType};
    use url::Url;

    let parsed = Url::parse(uri).with_context(|| format!("not a valid URI: {shown}"))?;

    let Some(entry) = endpoint_catalog::find_scheme(parsed.scheme()) else {
        return extension_endpoint_from_uri(parsed.scheme(), uri);
    };
    let (schema, form) = match entry.target {
        endpoint_catalog::Target::Parser(parse, _) => return parse(&parsed, uri),
        endpoint_catalog::Target::Path(schema) => (schema, None),
        endpoint_catalog::Target::Url(schema, form) => (schema, Some(form)),
    };
    let tag = entry.endpoint;
    let fields = schema_fields(schema());

    // Split query params: recognised scalar config fields become endpoint config,
    // everything else is kept on the connection URL (driver params) — but only
    // where such a param can actually take effect, see `driver_options`.
    let mut config = serde_json::Map::new();
    let mut driver_params: Vec<(String, String)> = Vec::new();
    // Escaped mode: `?url=<percent-encoded connection string>` supplies the exact
    // connection URL verbatim, so its own `?a=b` options are never re-interpreted
    // as config fields. Use it when a driver option would otherwise collide.
    let mut escaped_url: Option<String> = None;
    let driver_options = form.is_some_and(|form| form.driver_options);
    // Target is a filesystem path carried by the URI itself, so `?path=`/`?url=`
    // would be a second source for the same field.
    let path_target = form.is_none();
    for (k, v) in parsed.query_pairs() {
        let (k, v) = (k.into_owned(), v.into_owned());
        if k == "path" && path_target {
            continue;
        }
        if k == "url" && !path_target {
            escaped_url = Some(v);
            continue;
        }
        match fields.get(&k).copied() {
            // An object/array field (e.g. `encryption`, `tls`, kafka's
            // `producer_options`) can't be populated from a scalar, so its value is
            // read as a JSON literal, as the middleware spec syntax already does.
            // Where driver options exist the same name is also a plausible option
            // (`?tls=true`), so only an actual `{`/`[` literal is taken as config.
            Some(FieldType::Object) if !driver_options || is_json_literal(&v) => {
                let value: serde_json::Value = serde_json::from_str(&v).with_context(|| {
                    format!(
                        "query param '{k}' in URI '{shown}' expects a JSON literal, got '{}'",
                        copy_pipeline::redact_param(&k, &v, None)
                    )
                })?;
                config.insert(k, value);
            }
            Some(FieldType::Object) | None if driver_options => driver_params.push((k, v)),
            None => bail!(
                "unrecognised query param '{k}' in URI '{shown}': a '{tag}' endpoint has no connection-URL driver options, so '{k}' would have no effect. Run `mqb describe {}` for its params",
                parsed.scheme()
            ),
            Some(ty) => {
                config.insert(k, coerce_scalar(v, ty));
            }
        }
    }

    // Non-destructive default for MongoDB sources: `capture_all` (read existing
    // docs, then watch) so pointing at an existing collection never mutates it.
    // This matches the library default since 0.4.0 and is pinned here so a future
    // library change cannot make a CLI/UI source destructive. Only applied when the
    // user gave neither `consume` nor the deprecated `change_stream`, so any
    // explicit choice still wins.
    if tag == "mongodb" && !config.contains_key("consume") && !config.contains_key("change_stream")
    {
        config.insert(
            "consume".into(),
            serde_json::Value::String("capture_all".into()),
        );
    }

    if path_target {
        let path = uri.split('?').next().unwrap_or(uri);
        // `scheme://x` and the authority-less `scheme:/x` both name a path, so the
        // slashes come off separately from the scheme.
        let prefix = format!("{}:", parsed.scheme());
        let path = path.strip_prefix(prefix.as_str()).unwrap_or(path);
        let path = path.strip_prefix("//").unwrap_or(path);
        if path.is_empty() {
            bail!("{tag} URI '{shown}' must include a path");
        }
        // `x.jsonl.gz` says how it is compressed; `compression=none` overrides.
        if tag == "file" && !config.contains_key("compression") {
            let codec = [(".gz", "gzip"), (".zst", "zstd"), (".lz4", "lz4")]
                .into_iter()
                .find(|(extension, _)| path.ends_with(extension));
            if let Some((_, codec)) = codec {
                config.insert("compression".into(), codec.into());
            }
        }
        config.insert("path".into(), serde_json::Value::String(path.to_string()));
    } else if let Some(url) = escaped_url {
        // Escaped mode: the connection string is authoritative and complete, so
        // any leftover non-config param is ambiguous — it belongs inside `url=`.
        if let Some((k, _)) = driver_params.first() {
            bail!(
                "in escaped mode (url=...), put driver options inside the encoded connection string; unexpected query param '{k}' in URI '{shown}'"
            );
        }
        config.insert("url".into(), serde_json::Value::String(url));
    } else {
        let path_field = form.and_then(|form| form.path_field);
        let mut base = parsed.clone();
        if let Some(field) = path_field {
            let path = base.path().trim_matches('/');
            if !path.is_empty() && !config.contains_key(field) {
                config.insert(field.into(), serde_json::Value::String(path.to_string()));
            }
            base.set_path("");
        }
        // Connection URL = base URI plus any leftover (driver) params.
        base.set_fragment(None);
        base.set_query(None);
        if !driver_params.is_empty() {
            let mut qs = base.query_pairs_mut();
            for (k, v) in &driver_params {
                qs.append_pair(k, v);
            }
        }
        let mut url = base.to_string();
        let rewrites = form.map_or(&[][..], |form| form.rewrites);
        for (prefix, replacement) in rewrites {
            if let Some(rest) = url.strip_prefix(prefix) {
                url = format!("{replacement}{rest}");
                break;
            }
        }
        let url = match form.and_then(|form| form.fixup) {
            Some(fixup) => fixup(uri, url),
            None => Some(url),
        };
        if let Some(url) = url {
            config.insert("url".into(), serde_json::Value::String(url));
        }
    }

    let mut tagged = serde_json::Map::new();
    tagged.insert(tag.to_string(), serde_json::Value::Object(config));
    let endpoint_type: EndpointType = serde_json::from_value(serde_json::Value::Object(tagged))
        .with_context(|| format!("could not build a '{tag}' endpoint from URI '{shown}'"))?;
    Ok(Endpoint::new(endpoint_type))
}

/// rdkafka's bootstrap.servers is a bare `host:port` list, without a trailing `/`.
pub(crate) fn kafka_url(_: &str, url: String) -> Option<String> {
    Some(url.trim_end_matches('/').to_string())
}

/// `zeromq://tcp://host:port` names the transport itself; it is taken as written.
pub(crate) fn zeromq_url(uri: &str, url: String) -> Option<String> {
    let address = uri.split(['?', '#']).next().unwrap_or(uri);
    let transport = ["zeromq://", "zmq://"]
        .iter()
        .find_map(|prefix| address.strip_prefix(prefix))
        .filter(|t| t.contains("://"));
    Some(transport.map_or(url, str::to_string))
}

/// IBM MQ's driver expects `host(port)` (with comma-separated hosts for
/// failover), not a URI authority.
pub(crate) fn ibmmq_url(_: &str, url: String) -> Option<String> {
    let Some(rest) = url.strip_prefix("ibmmq://") else {
        return Some(url);
    };
    let rest = rest.trim_end_matches('/');
    Some(match rest.rsplit_once(':') {
        Some((host, port)) => format!("{host}({port})"),
        None => rest.to_string(),
    })
}

/// AwsConfig has no `url` field (`queue_url`/`topic_arn` carry the connection
/// info), so the placeholder authority is dropped.
pub(crate) fn no_url(_: &str, _: String) -> Option<String> {
    None
}

/// The JSON scalar type a config field expects, used to coerce string query
/// params into the right type without guessing from the value's shape.
#[derive(Clone, Copy)]
pub(crate) enum FieldType {
    Bool,
    Integer,
    Number,
    /// An object or array field (e.g. a nested config struct): it cannot be set
    /// from a scalar query param, so such params are routed to driver options.
    Object,
    /// Strings, enums, and anything else scalar — kept as a JSON string.
    StringLike,
}

/// Maps a config struct's serde field names to their expected scalar type, so
/// query params can be routed (recognised field vs driver param) and coerced
/// correctly. Walks the JSON schema following `$ref`, `allOf`, `anyOf` and
/// `oneOf`, so fields introduced via `#[serde(flatten)]` (e.g. an internally
/// tagged enum) are recognised too, not just top-level `properties`.
fn schema_fields(schema: schemars::Schema) -> std::collections::HashMap<String, FieldType> {
    let mut out = std::collections::HashMap::new();
    if let Ok(root) = serde_json::to_value(&schema) {
        let mut visited = std::collections::HashSet::new();
        collect_props(&root, &root, &mut out, &mut visited);
    }
    out
}

/// Recursively collects `(field name, type)` pairs from `node` into `out`,
/// resolving local `$ref`s against `root` and descending schema combinators.
fn collect_props(
    root: &serde_json::Value,
    node: &serde_json::Value,
    out: &mut std::collections::HashMap<String, FieldType>,
    visited: &mut std::collections::HashSet<String>,
) {
    let Some(obj) = node.as_object() else { return };

    if let Some(reference) = obj.get("$ref").and_then(|r| r.as_str()) {
        if visited.insert(reference.to_string())
            && let Some(target) = resolve_ref(root, reference)
        {
            collect_props(root, target, out, visited);
        }
        return;
    }

    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for (name, sub) in props {
            out.entry(name.clone())
                .or_insert_with(|| field_type(root, sub));
        }
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(|a| a.as_array()) {
            for sub in arr {
                collect_props(root, sub, out, visited);
            }
        }
    }
}

/// Resolves a local JSON-schema `$ref` (`#/$defs/Name` or `#/definitions/Name`)
/// to its target subschema within `root`.
pub(crate) fn resolve_ref<'a>(
    root: &'a serde_json::Value,
    reference: &str,
) -> Option<&'a serde_json::Value> {
    let name = reference.rsplit('/').next()?;
    ["$defs", "definitions"]
        .iter()
        .find_map(|defs| root.get(defs).and_then(|d| d.get(name)))
}

/// Determines the scalar [`FieldType`] of a property subschema. Handles a direct
/// `type`, an `Option<T>` (`{"type":["integer","null"]}` or an `anyOf`/`oneOf`
/// with a null member), and a `$ref` to a scalar def; anything else is treated
/// as string-like (enums deserialize from a string, so no coercion is needed).
pub(crate) fn field_type(root: &serde_json::Value, sub: &serde_json::Value) -> FieldType {
    if let Some(reference) = sub.get("$ref").and_then(|r| r.as_str())
        && let Some(target) = resolve_ref(root, reference)
    {
        return field_type(root, target);
    }
    // A `serde_json::Value` field (e.g. `transform`'s `schema`) constrains
    // nothing, so schemars renders it as the always-true schema. It takes whole
    // JSON rather than a scalar, which is exactly the `Object` handling.
    if is_unconstrained_schema(sub) {
        return FieldType::Object;
    }
    let has = |t: &str| match sub.get("type") {
        Some(serde_json::Value::String(s)) => s == t,
        Some(serde_json::Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(t)),
        _ => false,
    };
    if has("boolean") {
        return FieldType::Bool;
    }
    if has("integer") {
        return FieldType::Integer;
    }
    if has("number") {
        return FieldType::Number;
    }
    // A string-or-list field (kafka's `topic`) also takes the list comma-separated.
    if has("array") && has("string") && !has("object") {
        return FieldType::StringLike;
    }
    if has("object") || has("array") {
        return FieldType::Object;
    }
    // Option<scalar> is often modelled as anyOf/oneOf of the scalar and null.
    for key in ["anyOf", "oneOf"] {
        if let Some(arr) = sub.get(key).and_then(|a| a.as_array()) {
            for member in arr {
                match field_type(root, member) {
                    FieldType::StringLike => {}
                    ty => return ty,
                }
            }
        }
    }
    FieldType::StringLike
}

/// Whether a subschema accepts any JSON at all, i.e. states no `type`, `$ref`,
/// combinator or enumeration. Only annotations such as `description`/`default`
/// may remain.
fn is_unconstrained_schema(sub: &serde_json::Value) -> bool {
    match sub {
        serde_json::Value::Bool(accepts_anything) => *accepts_anything,
        serde_json::Value::Object(obj) => !obj.keys().any(|key| {
            matches!(
                key.as_str(),
                "type"
                    | "$ref"
                    | "allOf"
                    | "anyOf"
                    | "oneOf"
                    | "enum"
                    | "const"
                    | "properties"
                    | "items"
            )
        }),
        _ => false,
    }
}

/// Whether a query-param value is written as a JSON object or array literal.
/// Used to tell a nested-config value (`?tls={"ca_file":"/x"}`) from a driver
/// option that happens to share the field's name (`?tls=true`) on endpoints
/// whose connection URL carries both.
fn is_json_literal(v: &str) -> bool {
    let v = v.trim_start();
    v.starts_with('{') || v.starts_with('[')
}

/// Coerces a query-param string into the JSON scalar its target field expects.
/// Only bool/number *fields* trigger bool/number coercion, so a string field
/// keeps values like `2024` or `true` verbatim; a value that fails to parse for
/// a numeric field falls back to a string so serde reports a clear type error.
fn coerce_scalar(s: String, ty: FieldType) -> serde_json::Value {
    match ty {
        FieldType::Bool => match s.as_str() {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(s),
        },
        FieldType::Integer => match s.parse::<i64>() {
            Ok(i) => serde_json::Value::from(i),
            Err(_) => serde_json::Value::String(s),
        },
        FieldType::Number => match s.parse::<f64>() {
            Ok(f) => serde_json::Value::from(f),
            Err(_) => serde_json::Value::String(s),
        },
        // Object/array fields are routed to driver params before reaching here;
        // keep the raw string as a defensive fallback.
        FieldType::StringLike | FieldType::Object => serde_json::Value::String(s),
    }
}

#[cfg(test)]
mod tests {
    use super::{endpoint_from_uri, make_listen_address};
    use mq_bridge::models::{EndpointType, MongoConsume};

    fn config(uri: &str, tag: &str) -> serde_json::Value {
        let ep = endpoint_from_uri(uri).expect("uri should parse");
        serde_json::to_value(&ep).unwrap()[tag].clone()
    }

    // A driver option whose name matches an object-typed config field (`tls` is a
    // TlsConfig struct) must stay on the connection URL, not be hijacked as config.
    #[test]
    fn http_bulk_uri_takes_its_config_inline_or_from_a_file() {
        use mq_bridge::models::EndpointType;

        let file = std::env::temp_dir().join(format!("mqb-http-bulk-{}.yaml", std::process::id()));
        std::fs::write(
            &file,
            "http_bulk:\n  url: http://from-file\n  upsert:\n    path: /docs\n    result: &r\n      job: {id: /taskUid, poll: '/tasks/{id}', status: /status, succeeded: [ok], failed: [bad]}\n  delete:\n    path: /delete\n    result: *r\n",
        )
        .unwrap();
        let from_file = endpoint_from_uri(&format!(
            "http-bulk:?config_file={}&url=http://other:7700",
            file.display()
        ));
        std::fs::remove_file(&file).unwrap();
        let EndpointType::HttpBulk(config) = from_file.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(config.url, "http://other:7700");
        assert!(config.delete.unwrap().result.job.is_some());

        let inline =
            endpoint_from_uri(r#"http-bulk:?config={"url":"http://h","upsert":{"path":"/d"}}"#);
        let EndpointType::HttpBulk(config) = inline.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(
            (config.url.as_str(), config.upsert.unwrap().path.as_str()),
            ("http://h", "/d")
        );

        // The same scheme names a source: the config then carries `read`.
        let source = endpoint_from_uri(
            r#"http-bulk:?config={"url":"http://h","read":{"path":"/d?o={cursor}"}}"#,
        );
        let EndpointType::HttpBulk(config) = source.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(config.read.unwrap().path, "/d?o={cursor}");
    }

    #[test]
    fn mongodb_tls_option_stays_on_url() {
        let cfg = config("mongodb://host:27017/?tls=true&database=appdb", "mongodb");
        assert_eq!(cfg["url"], "mongodb://host:27017/?tls=true");
        assert_eq!(cfg["database"], "appdb");
    }

    // A recognised scalar field becomes config; an unrecognised param is a driver
    // option and passes through on the URL unchanged.
    #[test]
    fn mongodb_scalar_field_vs_driver_param() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&replicaSet=rs0",
            "mongodb",
        );
        assert_eq!(cfg["collection"], "orders");
        assert_eq!(cfg["url"], "mongodb://host/?replicaSet=rs0");
    }

    #[test]
    fn mongodb_srv_scheme_is_mongodb() {
        let cfg = config(
            "mongodb+srv://app:pw@cluster0.example.net/?database=shop&retryWrites=true",
            "mongodb",
        );
        assert_eq!(
            cfg["url"],
            "mongodb+srv://app:pw@cluster0.example.net/?retryWrites=true"
        );
        assert_eq!(cfg["database"], "shop");
    }

    #[test]
    fn redis_socket_and_valkey_schemes_are_redis() {
        for (uri, url) in [
            (
                "redis+unix:///tmp/redis.sock?stream=s",
                "redis+unix:///tmp/redis.sock",
            ),
            ("valkey://host:6379?stream=s", "redis://host:6379"),
            ("valkeys://host:6380?stream=s", "rediss://host:6380"),
            (
                "valkey+unix:///tmp/v.sock?stream=s",
                "redis+unix:///tmp/v.sock",
            ),
        ] {
            let cfg = config(uri, "redis_streams");
            assert_eq!(cfg["url"], url, "{uri}");
            assert_eq!(cfg["stream"], "s", "{uri}");
        }
    }

    // MongoDB sources are non-destructive by default in the CLI: `consume` is set
    // to `capture_all` when the user gives neither `consume` nor `change_stream`,
    // so pointing at an existing collection never claims/deletes its documents.
    #[test]
    fn mongodb_defaults_to_non_destructive_capture_all() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "capture_all");
    }

    // An explicit `consume` (or the deprecated `change_stream`) always wins over
    // the non-destructive default.
    #[test]
    fn mongodb_explicit_consume_wins_over_default() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&consume=delete",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "delete");
        // The old name of the mode is still read.
        let endpoint =
            endpoint_from_uri("mongodb://host/?collection=orders&database=appdb&consume=consumer")
                .unwrap();
        let EndpointType::MongoDb(mongo) = endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(mongo.resolved_consume(), MongoConsume::Consumer);

        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&change_stream=true",
            "mongodb",
        );
        assert!(cfg["consume"].is_null());
        assert_eq!(cfg["change_stream"], true);
        let endpoint = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&change_stream=true",
        )
        .unwrap();
        let EndpointType::MongoDb(mongo) = endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(mongo.resolved_consume(), MongoConsume::CaptureNew);

        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&consume=snapshot&change_stream=true",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "snapshot");
        assert_eq!(cfg["change_stream"], true);
        let endpoint = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&consume=snapshot&change_stream=true",
        )
        .unwrap();
        let EndpointType::MongoDb(mongo) = endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(mongo.resolved_consume(), MongoConsume::Snapshot);

        let error = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&consume=subscriber",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("subscriber"), "{error:#}");
    }

    // A redis URL path is the database number, not the stream, so it must remain on
    // the connection URL.
    #[test]
    fn redis_path_is_database_not_stream() {
        let cfg = config("redis://host:6379/0", "redis_streams");
        assert_eq!(cfg["url"], "redis://host:6379/0");
        assert!(cfg["stream"].is_null());
    }

    // Escaped mode: `?url=<encoded>` is used verbatim (its own options are never
    // re-interpreted), while sibling params still set config fields.
    #[test]
    fn escaped_url_is_verbatim() {
        let inner = "mongodb://u:p@host/db?tls=true&replicaSet=rs0";
        let mut outer = url::Url::parse("mongodb://_/").unwrap();
        outer
            .query_pairs_mut()
            .append_pair("url", inner)
            .append_pair("collection", "orders")
            .append_pair("database", "appdb");
        let cfg = config(outer.as_str(), "mongodb");
        assert_eq!(cfg["url"], inner);
        assert_eq!(cfg["collection"], "orders");
    }

    // `null:` builds the discard sink regardless of trailing content. A flattened
    // unit `Null` variant serializes as a `null` key with a null value.
    #[test]
    fn null_scheme_builds_null_endpoint() {
        let ep = endpoint_from_uri("null:").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert!(v.get("null").is_some(), "expected a null endpoint, got {v}");
    }

    // `static:` carries its payload in `?body=`; `raw=true` sends it verbatim.
    #[test]
    fn static_scheme_body_and_raw() {
        let cfg = config("static:?body=hello&raw=true", "static");
        assert_eq!(cfg["body"], "hello");
        assert_eq!(cfg["raw"], true);
    }

    // `memory://topic` maps the host to the channel topic.
    #[test]
    fn memory_scheme_topic_from_host() {
        let cfg = config("memory://my-topic?capacity=1000", "memory");
        assert_eq!(cfg["topic"], "my-topic");
        assert_eq!(cfg["capacity"], 1000);
    }

    // `postgres_cdc://` selects the CDC endpoint; the connection URL is rebuilt
    // with a plain `postgres` scheme, and `publication` is a scalar config field.
    #[test]
    fn postgres_cdc_scheme_rewrites_url_and_takes_publication() {
        let cfg = config(
            "postgres-cdc://u:p@host:5432/db?publication=mqb_pub&slot_name=mqb_slot",
            "postgres_cdc",
        );
        assert_eq!(cfg["url"], "postgres://u:p@host:5432/db");
        assert_eq!(cfg["publication"], "mqb_pub");
        assert_eq!(cfg["slot_name"], "mqb_slot");
    }

    // In escaped mode the connection string is complete, so a stray driver-style
    // param is rejected rather than silently dropped.
    #[test]
    fn escaped_url_rejects_stray_param() {
        let mut outer = url::Url::parse("mongodb://_/").unwrap();
        outer
            .query_pairs_mut()
            .append_pair("url", "mongodb://host/db")
            .append_pair("database", "appdb")
            .append_pair("bogus", "x");
        let err = endpoint_from_uri(outer.as_str()).unwrap_err();
        assert!(err.to_string().contains("escaped mode"), "got: {err}");
    }

    // A file endpoint has no driver options, so an object-typed field such as
    // `encryption` is read as a JSON literal instead of being dropped.
    #[test]
    fn file_object_field_is_read_as_json_literal() {
        let mut uri = url::Url::parse("file:///tmp/out.jsonl").unwrap();
        uri.query_pairs_mut()
            .append_pair("format", "raw")
            .append_pair("compression", "gzip")
            .append_pair("encryption", r#"{"key_id":"k1","key":"${env:MQB_KEY}"}"#);
        let cfg = config(uri.as_str(), "file");
        assert_eq!(cfg["path"], "/tmp/out.jsonl");
        assert_eq!(cfg["format"], "raw");
        assert_eq!(cfg["compression"], "gzip");
        assert_eq!(cfg["encryption"]["key_id"], "k1");
        assert_eq!(cfg["encryption"]["key"], "${env:MQB_KEY}");
    }

    // The consumer mode is a `#[serde(flatten)]`ed tagged enum, so `mode` and its
    // variant fields are only recognised by walking the schema — they must not
    // trip the unrecognised-param error.
    #[test]
    fn file_flattened_mode_fields_are_recognised() {
        let cfg = config("file:///var/log/app.log?mode=subscribe&delete=true", "file");
        assert_eq!(cfg["mode"], "subscribe");
        assert_eq!(cfg["delete"], true);
    }

    /// `dir_spool` is targeted by path like `file`, under three scheme spellings
    /// (an underscore is not legal in a URI scheme).
    #[test]
    fn spool_scheme_takes_the_path_from_the_uri() {
        for uri in [
            "spool:///tmp/video?payload_extension=.h264",
            "dir-spool:///tmp/video?payload_extension=.h264",
            "dirspool:///tmp/video?payload_extension=.h264",
        ] {
            let cfg = config(uri, "dir_spool");
            assert_eq!(cfg["path"], "/tmp/video", "for {uri}");
            assert_eq!(cfg["payload_extension"], ".h264", "for {uri}");
            assert!(cfg.get("url").is_none(), "{uri} must not carry a url field");
        }
    }

    /// The CSV dialect is a nested struct, so it arrives as one JSON parameter.
    #[test]
    fn file_csv_dialect_is_a_json_param() {
        let mut uri = url::Url::parse("file:///tmp/export.csv?format=csv").unwrap();
        uri.query_pairs_mut().append_pair(
            "csv",
            r#"{"separator":"auto","header":false,"columns":["id"]}"#,
        );
        let cfg = config(uri.as_str(), "file");
        assert_eq!(cfg["format"], "csv");
        assert_eq!(cfg["csv"]["separator"], "auto");
        assert_eq!(cfg["csv"]["columns"][0], "id");
    }

    /// The authority-less form has no `//` to strip, so the scheme has to come off on
    /// its own or the path keeps it and names a relative directory.
    #[test]
    fn a_path_uri_without_an_authority_still_yields_an_absolute_path() {
        for (uri, tag) in [
            ("spool:/tmp/video", "dir_spool"),
            ("dir-spool:/tmp/video", "dir_spool"),
            ("file:/tmp/out.jsonl", "file"),
        ] {
            assert_eq!(config(uri, tag)["path"], uri.split(':').nth(1).unwrap());
        }
    }

    /// The producer/consumer settings from the documented example are all scalar
    /// config fields, so none of them trips the unrecognised-param error.
    #[test]
    fn spool_producer_and_consumer_settings_are_config_fields() {
        let cfg = config(
            "spool:///tmp/q?naming_pattern={seq:06d}&emit_done=success&shard_depth=2",
            "dir_spool",
        );
        assert_eq!(cfg["naming_pattern"], "{seq:06d}");
        assert_eq!(cfg["emit_done"], "success");
        assert_eq!(cfg["shard_depth"], 2);

        let cfg = config(
            "spool:///tmp/q?drain_on_read=true&stop_on_done=true",
            "dir_spool",
        );
        assert_eq!(cfg["drain_on_read"], true);
        assert_eq!(cfg["stop_on_done"], true);
    }

    /// A spool has no connection URL, so an unrecognised param is a user error
    /// rather than a driver option riding along.
    #[test]
    fn spool_rejects_an_unrecognised_param() {
        let err = endpoint_from_uri("spool:///tmp/q?nonsense=1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonsense"), "got: {err}");
    }

    #[test]
    fn file_name_by_is_a_scalar_flag() {
        let cfg = config("file:///var/lib/mqb/parts?name_by=source_position", "file");
        assert_eq!(cfg["name_by"], "source_position");
    }

    /// `idempotency` is the deprecated spelling of `name_by`; v0.4.x URLs must keep working.
    #[test]
    fn file_idempotency_is_still_accepted() {
        let cfg = config("file:///var/lib/mqb/parts?idempotency=true", "file");
        assert_eq!(cfg["idempotency"], true);
    }

    // Rejecting unrecognised params on the endpoints that have no driver options
    // only works if every documented param really is a config field. These are the
    // example URIs from README.md, docs/book/ and benches/etl/.
    #[test]
    fn documented_example_uris_parse() {
        for uri in [
            // Endpoints with no connection-URL driver options: newly strict.
            "kafka://kafka.local:9092?topic=orders&group_id=mqb-orders-sync",
            "kafka://kafka.local:9093?topic=orders&username=svc&password=secret",
            "mqtt://broker.local:1883?topic=alerts&client_id=mqb-alerts-01&qos=2",
            "mqtts://user:pass@broker.local:8883?topic=events",
            "nats://localhost:4222?subject=orders",
            "zeromq://127.0.0.1:5555?socket_type=push",
            "grpc://localhost:50051?topic=orders",
            "ibmmq://qmhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=orders",
            "aws://_/?queue_url=https://sqs.us-east-1.amazonaws.com/123/orders&region=us-east-1",
            "memory://my-topic?capacity=1000",
            "file:///data/customers.csv?format=csv",
            "file:///var/log/app/events.log?mode=subscribe",
            // Endpoints that do carry driver options: `sslmode`/`async_insert` are
            // driver options, not config fields, and must still pass through.
            "postgres://u:p@localhost:5432/db?table=bench&cursor_column=id&sslmode=disable",
            "clickhouse://user:pass@ch.local:8123?table=events&database=analytics&async_insert=true",
            "amqp://guest:guest@localhost:5672/%2f?exchange=events&queue=events",
            "mongodb://localhost?database=app&collection=orders&consume=capture_new",
            "postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot",
            "https://api.example.com/ingest?method=POST&request_timeout_ms=5000",
        ] {
            if let Err(e) = endpoint_from_uri(uri) {
                panic!("documented URI should parse: {uri}\n  {e:#}");
            }
        }
    }

    // On an endpoint whose URL does carry driver options, a JSON literal picks the
    // nested config field while a scalar of the same name stays a driver option
    // (see `mongodb_tls_option_stays_on_url` for the scalar half).
    #[test]
    fn mongodb_tls_json_literal_is_config() {
        let mut uri = url::Url::parse("mongodb://host:27017/").unwrap();
        uri.query_pairs_mut()
            .append_pair("database", "appdb")
            .append_pair("tls", r#"{"required":true,"ca_file":"/etc/ca.pem"}"#);
        let cfg = config(uri.as_str(), "mongodb");
        assert_eq!(cfg["tls"]["required"], true);
        assert_eq!(cfg["tls"]["ca_file"], "/etc/ca.pem");
        assert_eq!(cfg["url"], "mongodb://host:27017/");
    }

    // Kafka's connection string is a bare `host:port` list, not a URI, so a param
    // appended to it could never be read as a driver option — an object field is
    // config, and an unrecognised name is an error.
    #[test]
    fn kafka_object_fields_are_config_not_url_junk() {
        let mut uri = url::Url::parse("kafka://broker:9092").unwrap();
        uri.query_pairs_mut()
            .append_pair("topic", "orders")
            .append_pair("tls", r#"{"required":true}"#)
            .append_pair("producer_options", r#"[["linger.ms","5"]]"#);
        let cfg = config(uri.as_str(), "kafka");
        assert_eq!(cfg["url"], "broker:9092");
        assert_eq!(cfg["tls"]["required"], true);
        assert_eq!(cfg["producer_options"][0][0], "linger.ms");
    }

    #[test]
    fn kafka_rejects_unrecognised_param() {
        let err = endpoint_from_uri("kafka://broker:9092?topic=t&bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // IBM MQ's `host(port)` connection string is likewise not a URI; `tls` is a
    // nested struct that was previously unreachable from a URI.
    #[test]
    fn ibmmq_tls_is_config_and_url_stays_host_port() {
        let mut uri = url::Url::parse("ibmmq://qmhost:1414").unwrap();
        uri.query_pairs_mut()
            .append_pair("queue_manager", "QM1")
            .append_pair("channel", "DEV.APP.SVRCONN")
            .append_pair("queue", "orders")
            .append_pair("tls", r#"{"required":true,"cipher_spec":"ANY_TLS12"}"#);
        let cfg = config(uri.as_str(), "ibmmq");
        assert_eq!(cfg["url"], "qmhost(1414)");
        assert_eq!(cfg["tls"]["cipher_spec"], "ANY_TLS12");
    }

    // AwsConfig has no `url` field, so a leftover param had nothing to ride on and
    // was dropped on the floor.
    #[test]
    fn aws_rejects_unrecognised_param() {
        let err = endpoint_from_uri("aws://_/?region=us-east-1&bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // An in-process channel has no connection URL at all.
    #[test]
    fn memory_rejects_unrecognised_param() {
        let err = endpoint_from_uri("memory://my-topic?bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // `path` is a real WebSocketConfig field; it used to be skipped for every
    // scheme because a file endpoint derives its path from the URI.
    #[test]
    fn websocket_path_param_reaches_config() {
        let cfg = config("ws://host:8080?path=/stream", "websocket");
        assert_eq!(cfg["path"], "/stream");
    }

    // A param that is not a FileConfig field can never take effect, so it is
    // rejected rather than silently ignored.
    #[test]
    fn file_rejects_unrecognised_param() {
        let err = endpoint_from_uri("file:///tmp/out.jsonl?bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // An object-typed field given something that is not JSON at all is reported as
    // such, rather than reaching serde as a bare string. (A value that *is* valid
    // JSON but the wrong shape still gets serde's own type error.)
    #[test]
    fn file_object_field_rejects_non_json() {
        let err = endpoint_from_uri("file:///tmp/out.jsonl?encryption=yes-please").unwrap_err();
        assert!(
            err.to_string().contains("expects a JSON literal"),
            "got: {err}"
        );
    }

    // `|`-separated middlewares wrap the endpoint in the order written, and their
    // params are coerced to the middleware config field's own type.
    #[test]
    fn middlewares_are_appended_in_order() {
        let ep = endpoint_from_uri("kafka://broker:9092?topic=orders|retry?max_attempts=5|metrics")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["kafka"]["topic"], "orders");
        let mw = v["middlewares"].as_array().expect("middlewares array");
        assert_eq!(mw.len(), 2);
        assert_eq!(mw[0]["retry"]["max_attempts"], 5);
        assert!(mw[1].get("metrics").is_some(), "got: {}", mw[1]);
    }

    // A middleware's own object-typed field takes a JSON literal, and `-` in the
    // name is accepted for the snake_case tag.
    #[test]
    fn middleware_dash_alias_and_json_field() {
        let ep = endpoint_from_uri(
            "null:|weak-join?group_by=cid&expected_count=2&timeout_ms=1000&required=[\"a\",\"b\"]",
        )
        .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let wj = &v["middlewares"][0]["weak_join"];
        assert_eq!(wj["group_by"], "cid");
        assert_eq!(wj["required"], serde_json::json!(["a", "b"]));
    }

    // `transform`'s `schema` is an untyped `serde_json::Value`, so it must still
    // be read as a JSON literal rather than handed through as a string (which
    // `transform` rejects with "schema must be a JSON object").
    #[test]
    fn transform_schema_param_is_a_json_literal() {
        let schema = r#"{"type":"object","properties":{"qty":{"type":"number"}}}"#;
        let mut spec = String::from("null:|transform?");
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("schema", schema);
        spec.push_str(&q.finish());

        let ep = endpoint_from_uri(&spec).expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(
            v["middlewares"][0]["transform"]["schema"],
            serde_json::from_str::<serde_json::Value>(schema).unwrap()
        );
    }

    // `dlq`'s `endpoint` param is itself an endpoint URI, parsed recursively.
    #[test]
    fn dlq_endpoint_param_is_a_nested_uri() {
        let mut spec = String::from("kafka://broker:9092?topic=orders|dlq?");
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("endpoint", "file:///tmp/failed.jsonl");
        spec.push_str(&q.finish());
        let ep = endpoint_from_uri(&spec).expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(
            v["middlewares"][0]["dlq"]["endpoint"]["file"]["path"],
            "/tmp/failed.jsonl"
        );
    }

    // A `--from` http endpoint is a listener, and its driver takes a bare
    // `host:port` — the scheme the URI needed to select the endpoint would
    // otherwise reach it as part of the address.
    #[test]
    fn an_http_source_url_becomes_a_listen_address() {
        let mut ep =
            endpoint_from_uri("http://0.0.0.0:8080?method=POST").expect("uri should parse");
        make_listen_address(&mut ep).unwrap();
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["http"]["url"], "0.0.0.0:8080");
        assert_eq!(v["http"]["method"], "POST");
        assert_eq!(v["http"]["tls"]["required"], false);

        // `https` asks for a TLS listener; the certificate still comes from `tls`.
        let mut ep = endpoint_from_uri("https://0.0.0.0:8443").expect("uri should parse");
        make_listen_address(&mut ep).unwrap();
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["http"]["url"], "0.0.0.0:8443");
        assert_eq!(v["http"]["tls"]["required"], true);

        // Endpoints that are never servers keep their connection URL.
        let mut ep = endpoint_from_uri("kafka://broker:9092?topic=orders").expect("parses");
        let before = serde_json::to_value(&ep).unwrap();
        make_listen_address(&mut ep).unwrap();
        assert_eq!(serde_json::to_value(&ep).unwrap(), before);

        // `wss` asks for a TLS listener too.
        let mut ep = endpoint_from_uri(
            r#"wss://0.0.0.0:9000?tls={"cert_file":"/c.pem","key_file":"/k.pem"}"#,
        )
        .expect("uri should parse");
        make_listen_address(&mut ep).unwrap();
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["websocket"]["url"], "0.0.0.0:9000");
        assert_eq!(v["websocket"]["tls"]["required"], true);
        assert_eq!(v["websocket"]["tls"]["cert_file"], "/c.pem");
    }

    // The mirror-proxy shape: every branch gets the message, and only the `to`
    // branch is left able to answer the caller.
    #[test]
    fn fanout_mirrors_and_keeps_one_answering_branch() {
        let ep =
            endpoint_from_uri("fanout:?mirror=http://staging.internal/&to=http://prod.internal/")
                .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let branches = v["fanout"].as_array().expect("branches keep their order");
        assert_eq!(branches.len(), 2);

        // The mirror is wrapped so its response and its failures go nowhere.
        assert_eq!(
            branches[0]["request"]["to"]["http"]["url"],
            "http://staging.internal/"
        );
        assert!(branches[0]["request"]["forward_to"].get("null").is_some());
        assert_eq!(branches[1]["http"]["url"], "http://prod.internal/");
    }

    #[test]
    fn structural_uris_nest_endpoints_and_name_their_mistakes() {
        // A nested URI with its own query params is percent-encoded.
        let ep = endpoint_from_uri("request:?to=http%3A%2F%2Fapi.internal%2F%3Fmethod%3DPUT")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["request"]["to"]["http"]["method"], "PUT");
        // Without a `forward_to` the response is discarded.
        assert!(v["request"]["forward_to"].get("null").is_some());

        let ep = endpoint_from_uri("switch:?metadata_key=http_status_code&case.200=null:&default=file%3A%2F%2F%2Ftmp%2Fother.jsonl")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["switch"]["metadata_key"], "http_status_code");
        assert!(v["switch"]["cases"]["200"].get("null").is_some());
        assert_eq!(v["switch"]["default"]["file"]["path"], "/tmp/other.jsonl");

        // Predicate mode: `when`/`to` pairs keep their order, and an expression
        // needs no escaping for its `=` because it travels as a query *value*.
        let ep = endpoint_from_uri(
            "switch:?when=amount > 100&to=null:&when=status == 'paid'&to=file%3A%2F%2F%2Ftmp%2Fpaid.jsonl&default=file%3A%2F%2F%2Ftmp%2Frest.jsonl",
        )
        .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["switch"]["when"][0]["if"], "amount > 100");
        assert!(v["switch"]["when"][0]["to"].get("null").is_some());
        assert_eq!(v["switch"]["when"][1]["if"], "status == 'paid'");
        assert_eq!(
            v["switch"]["when"][1]["to"]["file"]["path"],
            "/tmp/paid.jsonl"
        );
        assert_eq!(v["switch"]["default"]["file"]["path"], "/tmp/rest.jsonl");
        assert_eq!(v["switch"]["metadata_key"], "");

        let ep = endpoint_from_uri("response:").expect("uri should parse");
        assert!(serde_json::to_value(&ep).unwrap().get("response").is_some());

        for (uri, expected) in [
            ("fanout:", "no branches"),
            // Named by the key, not by whatever its value fails to parse as.
            (
                "fanout:?towards=bogus://x",
                "unsupported query param 'towards'",
            ),
            ("response:?to=null:", "unsupported query param 'to'"),
            ("http-bulk:", "needs 'config_file=<path>'"),
            ("http-bulk:?config=a&config_file=b", "twice"),
            ("http-bulk:?config=url: [", "not valid YAML or JSON"),
            (
                "http-bulk:?config={url: 'http://h', upsert: {path: /d, foo: 1}}",
                "unknown field `foo`",
            ),
            ("request:?forward_to=null:", "needs a 'to=<uri>'"),
            ("request:?to=null:&to=null:", "duplicate query param 'to'"),
            (
                "request:?to=null:&forward_to=null:&forward_to=null:",
                "duplicate query param 'forward_to'",
            ),
            ("switch:?case.200=null:", "needs a 'metadata_key=<key>'"),
            ("switch:?when=amount > 100", "with no 'to=<uri>' after it"),
            (
                "switch:?when=a&when=b&to=null:",
                "with no 'to=<uri>' after it",
            ),
            ("switch:?to=null:", "that no 'when=<expression>' precedes"),
            (
                "switch:?metadata_key=k&case.1=null:&when=amount > 100&to=null:",
                "mixes both modes",
            ),
            (
                "fanout:?to=bogus://x",
                "unsupported endpoint scheme 'bogus'",
            ),
        ] {
            let err = format!("{:#}", endpoint_from_uri(uri).unwrap_err());
            assert!(err.contains(expected), "{uri}: got {err}");
        }
    }

    // A URI that fails to parse is quoted in the error without its credentials.
    #[test]
    fn uri_errors_do_not_quote_credentials() {
        for uri in [
            "mongodb://app:hunter2@db/shop?bogus=1",
            "s3://bucket/prefix?secret_access_key=hunter2&bogus=1",
            "http://host/x?basic_auth=%5B%22app%22%2C%22hunter2%22%5D&request_timeout_ms=soon",
            "fanout:?to=bogus://app:hunter2@host/x",
            "null:|encryption?key=hunter2&bogus=1",
            "null:|dlq?endpoint=bogus://app:hunter2@host/x",
            "http://host/x?basic_auth=[hunter2",
            "switch:?to=bogus://app:hunter2@host/x",
        ] {
            let err = format!("{:#}", endpoint_from_uri(uri).unwrap_err());
            assert!(!err.contains("hunter2"), "{uri}: got {err}");
        }
    }

    // An unknown middleware name is rejected with the supported list.
    #[test]
    fn unknown_middleware_is_rejected() {
        let err = endpoint_from_uri("null:|bogus").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported middleware 'bogus'"), "got: {msg}");
    }

    // A registered middleware builds `custom`, its params typed by its declared
    // schema, and is named in the unknown-middleware error.
    #[test]
    fn registered_middleware_builds_a_custom_middleware() {
        #[derive(Debug)]
        struct Declaring;

        impl mq_bridge::traits::CustomMiddlewareFactory for Declaring {
            fn config_schema(&self) -> Option<serde_json::Value> {
                Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer" },
                        "id": { "type": "string" }
                    }
                }))
            }
        }

        let name = "cli-test-declaring-mw";
        mq_bridge::extensions::register_middleware_factory(name, std::sync::Arc::new(Declaring))
            .unwrap();
        let built = super::middleware_from_spec(&format!("{name}?limit=5&id=0123"));
        let err = format!("{:#}", super::middleware_from_spec("bogus").unwrap_err());
        mq_bridge::extensions::unregister_middleware_factory(name);

        let mq_bridge::models::Middleware::Custom {
            name: built_name,
            config,
        } = built.unwrap()
        else {
            panic!("expected a custom middleware");
        };
        assert_eq!(built_name, name);
        assert_eq!(config, serde_json::json!({ "limit": 5, "id": "0123" }));
        assert!(err.contains(name), "got: {err}");
    }

    // Every `Middleware` variant is reachable from a spec, so a new one can't
    // go missing here. Some reject an empty config, but never as unsupported.
    #[test]
    fn every_middleware_variant_is_accepted_by_name() {
        let root =
            serde_json::to_value(schemars::schema_for!(mq_bridge::models::Middleware)).unwrap();
        let tags = super::middleware_tags(&root);
        assert!(tags.contains(&"otel".to_string()), "got: {tags:?}");
        for tag in tags {
            if let Err(err) = super::middleware_from_spec(&tag) {
                let msg = format!("{err:#}");
                assert!(!msg.contains("unsupported middleware"), "{tag}: {msg}");
            }
        }
    }

    #[test]
    fn string_middlewares_take_the_raw_query() {
        let ep = endpoint_from_uri("null:|id?${payload:order_id}|filter?amount%20%3E%20100|otel")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["id"], "${payload:order_id}");
        assert_eq!(v["middlewares"][1]["filter"], "amount > 100");
        assert!(v["middlewares"][2].get("otel").is_some(), "got: {v}");
    }

    #[test]
    fn filter_takes_its_map_form_from_named_fields() {
        let ep = endpoint_from_uri("null:|filter?expression=amount%20%3E%20100&on_error=drop")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["filter"]["expression"], "amount > 100");
        assert_eq!(v["middlewares"][0]["filter"]["on_error"], "drop");

        // `==` is not a field assignment, so this stays a bare expression.
        let ep = endpoint_from_uri("null:|filter?on_error%20==%201").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["filter"], "on_error == 1");
    }

    // An endpoint-typed field anywhere takes a URI, not only `dlq`'s.
    #[test]
    fn lookup_from_takes_an_endpoint_uri() {
        let ep =
            endpoint_from_uri("null:|lookup?from=null%3A&into=extra").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let lookup = &v["middlewares"][0]["lookup"];
        assert_eq!(lookup["into"], "extra");
        assert!(lookup["from"].get("null").is_some(), "got: {lookup}");
    }

    // Without a declared schema every param stays a string, and a registered
    // `_` name resolves from its `-` spelling.
    #[test]
    fn undeclared_registered_middleware_keeps_params_as_strings() {
        #[derive(Debug)]
        struct Silent;

        impl mq_bridge::traits::CustomMiddlewareFactory for Silent {}

        let name = "cli_test_silent_mw";
        mq_bridge::extensions::register_middleware_factory(name, std::sync::Arc::new(Silent))
            .unwrap();
        let built = super::middleware_from_spec("cli-test-silent-mw?id=0123");
        mq_bridge::extensions::unregister_middleware_factory(name);

        let mq_bridge::models::Middleware::Custom {
            name: built_name,
            config,
        } = built.unwrap()
        else {
            panic!("expected a custom middleware");
        };
        assert_eq!(built_name, name);
        assert_eq!(config, serde_json::json!({ "id": "0123" }));
    }

    // A scheme naming a registered endpoint builds `custom`, so `copy` can
    // address an extension or `--plugin` endpoint the URI parser knows nothing
    // about. `pulsar` is compiled in, so registering it is enough to exercise it.
    #[test]
    #[cfg(feature = "pulsar")]
    fn registered_endpoint_scheme_builds_a_custom_endpoint() {
        crate::plugins::register_builtin_endpoints().unwrap();

        let endpoint = endpoint_from_uri(
            "pulsar://localhost:6650?topic=persistent://public/default/orders&subscription=workers",
        )
        .unwrap();

        let EndpointType::Custom { name, config } = endpoint.endpoint_type else {
            panic!(
                "expected a custom endpoint, got {:?}",
                endpoint.endpoint_type
            );
        };
        assert_eq!(name, "pulsar");
        assert_eq!(config["url"], "pulsar://localhost:6650");
        assert_eq!(config["topic"], "persistent://public/default/orders");
        assert_eq!(config["subscription"], "workers");
    }

    /// A named endpoint's key and config, as a route file writes them.
    #[cfg(feature = "http-bulk")]
    fn named(endpoint: &mq_bridge::models::Endpoint) -> (String, serde_json::Value) {
        let written = serde_json::to_value(&endpoint.endpoint_type).unwrap();
        let (name, config) = written.as_object().unwrap().iter().next().unwrap();
        (name.clone(), config.clone())
    }

    // Meilisearch reaches `copy` as its own endpoint type, which normalizes the
    // `meilisearch://` url to the HTTP one the server speaks.
    #[test]
    #[cfg(feature = "http-bulk")]
    fn meilisearch_scheme_builds_its_endpoint() {
        crate::plugins::register_builtin_endpoints().unwrap();

        let endpoint =
            endpoint_from_uri("meilisearch://localhost:7700?index=orders&primary_key=id").unwrap();

        assert!(matches!(
            endpoint.endpoint_type,
            EndpointType::Meilisearch(_)
        ));
        let (_, config) = named(&endpoint);
        assert_eq!(config["url"], "meilisearch://localhost:7700");
        assert_eq!(config["index"], "orders");
        assert_eq!(config["primary_key"], "id");
    }

    // A named `http_bulk` endpoint takes its address and its collection or index
    // from the URI, so a load needs no config file.
    #[test]
    #[cfg(feature = "http-bulk")]
    fn http_bulk_preset_schemes_build_their_endpoints() {
        crate::plugins::register_builtin_endpoints().unwrap();

        for (uri, url, field) in [
            (
                "typesense://localhost:8108/books?api_key=k",
                "typesense://localhost:8108",
                "collection",
            ),
            (
                "elasticsearch+https://es.example.com/books?api_key=k",
                "https://es.example.com",
                "index",
            ),
        ] {
            let endpoint = endpoint_from_uri(uri).unwrap();
            let (_, config) = named(&endpoint);
            assert_eq!(config["url"], url);
            assert_eq!(config[field], "books");
            assert_eq!(config["api_key"], "k");
        }
        for (uri, name, url, field, value) in [
            (
                "qdrant://localhost:6333/books?wait=false",
                "qdrant",
                "qdrant://localhost:6333",
                "wait",
                serde_json::json!(false),
            ),
            (
                "openai-chat://localhost:11434?model=m&prompt=Hi&json=true",
                "openai_chat",
                "openai-chat://localhost:11434",
                "json",
                serde_json::json!(true),
            ),
            (
                "openai+https://llm.example.com/v1?model=m&prompt=Hi&concurrency=2",
                "openai_chat",
                "https://llm.example.com/v1",
                "concurrency",
                serde_json::json!(2),
            ),
            (
                "openai://localhost:11434?model=m&prompt=Hi&messages_per_request=20",
                "openai_chat",
                "openai://localhost:11434",
                "messages_per_request",
                serde_json::json!(20),
            ),
            (
                "openai-embeddings+https://llm.example.com/v1?model=m&input=x&dimensions=8",
                "openai_embeddings",
                "https://llm.example.com/v1",
                "dimensions",
                serde_json::json!(8),
            ),
        ] {
            let endpoint = endpoint_from_uri(uri).unwrap();
            let (found, config) = named(&endpoint);
            assert_eq!(found, name);
            assert_eq!(config["url"], url);
            assert_eq!(config[field], value, "{uri}");
        }
        let error = endpoint_from_uri("typesense://h/books?request_timeout_ms=soon").unwrap_err();
        assert!(
            format!("{error:#}").contains("request_timeout_ms"),
            "{error:#}"
        );
    }

    // Deliberately ungated: the features that name an extension here are this
    // crate's, while the one that compiles it in is the core crate's. A build
    // enabling only the latter registers the endpoint but omits it from the
    // error, and every gated test would still pass.
    #[test]
    fn every_registered_extension_is_named_in_the_scheme_error() {
        crate::plugins::register_builtin_endpoints().unwrap();
        let schemes = super::extension_schemes();

        for name in [
            "pulsar",
            "meilisearch",
            "typesense",
            "elasticsearch",
            "qdrant",
            "openai_chat",
            "openai_embeddings",
        ] {
            if mq_bridge::extensions::get_endpoint_factory(name).is_some() {
                assert!(
                    schemes.contains(&name.replace('_', "-")),
                    "`{name}` is registered but missing from `{schemes}`: enable the \
                     `{name}` feature of this crate alongside the core one"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "pulsar")]
    fn registered_endpoint_url_preserves_a_trailing_slash() {
        crate::plugins::register_builtin_endpoints().unwrap();

        let endpoint = endpoint_from_uri("pulsar://localhost:6650/?topic=orders").unwrap();

        let EndpointType::Custom { config, .. } = endpoint.endpoint_type else {
            panic!("expected a custom endpoint");
        };
        assert_eq!(config["url"], "pulsar://localhost:6650/");
    }

    // An unregistered scheme still fails fast rather than becoming a `custom`
    // endpoint that would only break later with "no factory named ...".
    #[test]
    fn unregistered_scheme_still_fails_fast() {
        let err = endpoint_from_uri("kafkaa://broker:9092").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported endpoint scheme 'kafkaa'"),
            "got: {msg}"
        );
    }

    #[derive(Debug)]
    struct SubschemeFactory;

    impl mq_bridge::traits::CustomEndpointFactory for SubschemeFactory {
        fn config_schema(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "component": { "type": "string", "x-mqb-uri": "subscheme" },
                    "url": { "type": "string", "x-mqb-uri": "url" },
                    "subject": { "type": "string", "x-mqb-uri": "path" }
                }
            }))
        }
    }

    // `plugin+component://` names the plugin before the `+`; the component
    // reaches the factory's config through its schema.
    #[test]
    fn a_plugin_scheme_with_a_component_resolves_the_plugin() {
        let _ = mq_bridge::extensions::register_endpoint_factory(
            "subschemetest",
            std::sync::Arc::new(SubschemeFactory),
        );

        let endpoint =
            endpoint_from_uri("subschemetest+nats-jetstream://127.0.0.1:4222/subj.data").unwrap();

        let EndpointType::Custom { name, config } = endpoint.endpoint_type else {
            panic!(
                "expected a custom endpoint, got {:?}",
                endpoint.endpoint_type
            );
        };
        assert_eq!(name, "subschemetest");
        assert_eq!(config["component"], "nats-jetstream");
        assert_eq!(config["url"], "nats-jetstream://127.0.0.1:4222/subj.data");
        assert_eq!(config["subject"], "subj.data");
    }

    // The search-path hint names the plugin's library, not one for the whole scheme.
    #[test]
    fn an_unknown_plugin_with_a_component_names_the_plugin_library() {
        let err = endpoint_from_uri("nosuchplugin+nats-jetstream://127.0.0.1:4222").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported endpoint scheme 'nosuchplugin+nats-jetstream'"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("jetstream."),
            "hint names the wrong library: {msg}"
        );
    }

    // `kafka://` selects the Kafka endpoint; the scheme is stripped so `url`
    // (rdkafka's bootstrap.servers) is a bare host:port, and `topic` is scalar.
    #[test]
    fn kafka_scheme_strips_prefix_and_takes_topic() {
        let cfg = config("kafka://broker:9092?topic=orders", "kafka");
        assert_eq!(cfg["url"], "broker:9092");
        assert_eq!(cfg["topic"], "orders");
        assert_eq!(
            config("kafka://broker:9092?topic=a,b", "kafka")["topic"],
            "a,b"
        );
        assert_eq!(
            config("kafka://broker:9092/?topic=o", "kafka")["url"],
            "broker:9092"
        );
    }

    #[test]
    fn kafka_source_metadata_is_a_scalar_flag() {
        let cfg = config(
            "kafka://broker:9092?topic=orders&source_metadata=true",
            "kafka",
        );
        assert_eq!(cfg["source_metadata"], true);
    }

    // `mqtt://` is rewritten to `tcp://` (what rumqtt expects); `mqtts://`
    // becomes `ssl://`.
    #[test]
    fn mqtt_scheme_rewrites_to_tcp_and_ssl() {
        let cfg = config("mqtt://broker:1883?topic=sensors", "mqtt");
        assert_eq!(cfg["url"], "tcp://broker:1883");
        assert_eq!(cfg["topic"], "sensors");

        let cfg = config("mqtts://broker:8883?topic=sensors", "mqtt");
        assert_eq!(cfg["url"], "ssl://broker:8883");
    }

    // `rabbitmq://` is accepted as an alias for `amqp://`, rewritten to the
    // native scheme; `queue` is a scalar config field.
    #[test]
    fn rabbitmq_scheme_rewrites_to_amqp() {
        let cfg = config(
            "rabbitmq://guest:guest@host:5672/vhost?queue=orders",
            "amqp",
        );
        assert_eq!(cfg["url"], "amqp://guest:guest@host:5672/vhost");
        assert_eq!(cfg["queue"], "orders");
    }

    // `http://`/`https://` pass through unchanged, with the target path already
    // part of the URL; `method` is a scalar config field.
    #[test]
    fn http_scheme_passthrough_and_scalar_fields() {
        let cfg = config("http://api.example.com/ingest?method=POST", "http");
        assert_eq!(cfg["url"], "http://api.example.com/ingest");
        assert_eq!(cfg["method"], "POST");
    }

    // `clickhouse://` is rewritten to `http://` (ClickHouse's HTTP interface);
    // `table` and `database` are scalar config fields.
    #[test]
    fn clickhouse_scheme_rewrites_to_http() {
        let cfg = config(
            "clickhouse://host:8123?table=events&database=analytics",
            "clickhouse",
        );
        assert_eq!(cfg["url"], "http://host:8123");
        assert_eq!(cfg["table"], "events");
        assert_eq!(cfg["database"], "analytics");
    }

    // Storage schemes select the `object_store` endpoint. Cloud URLs pass through,
    // while CLI aliases are normalized to schemes understood by the backing crate.
    // `cursor_id`/`checkpoint_store` are scalar fields.
    #[test]
    fn object_store_bucket_schemes() {
        let cfg = config(
            "s3://my-bucket/events?cursor_id=replayer&checkpoint_store=file:///tmp/c.json",
            "object_store",
        );
        assert_eq!(cfg["url"], "s3://my-bucket/events");
        assert_eq!(cfg["cursor_id"], "replayer");
        assert_eq!(cfg["checkpoint_store"], "file:///tmp/c.json");

        assert_eq!(config("gs://b/p", "object_store")["url"], "gs://b/p");
        assert_eq!(config("az://b/p", "object_store")["url"], "az://b/p");
        assert_eq!(config("gcs://b/p", "object_store")["url"], "gs://b/p");
        assert_eq!(
            config("local-store:///var/lib/mqb/incoming", "object_store")["url"],
            "file:///var/lib/mqb/incoming"
        );
    }

    #[test]
    fn object_store_name_by_is_a_scalar_flag() {
        let cfg = config(
            "s3://my-bucket/events?name_by=source_position",
            "object_store",
        );
        assert_eq!(cfg["name_by"], "source_position");
        let cfg = config("local-store:///var/lib/mqb/parts?fsync=off", "object_store");
        assert_eq!(cfg["fsync"], "off");
    }

    /// `idempotency` is the deprecated spelling of `name_by`; v0.4.x URLs must keep working.
    #[test]
    fn object_store_idempotency_is_still_accepted() {
        let cfg = config("s3://my-bucket/events?idempotency=true", "object_store");
        assert_eq!(cfg["idempotency"], true);
    }

    // `ws://`/`wss://` pass through unchanged.
    #[test]
    fn websocket_scheme_passthrough() {
        let cfg = config("ws://0.0.0.0:9000", "websocket");
        assert_eq!(cfg["url"], "ws://0.0.0.0:9000/");
    }

    // `grpc://` is rewritten to `http://` (the client-mode URL GrpcConfig
    // expects); `topic` is a scalar config field.
    #[test]
    fn grpc_scheme_rewrites_to_http() {
        let cfg = config("grpc://localhost:50051?topic=orders", "grpc");
        assert_eq!(cfg["url"], "http://localhost:50051");
        assert_eq!(cfg["topic"], "orders");
    }

    // The documented lake-export target. `format` decides whether rows are written
    // as themselves or wrapped in a stringified envelope, so it has to survive the
    // URI rather than falling back to the sink's `normal` default; `extension` and
    // `compression` ride alongside it as scalar config fields.
    #[test]
    fn object_store_target_carries_format_extension_and_compression() {
        let cfg = config(
            "s3://lake/orders?format=raw&extension=jsonl.zst&compression=zstd",
            "object_store",
        );
        assert_eq!(cfg["url"], "s3://lake/orders");
        assert_eq!(cfg["format"], "raw");
        assert_eq!(cfg["compression"], "zstd");
        // Spelled out including the compression suffix on purpose: overriding
        // `extension` replaces the derived name wholesale, so `extension=jsonl`
        // beside `compression=zstd` would name a zstd object `.jsonl`.
        assert_eq!(cfg["extension"], "jsonl.zst");
    }

    // `ibmmq://` is reformatted to the driver's `host(port)` connection string;
    // `queue_manager` and `channel` are required scalar config fields.
    #[test]
    fn ibmmq_scheme_reformats_host_port() {
        let cfg = config(
            "ibmmq://qmhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=orders",
            "ibmmq",
        );
        assert_eq!(cfg["url"], "qmhost(1414)");
        assert_eq!(cfg["queue_manager"], "QM1");
        assert_eq!(cfg["channel"], "DEV.APP.SVRCONN");
        assert_eq!(cfg["queue"], "orders");
        let alias = config(
            "ibm-mq://qmhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN",
            "ibmmq",
        );
        assert_eq!(alias["url"], "qmhost(1414)");
    }

    // AWS SQS/SNS has no connection URL: the placeholder authority is dropped,
    // and `queue_url`/`region` are scalar config fields set via query params.
    #[test]
    fn aws_scheme_has_no_url_field() {
        let cfg = config(
            "aws://_/?queue_url=https://sqs.us-east-1.amazonaws.com/123/orders&region=us-east-1",
            "aws",
        );
        assert!(
            cfg.get("url").is_none(),
            "aws config should have no url field, got {cfg}"
        );
        assert_eq!(
            cfg["queue_url"],
            "https://sqs.us-east-1.amazonaws.com/123/orders"
        );
        assert_eq!(cfg["region"], "us-east-1");
    }

    // `zeromq://`/`zmq://` are rewritten to `tcp://`, the transport ZeroMQ expects.
    #[test]
    fn zeromq_scheme_rewrites_to_tcp() {
        let cfg = config("zeromq://127.0.0.1:5555?socket_type=push", "zeromq");
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");
        assert_eq!(cfg["socket_type"], "push");

        let cfg = config("zmq://127.0.0.1:5555", "zeromq");
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");

        // The form the book documents: the transport address after the scheme.
        let cfg = config(
            "zeromq://tcp://127.0.0.1:5555?socket_type=pull&bind=true",
            "zeromq",
        );
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");
        assert_eq!(cfg["bind"], true);
        let cfg = config("zmq://ipc:///tmp/mqb.sock", "zeromq");
        assert_eq!(cfg["url"], "ipc:///tmp/mqb.sock");
    }

    // `consume: capture_all` is the documented way to back fill a CDC source, so it has to
    // be reachable from a URI on both CDC endpoints.
    #[test]
    fn both_cdc_endpoints_take_consume_capture_all_from_a_uri() {
        let cfg = config(
            "postgres-cdc://u:p@host:5432/db?publication=mqb_pub&consume=capture_all",
            "postgres_cdc",
        );
        assert_eq!(cfg["consume"], "capture_all");

        // Mongo takes the database as a query param; the path stays part of the connection URL.
        let cfg = config(
            "mongodb://host:27017/?database=shop&collection=orders&consume=capture_all",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "capture_all");
    }

    /// An unknown scheme is often a plugin that is not installed yet, so the error
    /// has to name the file to install and where it looked — listing the built-in
    /// schemes alone sends the reader looking for a typo that is not there.
    #[test]
    fn an_unknown_scheme_names_the_plugin_file_it_looked_for() {
        let error = endpoint_from_uri("nosuchthing://host/orders")
            .map(|_| ())
            .expect_err("an unknown scheme cannot resolve");
        let message = format!("{error:#}");

        assert!(message.contains("nosuchthing"), "{message}");
        assert!(
            message.contains(&mq_bridge::plugin::library_file_name("nosuchthing")),
            "{message}"
        );
    }
}
