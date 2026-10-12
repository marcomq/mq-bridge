//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Named endpoints that are an `http_bulk` configuration: a few fields and a
//! URI of their own, and the generic publisher underneath.

use super::{HttpBulkConsumer, HttpBulkPublisher};
use crate::errors::InvalidConfig;
use crate::models::{
    ElasticsearchConfig, ElasticsearchMode, EndpointType, HttpBulkConfig, MeilisearchConfig,
    OpenAiChatConfig, OpenAiEmbeddingsConfig, QdrantConfig, TypesenseConfig,
};
use crate::traits::{CustomEndpointFactory, MessageConsumer, MessagePublisher};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// One named endpoint: the schema it shows and how its config becomes `http_bulk`.
#[derive(Debug)]
struct Preset {
    name: &'static str,
    /// `None` for a built-in one in a build without the `schema` feature.
    schema: Option<fn() -> Value>,
    /// Takes the route name and the endpoint's config.
    resolve: fn(&str, &Value) -> anyhow::Result<Value>,
}

const PRESETS: [Preset; 6] = [
    Preset {
        name: "elasticsearch",
        schema: derived::<ElasticsearchConfig>(),
        resolve: elasticsearch,
    },
    Preset {
        name: "meilisearch",
        schema: derived::<MeilisearchConfig>(),
        resolve: meilisearch,
    },
    Preset {
        name: "openai_chat",
        schema: derived::<OpenAiChatConfig>(),
        resolve: openai_chat,
    },
    Preset {
        name: "openai_embeddings",
        schema: derived::<OpenAiEmbeddingsConfig>(),
        resolve: openai_embeddings,
    },
    Preset {
        name: "qdrant",
        schema: derived::<QdrantConfig>(),
        resolve: qdrant,
    },
    Preset {
        name: "typesense",
        schema: derived::<TypesenseConfig>(),
        resolve: typesense,
    },
];

/// The endpoint names [`register_preset`] accepts.
pub fn preset_names() -> impl Iterator<Item = &'static str> {
    PRESETS.iter().map(|preset| preset.name)
}

/// Registers the named endpoint, e.g. `typesense`, as a custom endpoint factory.
pub fn register_preset(name: &str) -> anyhow::Result<()> {
    let preset = PRESETS
        .into_iter()
        .find(|preset| preset.name == name)
        .ok_or_else(|| anyhow!("no http_bulk preset is named `{name}`"))?;
    crate::extensions::register_endpoint_factory(name, Arc::new(preset))
}

/// Registers a named endpoint of your own as a custom endpoint factory.
///
/// `schema` describes its fields; `resolve` takes the route name and the
/// endpoint's config and returns the `http_bulk` configuration it stands for.
pub fn register_preset_with(
    name: &'static str,
    schema: fn() -> Value,
    resolve: fn(&str, &Value) -> anyhow::Result<Value>,
) -> anyhow::Result<()> {
    let preset = Preset {
        name,
        schema: Some(schema),
        resolve,
    };
    crate::extensions::register_endpoint_factory(name, Arc::new(preset))
}

/// The built-in named endpoint, for a route that names one nobody registered.
pub(crate) fn builtin(name: &str) -> Option<Arc<dyn CustomEndpointFactory>> {
    let preset = PRESETS.into_iter().find(|preset| preset.name == name)?;
    Some(Arc::new(preset))
}

#[cfg(feature = "schema")]
const fn derived<T: schemars::JsonSchema>() -> Option<fn() -> Value> {
    Some(|| serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default())
}

#[cfg(not(feature = "schema"))]
const fn derived<T>() -> Option<fn() -> Value> {
    None
}

/// The endpoint a typed config of the named endpoint stands for; unset fields are left out.
///
/// For a config type of your own, registered with [`register_preset_with`].
pub fn named_endpoint(name: &str, config: &impl Serialize) -> EndpointType {
    let mut config = serde_json::to_value(config).unwrap_or_default();
    if let Value::Object(fields) = &mut config {
        fields.retain(|_, value| !value.is_null());
    }
    EndpointType::Custom {
        name: name.to_string(),
        config,
    }
}

/// The `http_bulk` configuration a named endpoint stands for.
#[cfg(test)]
pub(super) fn resolve(name: &str, config: &Value) -> anyhow::Result<HttpBulkConfig> {
    let preset = PRESETS.into_iter().find(|preset| preset.name == name);
    preset
        .ok_or_else(|| anyhow!("no http_bulk preset is named `{name}`"))?
        .config("route", config)
}

impl Preset {
    fn config(&self, route_name: &str, value: &Value) -> anyhow::Result<HttpBulkConfig> {
        let resolved = (self.resolve)(route_name, value).map_err(InvalidConfig)?;
        serde_json::from_value(resolved)
            .with_context(|| format!("{} as http_bulk", self.name))
            .map_err(|error| InvalidConfig(error).into())
    }
}

#[async_trait]
impl CustomEndpointFactory for Preset {
    async fn create_consumer(
        &self,
        route_name: &str,
        config: &Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let config = self.config(route_name, config)?;
        if config.read.is_none() {
            return Err(InvalidConfig(anyhow!(
                "{} is an output only; read it through http_bulk with a 'read' request",
                self.name
            ))
            .into());
        }
        Ok(Box::new(HttpBulkConsumer::new(&config, false).await?))
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let config = self.config(route_name, config)?;
        Ok(Box::new(HttpBulkPublisher::new(&config)?))
    }

    fn config_schema(&self) -> Option<Value> {
        self.schema.map(|schema| schema())
    }
}

/// `name://host` is plain HTTP, `name+https://` and `names://` are HTTPS; `http(s)://` passes.
pub fn base_url(name: &str, url: &str) -> anyhow::Result<String> {
    let url = url.trim_end_matches('/');
    let rest = url.strip_prefix(name).unwrap_or(url);
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    match rest.split_once("://") {
        Some(("", host)) if rest.len() < url.len() => Ok(format!("http://{host}")),
        Some(("s", host)) if rest.len() < url.len() => Ok(format!("https://{host}")),
        Some(("http" | "https", _)) => Ok(rest.to_string()),
        _ => bail!("'url' must start with {name}://, {name}+https://, http:// or https://"),
    }
}

/// A collection or index name as one path segment.
pub fn segment<'a>(field: &str, name: &'a str) -> anyhow::Result<&'a str> {
    let reserved = ['/', '?', '#', '&', '%', '{', '}', ' '];
    if name.is_empty() || name == "." || name == ".." || name.contains(reserved) {
        bail!("'{field}' must be one name without '/', '?', '#', '&', '%', braces or spaces, got '{name}'");
    }
    Ok(name)
}

fn default_id_field() -> String {
    "id".to_string()
}

/// The fields every preset shares, as schema properties.
fn common_properties(example: &str) -> serde_json::Map<String, Value> {
    let properties = json!({
        "url": {
            "type": "string",
            "description": format!("Address of the server, e.g. `{example}`."),
            "x-mqb-uri": "origin"
        },
        "api_key": {
            "type": "string",
            "format": "password",
            "description": "API key sent with every request."
        },
        "operation": {
            "type": "string",
            "description": "Template for a message's operation, e.g. `${metadata:postgres.operation}`."
        },
        "compression": {
            "type": "string",
            "enum": ["none", "gzip", "zstd", "lz4"],
            "default": "none",
            "description": "Compression of request bodies."
        },
        "request_timeout_ms": {
            "type": "integer",
            "description": "Request timeout in milliseconds. Unset = 300 s without a response byte; `0` = none."
        },
        "tls": {
            "type": ["object", "string"],
            "description": "TLS options as on `http_bulk` (`ca_file`, `cert_file`, `key_file`, `required`); JSON in a URI."
        }
    });
    match properties {
        Value::Object(map) => map,
        _ => unreachable!("a JSON object literal"),
    }
}

/// A nested option, which a URI carries as JSON text.
fn nested(field: &str, value: Option<Value>) -> anyhow::Result<Option<Value>> {
    match value {
        Some(Value::String(text)) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("'{field}' is not valid JSON"))?,
        )),
        value => Ok(value),
    }
}

/// Puts the preset's `tls` options, if any, into the resolved configuration.
fn with_tls(mut resolved: Value, tls: Option<Value>) -> anyhow::Result<Value> {
    if let Some(tls) = nested("tls", tls)? {
        resolved["tls"] = tls;
    }
    Ok(resolved)
}

/// A preset's schema: the shared fields plus `own`; `example` is the URI shown for `url`.
pub fn schema(title: &str, example: &str, own: Value, required: &[&str]) -> Value {
    let mut properties = common_properties(example);
    if let Value::Object(own) = own {
        properties.extend(own);
    }
    json!({
        "title": title,
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn typesense(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: TypesenseConfig = serde_json::from_value(value.clone())?;
    let collection = segment("collection", &config.collection)?;
    let mut headers = serde_json::Map::new();
    if let Some(key) = config.api_key {
        headers.insert("X-TYPESENSE-API-KEY".into(), json!(key));
    }
    let resolved = json!({
        "url": base_url("typesense", &config.url)?,
        "headers": headers,
        "operation": config.operation,
        "compression": config.compression,
        "request_timeout_ms": config.request_timeout_ms,
        "upsert": {
            "path": format!("/collections/{collection}/documents/import?action=upsert"),
            "content_type": "text/plain",
            "result": {"lines": {"success": "/success", "error": "/error"}}
        },
        "delete": {
            "method": "DELETE",
            "path": format!("/collections/{collection}/documents?filter_by=id:[{{ids}}]"),
            "max_ids": 100
        }
    });
    with_tls(resolved, config.tls)
}

fn elasticsearch(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: ElasticsearchConfig = serde_json::from_value(value.clone())?;
    let index = segment("index", &config.index)?;
    let id_field = &config.id_field.clone().unwrap_or_else(default_id_field);
    if id_field.is_empty() || id_field.contains(['}', '"', '\\']) {
        bail!("'id_field' must be a plain field name, got '{id_field}'");
    }
    let mut headers = serde_json::Map::new();
    if let Some(key) = config.api_key {
        headers.insert("Authorization".into(), json!(format!("ApiKey {key}")));
    }
    let path = format!("/{index}/_bulk");
    let update = config.mode == ElasticsearchMode::Update;
    let auth = nested("auth", config.auth)?;
    let verb = if update { "update" } else { "index" };
    let resolved = json!({
        "url": base_url("elasticsearch", &config.url)?,
        "headers": headers,
        "auth": auth,
        "operation": config.operation,
        "compression": config.compression,
        "request_timeout_ms": config.request_timeout_ms,
        "upsert": {
            "path": path,
            "action": format!(r#"{{"{verb}":{{"_id":"${{payload:{id_field}}}"}}}}"#),
            "document": update.then_some(r#"{"doc":{document},"doc_as_upsert":true}"#),
            "result": {"items": {"path": "/items", "error": format!("/{verb}/error/reason")}}
        },
        "delete": {
            "path": path,
            "id_field": id_field,
            "line": r#"{"delete":{"_id":{id}}}"#,
            "result": {"items": {"path": "/items", "error": "/delete/error/reason"}}
        }
    });
    with_tls(resolved, config.tls)
}

/// Options of the `mq-bridge-meilisearch` plugin that this endpoint leaves out.
const MEILISEARCH_PLUGIN_ONLY: [&str; 8] = [
    "settings",
    "method",
    "update_where",
    "update_into",
    "swap_into",
    "create_index",
    "wait_for_task",
    "task_timeout_ms",
];

const MEILISEARCH_REQUEST_BYTES: u64 = 90_000_000;

fn plugin_only(what: &str) -> anyhow::Error {
    anyhow!(
        "{what} needs the mq-bridge-meilisearch plugin: install it and set \
         MQB_PLUGIN_OVERRIDE=meilisearch"
    )
}

fn meilisearch(route: &str, value: &Value) -> anyhow::Result<Value> {
    if let Some(field) = MEILISEARCH_PLUGIN_ONLY
        .iter()
        .find(|field| value.get(**field).is_some())
    {
        return Err(plugin_only(&format!("'{field}'")));
    }
    let config: MeilisearchConfig = serde_json::from_value(value.clone())?;
    let index = config.index.as_deref().unwrap_or(route);
    if index.contains("${") {
        return Err(plugin_only("a template in 'index'"));
    }
    let index = segment("index", index)?;
    let mut upsert = format!("/indexes/{index}/documents");
    if let Some(key) = &config.primary_key {
        upsert = format!("{upsert}?primaryKey={}", segment("primary_key", key)?);
    }
    let mut read = format!("/indexes/{index}/documents?offset={{cursor}}&limit={{limit}}");
    if let Some(fields) = &config.fields {
        if fields.contains(['&', '#', '%', '{', '}', ' ']) {
            bail!("'fields' must be field names separated by commas, got '{fields}'");
        }
        read = format!("{read}&fields={fields}");
    }
    let mut headers = serde_json::Map::new();
    if let Some(key) = config.api_key {
        headers.insert("Authorization".into(), json!(format!("Bearer {key}")));
    }
    let task = json!({"job": {
        "id": "/taskUid",
        "poll": "/tasks/{id}",
        "status": "/status",
        "succeeded": ["succeeded"],
        "failed": ["failed", "canceled"],
        "error": "/error/message"
    }});
    let mut resolved = json!({
        "url": base_url("meilisearch", &config.url)?,
        "headers": headers,
        "operation": config.operation,
        "max_request_bytes": config.max_request_bytes.unwrap_or(MEILISEARCH_REQUEST_BYTES),
        "compression": config.compression,
        "connect_timeout_ms": config.connect_timeout_ms,
        "request_timeout_ms": config.request_timeout_ms,
        "upsert": {"path": upsert, "result": task},
        "delete": {
            "path": format!("/indexes/{index}/documents/delete-batch"),
            "id_field": config.primary_key.unwrap_or_else(default_id_field),
            "result": task
        },
        "read": {
            "path": read,
            "items": "/results",
            "cursor_id": config.cursor_id,
            "checkpoint_store": config.checkpoint_store,
            "polling_interval_ms": config.polling_interval_ms,
            "max_polling_interval_ms": config.max_polling_interval_ms
        }
    });
    if let Some(values) = config.delete_values {
        resolved["delete_values"] = json!(values);
    }
    with_tls(resolved, config.tls)
}

fn qdrant(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: QdrantConfig = serde_json::from_value(value.clone())?;
    let points = format!(
        "/collections/{}/points",
        segment("collection", &config.collection)?
    );
    let wait = config.wait.unwrap_or(true);
    let mut headers = serde_json::Map::new();
    if let Some(key) = config.api_key {
        headers.insert("api-key".into(), json!(key));
    }
    let resolved = json!({
        "url": base_url("qdrant", &config.url)?,
        "headers": headers,
        "operation": config.operation,
        "compression": config.compression,
        "request_timeout_ms": config.request_timeout_ms,
        "upsert": {
            "method": "PUT",
            "path": format!("{points}?wait={wait}"),
            "format": "json_array",
            "envelope": r#"{"points":{documents}}"#
        },
        "delete": {
            "path": format!("{points}/delete?wait={wait}"),
            "envelope": r#"{"points":{ids}}"#
        },
        "read": {
            "path": format!("{points}/scroll"),
            "body": r#"{"limit":{limit},"offset":{cursor},"with_payload":true,"with_vector":true}"#,
            "items": "/result/points",
            "cursor": {"response": "/result/next_page_offset"},
            "cursor_id": config.cursor_id,
            "checkpoint_store": config.checkpoint_store
        }
    });
    with_tls(resolved, config.tls)
}

const OPENAI_URL: &str = "https://api.openai.com/v1";

/// The API's base URL; one without a path gets `/v1`, which every compatible server uses.
fn openai_url(name: &str, url: Option<&str>) -> anyhow::Result<String> {
    let url = url.unwrap_or(OPENAI_URL);
    // `openai://` is the short form of `openai-chat://`.
    let base = base_url(name, url).or_else(|error| match name {
        "openai-chat" => base_url("openai", url),
        _ => Err(error),
    })?;
    let parsed = url::Url::parse(&base).with_context(|| format!("'url' {base} is not a URL"))?;
    Ok(match parsed.path() {
        "" | "/" => format!("{base}/v1"),
        _ => base,
    })
}

fn bearer(api_key: Option<String>) -> serde_json::Map<String, Value> {
    let mut headers = serde_json::Map::new();
    if let Some(key) = api_key {
        headers.insert("Authorization".into(), json!(format!("Bearer {key}")));
    }
    headers
}

fn openai_embeddings(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: OpenAiEmbeddingsConfig = serde_json::from_value(value.clone())?;
    let mut envelope = format!(r#"{{"model":{},"input":{{requests}}"#, json!(config.model));
    if let Some(dimensions) = config.dimensions {
        envelope.push_str(&format!(r#","dimensions":{dimensions}"#));
    }
    envelope.push('}');
    let mut resolved = json!({
        "url": openai_url("openai-embeddings", config.url.as_deref())?,
        "headers": bearer(config.api_key),
        "request_timeout_ms": config.request_timeout_ms,
        "query": {
            "path": "/embeddings",
            "format": "json_array",
            // The text is one JSON string; the template escapes what a message puts in.
            "request": json!(config.input).to_string(),
            "envelope": envelope,
            "responses": "/data",
            "value": "/embedding"
        }
    });
    if let Some(bytes) = config.max_request_bytes {
        resolved["max_request_bytes"] = json!(bytes);
    }
    with_tls(resolved, config.tls)
}

fn openai_chat(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: OpenAiChatConfig = serde_json::from_value(value.clone())?;
    let mut body = match nested("options", config.options)? {
        Some(Value::Object(options)) => options,
        None => serde_json::Map::new(),
        Some(_) => bail!("'options' must be a JSON object"),
    };
    if let Some(field) = ["model", "messages", "stream"]
        .into_iter()
        .find(|field| body.contains_key(*field))
    {
        bail!("'options' cannot set '{field}'");
    }
    if config.json && !body.contains_key("response_format") {
        body.insert("response_format".into(), json!({"type": "json_object"}));
    }
    body.insert("model".into(), json!(config.model));
    let system = config.system.as_deref();
    let query = match config.messages_per_request {
        Some(0) => bail!("'messages_per_request' must be at least 1"),
        Some(count) => bundled_chat(body, system, &config.prompt, config.json, count)?,
        None => {
            let mut messages = Vec::new();
            if let Some(system) = system {
                messages.push(json!({"role": "system", "content": system}));
            }
            messages.push(json!({"role": "user", "content": config.prompt}));
            body.insert("messages".into(), json!(messages));
            json!({
                "path": "/chat/completions",
                "format": "json_array",
                // Serializing escapes the literal text; the template escapes what a message puts in.
                "request": Value::Object(body).to_string(),
                "per_message": true,
                "value": "/choices/0/message/content",
                "error": "/error/message",
                "parse_value": config.json
            })
        }
    };
    let mut resolved = json!({
        "url": openai_url("openai-chat", config.url.as_deref())?,
        "headers": bearer(config.api_key),
        "request_timeout_ms": config.request_timeout_ms,
        "query": query
    });
    resolved["query"]["concurrency"] = json!(config.concurrency.unwrap_or(8));
    if let Some(bytes) = config.max_request_bytes {
        resolved["max_request_bytes"] = json!(bytes);
    }
    with_tls(resolved, config.tls)
}

/// What a bundled request adds to the system message.
const BUNDLED: &str = "The user message holds numbered inputs; each starts with its number and \
    a colon. Treat every input on its own. Answer with one JSON object \
    {\"answers\": [{\"n\": 1, \"answer\": ...}, ...]} that has exactly one entry per input; \
    \"n\" is the number of the input.";

/// The query that sends `count` messages as numbered inputs of one completion.
fn bundled_chat(
    mut body: serde_json::Map<String, Value>,
    system: Option<&str>,
    prompt: &str,
    json: bool,
    count: usize,
) -> anyhow::Result<Value> {
    const REQUESTS: &str = "{requests}";
    // The system message is sent once per request, so it has no message to read from.
    if system.is_some_and(|system| system.contains("${") || system.contains(REQUESTS)) {
        bail!("'system' cannot hold '${{…}}' or '{REQUESTS}' with 'messages_per_request'");
    }
    if prompt.contains("{index}") {
        bail!("'prompt' cannot hold '{{index}}' with 'messages_per_request'");
    }
    let entry = match json {
        true => "Each \"answer\" is the JSON object that answers its input.",
        false => "Each \"answer\" is a string, the answer to its input.",
    };
    let system = [system.unwrap_or_default(), BUNDLED, entry].join("\n");
    // The answers arrive as one JSON object whatever a single answer is.
    body.entry("response_format")
        .or_insert_with(|| json!({"type": "json_object"}));
    body.insert(
        "messages".into(),
        json!([
            {"role": "system", "content": system.trim_start()},
            {"role": "user", "content": REQUESTS}
        ]),
    );
    let envelope = Value::Object(body).to_string();
    Ok(json!({
        "path": "/chat/completions",
        "format": "json_array",
        "request": json!({"type": "text", "text": format!("{{index}}: {prompt}")}).to_string(),
        "envelope": envelope.replace(&format!("\"{REQUESTS}\""), REQUESTS),
        "max_messages": count,
        "responses_text": "/choices/0/message/content",
        "responses": "/answers",
        "index": "/n",
        "value": "/answer",
        "stop_reason": "/choices/0/finish_reason",
        "split_on_reject": true
    }))
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use super::*;

    #[test]
    fn a_typed_config_is_the_endpoint_of_its_name() {
        use crate::models::Endpoint;
        let endpoint: Endpoint = QdrantConfig {
            url: "http://localhost:6333".into(),
            collection: "docs".into(),
            ..Default::default()
        }
        .into();
        assert!(matches!(endpoint.endpoint_type, EndpointType::Qdrant(_)));
        assert_eq!(endpoint.endpoint_type.name(), "qdrant");
        // A route file names it by its key and gets the same variant back.
        let written = serde_json::to_value(&endpoint).expect("serializes");
        assert_eq!(written["qdrant"]["collection"], "docs");
        let parsed: Endpoint = serde_json::from_value(written).expect("parses");
        assert!(matches!(parsed.endpoint_type, EndpointType::Qdrant(_)));
        // No registration: a route finds the built-in endpoint by its name.
        assert!(builtin("qdrant").is_some() && builtin("acme").is_none());
        // A config of one's own type stays a custom endpoint, without its unset fields.
        let EndpointType::Custom { name, config } =
            named_endpoint("acme", &json!({"a": 1, "b": null}))
        else {
            panic!("a custom endpoint");
        };
        assert_eq!((name.as_str(), config), ("acme", json!({"a": 1})));
    }
    use crate::plugin::test_support::StubHttpServer;
    use crate::CanonicalMessage;

    fn preset(name: &str) -> Preset {
        PRESETS
            .into_iter()
            .find(|preset| preset.name == name)
            .expect("preset")
    }

    fn calls(server: &StubHttpServer) -> Vec<(String, String, String)> {
        server
            .requests()
            .into_iter()
            .map(|r| (r.method, r.target, String::from_utf8(r.body).unwrap()))
            .collect()
    }

    fn delete(payload: &str) -> CanonicalMessage {
        CanonicalMessage::from(payload).with_metadata_kv("op", "delete")
    }

    #[test]
    fn every_preset_passes_tls_options_on() {
        for (name, own) in [
            ("typesense", json!({"collection": "books"})),
            ("elasticsearch", json!({"index": "books"})),
            ("meilisearch", json!({"index": "books"})),
            ("qdrant", json!({"collection": "books"})),
            ("openai_chat", json!({"model": "m", "prompt": "p"})),
            ("openai_embeddings", json!({"model": "m", "input": "i"})),
        ] {
            // A scheme cannot hold `_`, so the URI of `openai_embeddings` spells it with `-`.
            let scheme = name.replace('_', "-");
            for tls in [
                json!({"required": true, "ca_file": "/etc/ca.pem"}),
                json!(r#"{"required": true, "ca_file": "/etc/ca.pem"}"#),
            ] {
                let mut config = own.clone();
                config["url"] = json!(format!("{scheme}+https://search.example"));
                config["tls"] = tls;
                let resolved = resolve(name, &config).unwrap_or_else(|e| panic!("{name}: {e:#}"));
                assert!(resolved.tls.required, "{name}");
                assert_eq!(
                    resolved.tls.ca_file.as_deref(),
                    Some("/etc/ca.pem"),
                    "{name}"
                );
            }
            let mut plain = own.clone();
            plain["url"] = json!(format!("{scheme}://localhost"));
            assert!(!resolve(name, &plain).unwrap().tls.required, "{name}");
        }
    }

    #[tokio::test]
    async fn typesense_imports_by_line_and_deletes_by_filter() {
        let server = StubHttpServer::start(|_| (200, "{\"success\":true}\n".to_string()))
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "collection": "books", "api_key": "k",
            "operation": "${metadata:op}"
        });
        let publisher = preset("typesense")
            .create_publisher("route", &config)
            .await
            .unwrap();
        publisher
            .send_batch(vec![r#"{"id":"1"}"#.into(), delete(r#"{"id":"2"}"#)])
            .await
            .unwrap();

        assert_eq!(
            server.requests()[0].header("x-typesense-api-key"),
            Some("k")
        );
        assert_eq!(
            calls(&server),
            vec![
                (
                    "POST".to_string(),
                    "/collections/books/documents/import?action=upsert".to_string(),
                    "{\"id\":\"1\"}\n".to_string()
                ),
                (
                    "DELETE".to_string(),
                    "/collections/books/documents?filter_by=id:[2]".to_string(),
                    String::new()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn elasticsearch_names_the_id_in_bulk_actions() {
        let server = StubHttpServer::start(|_| (200, r#"{"items":[{"index":{}}]}"#.to_string()))
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "index": "books", "api_key": "k", "id_field": "isbn",
            "operation": "${metadata:op}"
        });
        let publisher = preset("elasticsearch")
            .create_publisher("route", &config)
            .await
            .unwrap();
        publisher
            .send_batch(vec![r#"{"isbn":"a"}"#.into(), delete(r#"{"isbn":"b"}"#)])
            .await
            .unwrap();

        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("ApiKey k")
        );
        assert_eq!(
            calls(&server),
            vec![
                (
                    "POST".to_string(),
                    "/books/_bulk".to_string(),
                    "{\"index\":{\"_id\":\"a\"}}\n{\"isbn\":\"a\"}\n".to_string()
                ),
                (
                    "POST".to_string(),
                    "/books/_bulk".to_string(),
                    "{\"delete\":{\"_id\":\"b\"}}\n".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn elasticsearch_update_mode_merges_and_reports_failures_per_document() {
        let answer = r#"{"errors":true,"items":[{"update":{"status":200}},{"update":{"status":400,"error":{"reason":"bad field"}}}]}"#;
        let server = StubHttpServer::start(move |_| (200, answer.to_string()))
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "index": "books", "id_field": "isbn", "mode": "update"
        });
        let publisher = preset("elasticsearch")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let sent = publisher
            .send_batch(vec![
                r#"{"isbn":"a","stock":3}"#.into(),
                r#"{"isbn":"b","stock":"x"}"#.into(),
            ])
            .await
            .unwrap();

        assert_eq!(
            calls(&server),
            vec![(
                "POST".to_string(),
                "/books/_bulk".to_string(),
                concat!(
                    "{\"update\":{\"_id\":\"a\"}}\n",
                    "{\"doc\":{\"isbn\":\"a\",\"stock\":3},\"doc_as_upsert\":true}\n",
                    "{\"update\":{\"_id\":\"b\"}}\n",
                    "{\"doc\":{\"isbn\":\"b\",\"stock\":\"x\"},\"doc_as_upsert\":true}\n",
                )
                .to_string()
            )]
        );
        let crate::outcomes::SentBatch::Partial { failed, .. } = sent else {
            panic!("the second document was refused");
        };
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0.payload.as_ref(), br#"{"isbn":"b","stock":"x"}"#);
        assert!(failed[0].1.to_string().contains("bad field"));
    }

    #[test]
    fn the_scheme_picks_http_or_https_and_a_bad_config_is_named() {
        let url = |value: Value| typesense("route", &value).map(|config| config["url"].clone());
        let config = |url: &str| json!({"url": url, "collection": "books"});
        assert_eq!(url(config("typesense://h:8108/")).unwrap(), "http://h:8108");
        assert_eq!(url(config("typesense+https://h")).unwrap(), "https://h");
        assert!(url(config("://h")).is_err());
        assert_eq!(url(config("https://h")).unwrap(), "https://h");
        assert!(url(config("ftp://h")).is_err());

        for (name, bad) in [
            ("typesense", json!({"url": "http://h", "collection": "a/b"})),
            (
                "typesense",
                json!({"url": "http://h", "collection": "a", "index": "b"}),
            ),
            (
                "elasticsearch",
                json!({"url": "http://h", "index": "a", "id_field": "x}"}),
            ),
            ("elasticsearch", json!({"url": "http://h"})),
            ("elasticsearch", json!({"url": "http://h", "index": ".."})),
            (
                "elasticsearch",
                json!({"url": "http://h", "index": "a", "auth": "{oauth2"}),
            ),
            (
                "elasticsearch",
                json!({"url": "http://h", "index": "a", "mode": "merge"}),
            ),
            (
                "meilisearch",
                json!({"url": "http://h", "index": "a", "primary_key": "id&x=1"}),
            ),
            ("qdrant", json!({"url": "http://h", "collection": "a/b"})),
            ("openai_chat", json!({"prompt": "p"})),
            (
                "openai_chat",
                json!({"model": "m", "prompt": "p", "options": {"stream": true}}),
            ),
            (
                "openai_chat",
                json!({"model": "m", "prompt": "p", "options": "[1]"}),
            ),
            (
                "openai_chat",
                json!({"model": "m", "prompt": "p", "concurrency": 0}),
            ),
            ("openai_embeddings", json!({"model": "m"})),
            (
                "openai_embeddings",
                json!({"model": "m", "input": "i", "url": "ftp://h"}),
            ),
        ] {
            let refused = preset(name)
                .config("route", &bad)
                .and_then(|config| HttpBulkPublisher::new(&config).map(|_| ()));
            let error = refused.expect_err("refused");
            let named = error.is::<InvalidConfig>() || error.to_string().contains("http_bulk");
            assert!(named, "{name}: {error:#}");
        }
    }

    #[tokio::test]
    async fn a_preset_of_your_own_registers_as_a_custom_endpoint() {
        fn acme_schema() -> Value {
            let own = json!({"table": {"type": "string", "x-mqb-uri": "path"}});
            schema("Acme", "acme://localhost:1", own, &["url", "table"])
        }
        fn acme(_route: &str, value: &Value) -> anyhow::Result<Value> {
            let url = value["url"].as_str().context("'url' is required")?;
            let table = segment("table", value["table"].as_str().unwrap_or_default())?;
            Ok(json!({
                "url": base_url("acme", url)?,
                "upsert": {"path": format!("/{table}/_bulk")}
            }))
        }
        register_preset_with("acme", acme_schema, acme).expect("registered");
        let factory = crate::extensions::get_endpoint_factory("acme").expect("factory");
        assert_eq!(factory.config_schema().unwrap()["title"], "Acme");
        let good = json!({"url": "acme://localhost:1", "table": "a"});
        factory
            .create_publisher("route", &good)
            .await
            .expect("publisher");
        let bad = json!({"url": "acme://localhost:1", "table": "a/b"});
        let error = factory
            .create_publisher("route", &bad)
            .await
            .err()
            .expect("refused");
        assert!(error.is::<InvalidConfig>(), "{error:#}");
        crate::extensions::unregister_endpoint_factory("acme");
    }

    #[tokio::test]
    async fn every_preset_declares_a_uri_and_only_two_are_inputs() {
        for name in preset_names() {
            let preset = preset(name);
            #[cfg(feature = "schema")]
            {
                let schema = preset.config_schema().expect("schema");
                let properties = &schema["properties"];
                assert!(properties["url"]["x-mqb-uri"].is_string(), "{name}");
                assert_eq!(properties["api_key"]["format"], "password", "{name}");
                assert_eq!(schema["additionalProperties"], false, "{name}");
            }
            let config = match name {
                "typesense" | "qdrant" => json!({"url": "http://localhost:1", "collection": "a"}),
                "openai_chat" => json!({"model": "m", "prompt": "p"}),
                "openai_embeddings" => json!({"model": "m", "input": "i"}),
                _ => json!({"url": "http://localhost:1", "index": "a"}),
            };
            let consumer = preset.create_consumer("route", &config).await;
            match name {
                "meilisearch" | "qdrant" => assert!(consumer.is_ok(), "{name}"),
                _ => assert!(consumer.err().expect("output only").is::<InvalidConfig>()),
            }
        }
    }

    #[tokio::test]
    async fn meilisearch_waits_for_the_task_and_reads_by_offset() {
        let server = StubHttpServer::start(|request| {
            let body = match request.target.as_str() {
                "/tasks/7" => r#"{"status":"succeeded"}"#,
                target if target.contains("offset=") => r#"{"results":[{"id":1}]}"#,
                _ => r#"{"taskUid":7}"#,
            };
            (200, body.to_string())
        })
        .await
        .expect("stub server");
        let config = json!({
            "url": server.url(), "api_key": "k", "primary_key": "isbn",
            "operation": "${metadata:op}", "fields": "isbn,title"
        });
        let preset = preset("meilisearch");
        let publisher = preset.create_publisher("books", &config).await.unwrap();
        publisher
            .send_batch(vec![r#"{"isbn":"a"}"#.into(), delete(r#"{"isbn":"b"}"#)])
            .await
            .unwrap();
        let mut consumer = preset.create_consumer("books", &config).await.unwrap();
        let read = consumer.receive_batch(10).await.unwrap();
        assert_eq!(read.messages.len(), 1);

        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer k")
        );
        let calls = calls(&server);
        let targets: Vec<&str> = calls.iter().map(|call| call.1.as_str()).collect();
        assert_eq!(
            targets,
            [
                "/indexes/books/documents?primaryKey=isbn",
                "/tasks/7",
                "/indexes/books/documents/delete-batch",
                "/tasks/7",
                "/indexes/books/documents?offset=0&limit=10&fields=isbn,title",
            ]
        );
        assert_eq!(calls[2].2, r#"["b"]"#);
    }

    #[tokio::test]
    async fn qdrant_puts_points_deletes_by_id_and_scrolls() {
        let server = StubHttpServer::start(|request| {
            let body = match request.target.as_str() {
                "/collections/books/points/scroll" => {
                    r#"{"result":{"points":[{"id":1,"vector":[0.1]}],"next_page_offset":null}}"#
                }
                _ => r#"{"result":{"status":"completed"},"status":"ok"}"#,
            };
            (200, body.to_string())
        })
        .await
        .expect("stub server");
        let config = json!({
            "url": server.url(), "collection": "books", "api_key": "k",
            "operation": "${metadata:op}"
        });
        let preset = preset("qdrant");
        let publisher = preset.create_publisher("route", &config).await.unwrap();
        let point = r#"{"id":1,"vector":[0.1],"payload":{"title":"Dune"}}"#;
        publisher
            .send_batch(vec![point.into(), delete(r#"{"id":2}"#)])
            .await
            .unwrap();
        let mut consumer = preset.create_consumer("route", &config).await.unwrap();
        let read = consumer.receive_batch(10).await.unwrap();
        assert_eq!(read.messages.len(), 1);

        assert_eq!(server.requests()[0].header("api-key"), Some("k"));
        let scroll = r#"{"limit":10,"offset":null,"with_payload":true,"with_vector":true}"#;
        assert_eq!(
            calls(&server),
            vec![
                (
                    "PUT".to_string(),
                    "/collections/books/points?wait=true".to_string(),
                    format!(r#"{{"points":[{point}]}}"#)
                ),
                (
                    "POST".to_string(),
                    "/collections/books/points/delete?wait=true".to_string(),
                    r#"{"points":[2]}"#.to_string()
                ),
                (
                    "POST".to_string(),
                    "/collections/books/points/scroll".to_string(),
                    scroll.to_string()
                ),
            ]
        );

        let config = json!({"url": "qdrant://h:6333", "collection": "books", "wait": false});
        let resolved = resolve("qdrant", &config).unwrap();
        assert_eq!(resolved.url, "http://h:6333");
        assert_eq!(
            resolved.upsert.unwrap().path,
            "/collections/books/points?wait=false"
        );
    }

    #[test]
    fn an_openai_url_without_a_path_gets_v1() {
        let url = |name: &str, url: Option<&str>| openai_url(name, url).unwrap();
        assert_eq!(url("openai-chat", None), "https://api.openai.com/v1");
        assert_eq!(
            url("openai-chat", Some("openai-chat://localhost:11434")),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            url("openai-chat", Some("openai-chat+https://h/api/v3/")),
            "https://h/api/v3"
        );
        assert_eq!(
            url("openai-embeddings", Some("openai-embeddings://h:1/")),
            "http://h:1/v1"
        );
        assert_eq!(url("openai-chat", Some("http://h/v1")), "http://h/v1");
        assert_eq!(url("openai-chat", Some("openai://h:1")), "http://h:1/v1");
        assert!(openai_url("openai-embeddings", Some("openai://h:1")).is_err());
    }

    #[tokio::test]
    async fn openai_embeddings_asks_once_for_a_batch() {
        let answer =
            r#"{"data":[{"index":0,"embedding":[0.1,0.2]},{"index":1,"embedding":[0.3,0.4]}]}"#;
        let server = StubHttpServer::start(move |_| (200, answer.to_string()))
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "api_key": "k", "model": "all-minilm",
            "input": "${payload:title}: ${payload:body}", "dimensions": 2
        });
        let publisher = preset("openai_embeddings")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let rows: Vec<CanonicalMessage> = vec![
            r#"{"title":"Dune","body":"a \"desert\"\nplanet"}"#.into(),
            r#"{"title":"Emma","body":"a village"}"#.into(),
        ];
        let vectors = publisher.lookup_batch(&rows).await.unwrap().unwrap();

        assert_eq!(
            vectors,
            vec![Some(json!([0.1, 0.2])), Some(json!([0.3, 0.4]))]
        );
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer k")
        );
        let body = concat!(
            r#"{"model":"all-minilm","input":["#,
            r#""Dune: a \"desert\"\nplanet","Emma: a village"],"dimensions":2}"#
        );
        assert_eq!(
            calls(&server),
            vec![(
                "POST".to_string(),
                "/v1/embeddings".to_string(),
                body.to_string()
            )]
        );
    }

    #[tokio::test]
    async fn openai_chat_asks_per_message_and_a_busy_answer_fails_only_its_message() {
        let server = StubHttpServer::start(|request| {
            let body = String::from_utf8_lossy(&request.body);
            if body.contains("busy") {
                return (429, r#"{"error":{"message":"slow down"}}"#.to_string());
            }
            let content = if body.contains("json_object") {
                r#"{\"label\":\"spam\"}"#
            } else {
                "a summary"
            };
            let answer = format!(r#"{{"choices":[{{"message":{{"content":"{content}"}}}}]}}"#);
            (200, answer)
        })
        .await
        .expect("stub server");
        let config = json!({
            "url": server.url(), "api_key": "k", "model": "m",
            "system": "Answer in \"one\" line.", "prompt": "Summarise: ${payload:body}",
            "options": {"temperature": 0}
        });
        let publisher = preset("openai_chat")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let rows: Vec<CanonicalMessage> = vec![
            r#"{"body":"a \"quoted\" text"}"#.into(),
            r#"{"body":"busy"}"#.into(),
        ];
        // The lookup middleware asks one by one, with its own concurrency.
        assert!(publisher.lookup_batch(&rows).await.is_none());
        let sent = publisher.send_batch(rows).await.unwrap();

        let crate::outcomes::SentBatch::Partial { responses, failed } = sent else {
            panic!("a query answers with responses");
        };
        let responses = responses.unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].payload.as_ref(), br#""a summary""#);
        assert_eq!(failed.len(), 1);
        assert!(
            matches!(failed[0].1, crate::errors::PublisherError::Retryable(_)),
            "{}",
            failed[0].1
        );
        let first = &calls(&server)[0];
        assert_eq!(first.1, "/v1/chat/completions");
        // Compared as JSON: the field order depends on serde_json's `preserve_order`.
        let body: Value = serde_json::from_str(&first.2).unwrap();
        let expected = json!({
            "model": "m",
            "temperature": 0,
            "messages": [
                {"role": "system", "content": "Answer in \"one\" line."},
                {"role": "user", "content": "Summarise: a \"quoted\" text"}
            ]
        });
        assert_eq!(body, expected);

        let mut config = config;
        config["json"] = json!(true);
        let publisher = preset("openai_chat")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let sent = publisher.send(r#"{"body":"x"}"#.into()).await.unwrap();
        let crate::outcomes::Sent::Response(answer) = sent else {
            panic!("a query answers with a response");
        };
        assert_eq!(answer.payload.as_ref(), br#"{"label":"spam"}"#);
    }

    /// A model that answers each numbered input with its text in capitals, last input first.
    /// Words in an input make it misbehave the way a real one does.
    fn bundling_model(request: &crate::plugin::test_support::StubRequest) -> (u16, String) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let parts = body["messages"][1]["content"].as_array().unwrap();
        let texts: Vec<&str> = parts.iter().map(|p| p["text"].as_str().unwrap()).collect();
        let has = |word: &str| texts.iter().any(|text| text.contains(word));
        if has("busy") {
            return (429, r#"{"error":{"message":"slow down"}}"#.to_string());
        }
        if has("huge") {
            return (400, r#"{"error":{"message":"context length"}}"#.to_string());
        }
        let mut answers: Vec<Value> = texts
            .iter()
            .map(|text| {
                let (number, text) = text.split_once(": Text: ").unwrap();
                json!({"n": number.parse::<u64>().unwrap(), "answer": text.to_uppercase()})
            })
            .rev()
            .collect();
        let many = texts.len() > 1;
        if has("lost") && many {
            answers.pop();
        }
        if has("twice") && many {
            answers[0]["n"] = answers[1]["n"].clone();
        }
        if has("stranger") && many {
            answers[0]["n"] = json!(99);
        }
        let mut choice = json!({
            "message": {"content": json!({"answers": answers}).to_string()},
            "finish_reason": "stop"
        });
        if has("long") {
            choice = json!({"message": {"content": "{\"answers\":[{\"n\":1,\"ans"},
                "finish_reason": "length"});
        }
        if has("refuse") {
            choice["message"] = json!({"content": null, "refusal": "I cannot help with that."});
        }
        (200, json!({"choices": [choice]}).to_string())
    }

    #[tokio::test]
    async fn openai_chat_bundles_messages_and_matches_the_answers_by_number() {
        let server = StubHttpServer::start(bundling_model)
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "model": "m", "system": "Shout.",
            "prompt": "Text: ${payload:body}", "messages_per_request": 2, "concurrency": 1
        });
        let publisher = preset("openai_chat")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let answers = publisher.lookup_batch(&rows(&["a", "b \"c\"", "d"])).await;
        // The model answers the last input first; the number puts each answer in its place.
        assert_eq!(
            answers.unwrap().unwrap(),
            vec![Some(json!("A")), Some(json!("B \"C\"")), Some(json!("D"))]
        );
        let sent = calls(&server);
        assert_eq!(sent.len(), 2);
        let body: Value = serde_json::from_str(&sent[0].2).unwrap();
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("Shout.\nThe user message holds numbered inputs"));
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"type": "text", "text": "1: Text: a"},
                {"type": "text", "text": "2: Text: b \"c\""}
            ])
        );
        // The third message opens the next request and is its first input.
        let body: Value = serde_json::from_str(&sent[1].2).unwrap();
        let expected = json!([{"type": "text", "text": "1: Text: d"}]);
        assert_eq!(body["messages"][1]["content"], expected);

        for (extra, reason) in [
            (json!({"messages_per_request": 0}), "at least 1"),
            (json!({"system": "For ${payload:lang}"}), "'system' cannot"),
            (
                json!({"prompt": "{index} ${payload:body}"}),
                "'prompt' cannot",
            ),
        ] {
            let mut bad = config.clone();
            bad.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let error = preset("openai_chat").config("route", &bad).unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{error:#}");
        }
    }

    fn rows(bodies: &[&str]) -> Vec<CanonicalMessage> {
        let row = |body: &&str| json!({"body": body}).to_string().into();
        bodies.iter().map(row).collect()
    }

    #[tokio::test]
    async fn openai_chat_halves_a_bundle_until_the_bad_message_stands_alone() {
        // (inputs, answers, reason of the one failure, requests sent)
        for (inputs, answered, reason, requests) in [
            // Too few entries, a number twice, a number nobody has: nothing can be matched.
            (vec!["a", "lost", "c", "d"], 4, None, 5),
            (vec!["a", "twice", "c", "d"], 4, None, 5),
            (vec!["a", "stranger", "c", "d"], 4, None, 5),
            // Refused for its size, cut off, or declined: only that message fails.
            (vec!["a", "huge", "c", "d"], 3, Some("context length"), 5),
            (
                vec!["a", "long", "c", "d"],
                3,
                Some("ended with 'length'"),
                5,
            ),
            (
                vec!["a", "b", "c", "refuse"],
                3,
                Some("no array of 1 entries"),
                5,
            ),
        ] {
            let server = StubHttpServer::start(bundling_model)
                .await
                .expect("stub server");
            let config = json!({
                "url": server.url(), "model": "m",
                "prompt": "Text: ${payload:body}", "messages_per_request": 4
            });
            let publisher = preset("openai_chat")
                .create_publisher("route", &config)
                .await
                .unwrap();
            let sent = publisher.send_batch(rows(&inputs)).await.unwrap();
            let crate::outcomes::SentBatch::Partial { responses, failed } = sent else {
                panic!("a query answers with responses");
            };
            let responses = responses.unwrap();
            assert_eq!(responses.len(), answered, "{inputs:?}");
            assert_eq!(responses[0].payload.as_ref(), br#""A""#, "{inputs:?}");
            assert_eq!(calls(&server).len(), requests, "{inputs:?}");
            match reason {
                Some(reason) => {
                    assert_eq!(failed.len(), 1, "{inputs:?}");
                    let error = &failed[0].1;
                    assert!(error.to_string().contains(reason), "{error}");
                    let permanent = matches!(error, crate::errors::PublisherError::NonRetryable(_));
                    assert!(permanent, "{error}");
                }
                None => assert!(failed.is_empty(), "{inputs:?}"),
            }
        }
    }

    #[tokio::test]
    async fn openai_chat_fails_a_busy_bundle_as_retryable_without_splitting_it() {
        let server = StubHttpServer::start(bundling_model)
            .await
            .expect("stub server");
        let config = json!({
            "url": server.url(), "model": "m",
            "prompt": "Text: ${payload:body}", "messages_per_request": 2, "concurrency": 1
        });
        let publisher = preset("openai_chat")
            .create_publisher("route", &config)
            .await
            .unwrap();
        let error = publisher
            .send_batch(rows(&["busy", "b", "c", "d"]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, crate::errors::PublisherError::Retryable(_)),
            "{error}"
        );
        // Neither halves nor the bundle after it.
        assert_eq!(calls(&server).len(), 1);
    }

    #[test]
    fn meilisearch_names_what_only_the_plugin_does() {
        for bad in [
            json!({"url": "http://h", "settings": {}}),
            json!({"url": "http://h", "swap_into": "live"}),
            json!({"url": "http://h", "index": "${metadata:postgres.table}"}),
        ] {
            let error = preset("meilisearch")
                .config("route", &bad)
                .expect_err("refused");
            assert!(
                format!("{error:#}").contains("MQB_PLUGIN_OVERRIDE"),
                "{error:#}"
            );
        }
    }
}
