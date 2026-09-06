//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Hand-written `Deserialize`, `Serialize`, `JsonSchema` and `Debug` impls, plus the
//! `deserialize_*` and `*_schema_transform` helpers named from attributes in the parent
//! module. Everything here exists because a derive cannot express the shape we accept.

use super::*;

/// Every name `EndpointType` deserializes from, including its serde aliases, but not
/// `custom` (which is the fallback itself).
///
/// A name listed here keeps the original field error when its config fails to parse;
/// anything else is treated as a custom endpoint. Omitting a built-in would turn a typo
/// in, say, a `clickhouse` block into an opaque unknown-custom-endpoint failure instead
/// of naming the offending field.
pub(crate) fn is_known_endpoint_name(name: &str) -> bool {
    matches!(
        name,
        "aws"
            | "kafka"
            | "nats"
            | "file"
            | "dir_spool"
            | "spool"
            | "dirspool"
            | "object_store"
            | "objectstore"
            | "s3"
            | "static"
            | "memory"
            | "sled"
            | "amqp"
            | "mongodb"
            | "mqtt"
            | "http"
            | "websocket"
            | "ibmmq"
            | "zeromq"
            | "redis_streams"
            | "redis"
            | "grpc"
            | "fanout"
            | "stream_buffer"
            | "ref"
            | "switch"
            | "response"
            | "reader"
            | "request"
            | "null"
            | "sqlx"
            | "clickhouse"
            | "click_house"
            | "postgres_cdc"
            | "postgres-cdc"
    )
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("middlewares", &self.middlewares)
            .field("endpoint_type", &self.endpoint_type)
            .field(
                "handler",
                &if self.handler.is_some() {
                    "Some(<Handler>)"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EndpointVisitor;

        impl<'de> Visitor<'de> for EndpointVisitor {
            type Value = Endpoint;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map representing an endpoint, the string \"null\", or null")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Endpoint::new(EndpointType::Null))
            }

            /// Unit variants of `EndpointType` serialize as a bare string (and are advertised
            /// that way in the JSON schema). `Null` is currently the only one.
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == "null" {
                    Ok(Endpoint::new(EndpointType::Null))
                } else {
                    Err(serde::de::Error::unknown_variant(value, &["null"]))
                }
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&value)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                // Buffer the map into a temporary serde_json::Map.
                // This allows us to separate the `middlewares` field from the rest.
                let mut temp_map = serde_json::Map::new();
                let mut middlewares_val = None;

                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if key == "middlewares" {
                        middlewares_val = Some(value);
                    } else {
                        temp_map.insert(key, value);
                    }
                }

                // Deserialize the rest of the map into the flattened EndpointType.
                let temp_val = serde_json::Value::Object(temp_map);
                let endpoint_type: EndpointType = match serde_json::from_value(temp_val.clone()) {
                    Ok(et) => et,
                    Err(original_err) => {
                        if let serde_json::Value::Object(map) = &temp_val {
                            if map.len() == 1 {
                                let (name, config) = map.iter().next().unwrap();
                                if is_known_endpoint_name(name) {
                                    return Err(serde::de::Error::custom(original_err));
                                }
                                trace!("Falling back to Custom endpoint for key: {}", name);
                                EndpointType::Custom {
                                    name: name.clone(),
                                    config: config.clone(),
                                }
                            } else if map.is_empty() {
                                EndpointType::Null
                            } else {
                                return Err(serde::de::Error::custom(
                                    "Invalid endpoint configuration: multiple keys found or unknown endpoint type",
                                ));
                            }
                        } else {
                            return Err(serde::de::Error::custom("Invalid endpoint configuration"));
                        }
                    }
                };

                // Deserialize the extracted middlewares value using the existing helper logic.
                let middlewares = match middlewares_val {
                    Some(val) => {
                        deserialize_middlewares_from_value(val).map_err(serde::de::Error::custom)?
                    }
                    None => Vec::new(),
                };

                Ok(Endpoint {
                    middlewares,
                    endpoint_type,
                    handler: None,
                })
            }
        }

        deserializer.deserialize_any(EndpointVisitor)
    }
}

pub(crate) fn is_known_middleware_name(name: &str) -> bool {
    matches!(
        name,
        "id" | "deduplication"
            | "metrics"
            | "dlq"
            | "retry"
            | "random_panic"
            | "delay"
            | "weak_join"
            | "limiter"
            | "buffer"
            | "cookie_jar"
            | "filter"
            | "custom"
    )
}

/// Deserialize middlewares from a generic serde_json::Value.
///
/// This logic was extracted from `deserialize_middlewares_from_map_or_seq` to be reused by the custom `Endpoint` deserializer.
pub(crate) fn deserialize_middlewares_from_value(
    value: serde_json::Value,
) -> anyhow::Result<Vec<Middleware>> {
    let arr = match value {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(map) => {
            // The config crate can produce maps with numeric string keys ("0", "1", ...)
            // from environment variables. We sort by these keys to maintain order.
            let mut middlewares = Vec::with_capacity(map.len());
            for (key, value) in map {
                let index = key.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid middleware configuration: expected numeric keys, found '{key}'"
                    )
                })?;
                middlewares.push((index, value));
            }
            middlewares.sort_by_key(|(index, _)| *index);

            middlewares.into_iter().map(|(_, value)| value).collect()
        }
        _ => return Err(anyhow::anyhow!("Expected an array or object")),
    };

    let mut middlewares = Vec::new();
    for item in arr {
        // Check if it is a map with a single key that matches a known middleware
        let known_name = if let serde_json::Value::Object(map) = &item {
            if map.len() == 1 {
                let (name, _) = map.iter().next().unwrap();
                if is_known_middleware_name(name) {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(name) = known_name {
            match serde_json::from_value::<Middleware>(item.clone()) {
                Ok(m) => middlewares.push(m),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to deserialize known middleware '{}': {}",
                        name,
                        e
                    ))
                }
            }
        } else if let Ok(m) = serde_json::from_value::<Middleware>(item.clone()) {
            middlewares.push(m);
        } else if let serde_json::Value::Object(map) = &item {
            if map.len() == 1 {
                let (name, config) = map.iter().next().unwrap();
                middlewares.push(Middleware::Custom {
                    name: name.clone(),
                    config: config.clone(),
                });
            } else {
                return Err(anyhow::anyhow!(
                    "Invalid middleware configuration: {:?}",
                    item
                ));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Invalid middleware configuration: {:?}",
                item
            ));
        }
    }
    Ok(middlewares)
}

// Hand-written schema: the `Deserialize` impl below accepts either a bare string
// or a map where only `body` is required, so the derived all-fields-required
// object schema would reject valid configs.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for StaticConfig {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StaticConfig".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Configuration for the `static` endpoint. Accepts either a bare string (the response body, JSON-encoded for backward compatibility) or a map where only `body` is required and `raw` / `metadata` are optional.",
            "oneOf": [
                {
                    "type": "string",
                    "description": "The response body, JSON-encoded as a string."
                },
                {
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "string",
                            "description": "The static response body."
                        },
                        "raw": {
                            "type": "boolean",
                            "description": "Send the body verbatim instead of JSON-encoding it as a string.",
                            "default": false
                        },
                        "metadata": {
                            "type": "object",
                            "description": "Extra metadata entries attached to the produced message.",
                            "additionalProperties": { "type": "string" }
                        }
                    },
                    "required": ["body"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

impl Serialize for StaticConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Backward-compatible: when no extra options are set, serialize as a bare
        // string exactly like the historical `Static(String)` so configs written
        // by this version remain readable by older versions.
        if !self.raw && self.metadata.is_empty() {
            return serializer.serialize_str(&self.body);
        }
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("StaticConfig", 3)?;
        state.serialize_field("body", &self.body)?;
        state.serialize_field("raw", &self.raw)?;
        state.serialize_field("metadata", &self.metadata)?;
        state.end()
    }
}

impl From<String> for StaticConfig {
    fn from(body: String) -> Self {
        StaticConfig {
            body,
            raw: false,
            metadata: std::collections::HashMap::new(),
        }
    }
}

impl From<&str> for StaticConfig {
    fn from(body: &str) -> Self {
        StaticConfig::from(body.to_string())
    }
}

impl<'de> Deserialize<'de> for StaticConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Map {
                body: String,
                #[serde(default)]
                raw: bool,
                #[serde(default)]
                metadata: std::collections::HashMap<String, String>,
            },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Str(body) => StaticConfig {
                body,
                raw: false,
                metadata: std::collections::HashMap::new(),
            },
            Repr::Map {
                body,
                raw,
                metadata,
            } => StaticConfig {
                body,
                raw,
                metadata,
            },
        })
    }
}

pub(crate) fn deserialize_null_as_false<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<bool>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(false))
}

impl<'de> Deserialize<'de> for MemoryConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct MemoryConfigSerde {
            #[serde(default)]
            topic: String,
            #[serde(default)]
            url: Option<String>,
            capacity: Option<usize>,
            #[serde(default)]
            request_reply: bool,
            request_timeout_ms: Option<u64>,
            #[serde(default)]
            subscribe_mode: bool,
            #[serde(default)]
            enable_nack: Option<bool>,
        }

        let raw = MemoryConfigSerde::deserialize(deserializer)?;
        if raw.topic.is_empty() && raw.url.as_deref().is_none_or(str::is_empty) {
            return Err(serde::de::Error::custom(
                "MemoryConfig: 'topic' (or 'url' alias) is required.",
            ));
        }
        let topic = if raw.topic.is_empty() {
            raw.url.clone().unwrap_or_default()
        } else {
            raw.topic
        };
        Ok(Self {
            topic,
            url: raw.url,
            capacity: raw.capacity,
            request_reply: raw.request_reply,
            request_timeout_ms: raw.request_timeout_ms,
            subscribe_mode: raw.subscribe_mode,
            enable_nack: raw.enable_nack.unwrap_or(false),
            enable_nack_overridden: raw.enable_nack.is_some(),
        })
    }
}

#[cfg(feature = "schema")]
pub(crate) fn memory_config_schema_transform(schema: &mut schemars::Schema) {
    let Some(schema_obj) = schema.as_object_mut() else {
        return;
    };

    let Some(properties) = schema_obj
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    properties.insert(
        "url".to_string(),
        serde_json::json!({
            "description": "Alias for `topic`. Use either `topic` or `url`.",
            "type": "string",
            "minLength": 1
        }),
    );

    // Mirror the runtime check (see `MemoryConfig::deserialize`): an empty
    // `topic`/`url` is rejected, so the schema must require a non-empty value.
    if let Some(topic) = properties
        .get_mut("topic")
        .and_then(serde_json::Value::as_object_mut)
    {
        topic.insert("minLength".to_string(), serde_json::json!(1));
    }

    schema_obj.insert(
        "anyOf".to_string(),
        serde_json::json!([
            { "required": ["topic"] },
            { "required": ["url"] }
        ]),
    );
}

/// `null` is a unit variant, so schemars emits it as the bare string `"null"` — which can
/// never validate inside `Endpoint`'s object schema. Flattened, it serialises as
/// `{ "null": null }`; rewrite the branch to that object form.
#[cfg(feature = "schema")]
pub(crate) fn endpoint_schema_transform(schema: &mut schemars::Schema) {
    let Some(one_of) = schema
        .as_object_mut()
        .and_then(|schema_obj| schema_obj.get_mut("oneOf"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for branch in one_of.iter_mut() {
        if branch.get("const") == Some(&serde_json::Value::String("null".to_string())) {
            *branch = serde_json::json!({
                "type": "object",
                "format": "structural_endpoint",
                "properties": { "null": { "type": "null" } },
                "required": ["null"]
            });
        }
    }
}

#[cfg(feature = "schema")]
pub(crate) fn route_schema_transform(schema: &mut schemars::Schema) {
    let Some(properties) = schema
        .as_object_mut()
        .and_then(|schema_obj| schema_obj.get_mut("properties"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    // `output: null` (the documented "no output" form) is valid; accept an Endpoint or null.
    // Input stays endpoint-only.
    if let Some(output) = properties
        .get_mut("output")
        .and_then(serde_json::Value::as_object_mut)
    {
        let reference = output.remove("$ref");
        let default = output.remove("default");
        let description = output.remove("description");
        output.clear();
        let mut any_of = Vec::new();
        if let Some(reference) = reference {
            any_of.push(serde_json::json!({ "$ref": reference }));
        }
        any_of.push(serde_json::json!({ "type": "null" }));
        output.insert("anyOf".to_string(), serde_json::Value::Array(any_of));
        if let Some(description) = description {
            output.insert("description".to_string(), description);
        }
        if let Some(default) = default {
            output.insert("default".to_string(), default);
        }
    }

    let Some(allow_fault_injection) = properties
        .get_mut("allow_fault_injection")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    allow_fault_injection.insert("default".to_string(), serde_json::Value::Bool(false));
}

/// `schema` and `schema_file` are mutually exclusive; reject a config setting both.
#[cfg(feature = "schema")]
pub(crate) fn transform_middleware_schema_transform(schema: &mut schemars::Schema) {
    if let Some(schema_obj) = schema.as_object_mut() {
        // Only reject a non-null `schema_file` alongside `schema`; `schema_file: null`
        // is allowed with `schema`, matching the runtime compiler (Option is None).
        schema_obj.insert(
            "not".to_string(),
            serde_json::json!({
                "required": ["schema", "schema_file"],
                "properties": { "schema_file": { "type": "string" } }
            }),
        );
    }
}

pub(crate) fn deserialize_basic_auth<'de, D>(
    deserializer: D,
) -> Result<Option<(String, String)>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(arr) => {
            if arr.len() != 2 {
                return Err(serde::de::Error::custom("basic_auth must have 2 elements"));
            }
            let u = arr[0]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("basic_auth[0] must be string"))?
                .to_string();
            let p = arr[1]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("basic_auth[1] must be string"))?
                .to_string();
            Ok(Some((u, p)))
        }
        serde_json::Value::Object(map) => {
            let u = map
                .get("0")
                .and_then(|v| v.as_str())
                .ok_or_else(|| serde::de::Error::custom("basic_auth map missing '0'"))?
                .to_string();
            let p = map
                .get("1")
                .and_then(|v| v.as_str())
                .ok_or_else(|| serde::de::Error::custom("basic_auth map missing '1'"))?
                .to_string();
            Ok(Some((u, p)))
        }
        _ => Err(serde::de::Error::custom("invalid type for basic_auth")),
    }
}

// schemars ignores serde `alias`, so the MQ-native names accepted at runtime
// (`key_repository`, `key_repository_password`) must be added to the schema by
// hand, otherwise `additionalProperties: false` rejects otherwise-valid configs.
#[cfg(feature = "schema")]
pub(crate) fn ibm_tls_config_schema_transform(schema: &mut schemars::Schema) {
    let Some(properties) = schema
        .as_object_mut()
        .and_then(|schema_obj| schema_obj.get_mut("properties"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    properties.insert(
        "key_repository".to_string(),
        serde_json::json!({
            "description": "MQ-native alias for `cert_file`: the CMS key repository stem \
                (e.g. `/path/to/tls` for `tls.kdb`/`tls.sth`).",
            "type": ["string", "null"]
        }),
    );

    properties.insert(
        "key_repository_password".to_string(),
        serde_json::json!({
            "description": "MQ-native alias for `cert_password`: password unlocking the key \
                repository. Requires an IBM MQ client/server at 9.3.0.0+.",
            "type": ["string", "null"],
            "format": "password"
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middleware_map_rejects_non_numeric_keys() {
        let value = serde_json::json!({
            "0": { "metrics": {} },
            "typo": { "limiter": { "rate_per_second": 1 } }
        });

        let error = deserialize_middlewares_from_value(value).unwrap_err();
        assert!(error.to_string().contains("found 'typo'"));
    }
}
