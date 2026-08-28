//! Generates `dev/docs/reference/<connector>.md` from the JSON Schemas of the
//! endpoint config structs used by `mqb copy --from/--to` (see
//! `endpoint_from_uri` in `src/main.rs`). The JSON Schema is the single
//! source of truth for parameter name/type/default/required/description;
//! this tool only renders it as a table. Examples in `dev/docs/connectors/`
//! are hand-written and not touched by this generator.
//!
//! Run with `cargo run -p mq-bridge-app --example gen_url_docs`.

use mq_bridge_app::mq_bridge::models::{
    AmqpConfig, AwsConfig, ClickHouseConfig, FileConfig, GrpcConfig, HttpConfig, IbmMqConfig,
    KafkaConfig, MongoDbConfig, MqttConfig, NatsConfig, ObjectStoreConfig, PostgresCdcConfig,
    RedisStreamsConfig, SqlxConfig, WebSocketConfig, ZeroMqConfig,
};
use schemars::Schema;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

struct Connector {
    /// File stem under `dev/docs/reference/` and `dev/docs/connectors/`.
    slug: &'static str,
    /// Human-readable name for the page heading.
    title: &'static str,
    /// URI schemes `mqb copy` recognises for this connector.
    schemes: &'static [&'static str],
    schema: Schema,
}

impl Connector {
    /// Whether this connector's connection string is a URL that can carry an
    /// unrecognised `?key=value` along as a driver option. Mirrors `driver_options`
    /// in `crates/cli/src/main.rs` (keyed there by endpoint tag) — keep both in step.
    fn driver_options(&self) -> bool {
        matches!(
            self.slug,
            "postgres"
                | "postgres-cdc"
                | "mongodb"
                | "redis"
                | "rabbitmq"
                | "http"
                | "clickhouse"
                | "websocket"
        )
    }
}

fn main() -> std::io::Result<()> {
    let connectors = vec![
        Connector {
            slug: "postgres",
            title: "PostgreSQL / MySQL / MariaDB / SQLite",
            schemes: &["postgres", "postgresql", "mysql", "mariadb", "sqlite"],
            schema: schemars::schema_for!(SqlxConfig),
        },
        Connector {
            slug: "postgres-cdc",
            title: "PostgreSQL CDC (logical replication)",
            schemes: &["postgres-cdc", "pgcdc"],
            schema: schemars::schema_for!(PostgresCdcConfig),
        },
        Connector {
            slug: "clickhouse",
            title: "ClickHouse",
            schemes: &["clickhouse", "clickhouses"],
            schema: schemars::schema_for!(ClickHouseConfig),
        },
        Connector {
            slug: "kafka",
            title: "Kafka",
            schemes: &["kafka"],
            schema: schemars::schema_for!(KafkaConfig),
        },
        Connector {
            slug: "mqtt",
            title: "MQTT",
            schemes: &["mqtt", "mqtts"],
            schema: schemars::schema_for!(MqttConfig),
        },
        Connector {
            slug: "rabbitmq",
            title: "RabbitMQ (AMQP)",
            schemes: &["amqp", "amqps", "rabbitmq", "rabbitmqs"],
            schema: schemars::schema_for!(AmqpConfig),
        },
        Connector {
            slug: "http",
            title: "HTTP",
            schemes: &["http", "https"],
            schema: schemars::schema_for!(HttpConfig),
        },
        Connector {
            slug: "websocket",
            title: "WebSocket",
            schemes: &["ws", "wss"],
            schema: schemars::schema_for!(WebSocketConfig),
        },
        Connector {
            slug: "mongodb",
            title: "MongoDB",
            schemes: &["mongodb"],
            schema: schemars::schema_for!(MongoDbConfig),
        },
        Connector {
            slug: "file",
            title: "File (CSV / JSON / JSONL)",
            schemes: &["file"],
            schema: schemars::schema_for!(FileConfig),
        },
        Connector {
            slug: "object-store",
            title: "Cloud Object Storage",
            schemes: &["s3", "s3a", "gs", "gcs", "az", "azure", "abfs", "abfss"],
            schema: schemars::schema_for!(ObjectStoreConfig),
        },
        Connector {
            slug: "nats",
            title: "NATS",
            schemes: &["nats"],
            schema: schemars::schema_for!(NatsConfig),
        },
        Connector {
            slug: "redis",
            title: "Redis Streams",
            schemes: &["redis", "rediss", "redis_streams"],
            schema: schemars::schema_for!(RedisStreamsConfig),
        },
        Connector {
            slug: "grpc",
            title: "gRPC",
            schemes: &["grpc", "grpcs"],
            schema: schemars::schema_for!(GrpcConfig),
        },
        Connector {
            slug: "aws",
            title: "AWS SQS / SNS",
            schemes: &["aws", "aws-sqs"],
            schema: schemars::schema_for!(AwsConfig),
        },
        Connector {
            slug: "zeromq",
            title: "ZeroMQ",
            schemes: &["zeromq", "zmq"],
            schema: schemars::schema_for!(ZeroMqConfig),
        },
        Connector {
            slug: "ibmmq",
            title: "IBM MQ",
            schemes: &["ibmmq", "ibm-mq"],
            schema: schemars::schema_for!(IbmMqConfig),
        },
    ];

    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/docs/reference");
    std::fs::create_dir_all(&out_dir)?;

    let mut index = String::from(
        "<!-- GENERATED by `cargo run -p mq-bridge-app --example gen_url_docs`. Do not edit by hand. -->\n\n\
         # URL Parameter Reference\n\n\
         Auto-generated from the same JSON Schemas `mqb copy` uses to parse \
         `--from`/`--to` query parameters (see `crates/cli/src/main.rs::endpoint_from_uri`). \
         For workflow examples, see the [connector pages](../connectors/).\n\n",
    );

    for c in &connectors {
        let page = render_connector(c);
        std::fs::write(out_dir.join(format!("{}.md", c.slug)), page)?;
        index.push_str(&format!(
            "- [{}](./{}.md) — schemes: {}\n",
            c.title,
            c.slug,
            c.schemes
                .iter()
                .map(|s| format!("`{s}://`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    std::fs::write(out_dir.join("README.md"), index)?;

    println!(
        "Wrote {} reference pages to {}",
        connectors.len(),
        out_dir.display()
    );
    Ok(())
}

struct FieldDoc {
    name: String,
    ty: String,
    required: bool,
    default: Option<Value>,
    description: String,
    /// For struct-typed fields (e.g. `tls`), the resolved `properties` schema
    /// so its own fields can be documented in a nested table instead of just
    /// dumping the default as opaque JSON.
    nested: Option<Value>,
}

/// A `#[serde(flatten)]`ed internally-tagged enum (e.g. `FileConsumerMode`)
/// walks as multiple `oneOf` branches, each contributing its own copy of the
/// tag field and any fields shared across variants (e.g. both `Consume` and
/// `Subscribe` have a `delete: bool`). Merge same-named fields into one row
/// — accurate enough for docs, since duplicates are near-always the same
/// field reused across mutually-exclusive variants.
fn dedup_fields(fields: Vec<FieldDoc>) -> Vec<FieldDoc> {
    let mut merged: Vec<FieldDoc> = Vec::new();
    for f in fields {
        if let Some(existing) = merged.iter_mut().find(|e| e.name == f.name) {
            let variants: Vec<&str> = existing.ty.split(" \\| ").collect();
            if !variants.contains(&f.ty.as_str()) {
                existing.ty = format!("{} \\| {}", existing.ty, f.ty);
            }
            existing.required &= f.required;
            existing.default = existing.default.take().or(f.default);
            if existing.description.is_empty() {
                existing.description = f.description;
            }
            existing.nested = existing.nested.take().or(f.nested);
        } else {
            merged.push(f);
        }
    }
    merged
}

fn render_connector(c: &Connector) -> String {
    let root: Value = serde_json::to_value(&c.schema).unwrap_or(Value::Null);
    let mut required: BTreeSet<String> = BTreeSet::new();
    if let Some(arr) = root.get("required").and_then(|r| r.as_array()) {
        required.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    let mut fields: Vec<FieldDoc> = Vec::new();
    let mut visited = HashSet::new();
    collect_fields(&root, &root, &required, &mut fields, &mut visited);
    let mut fields = dedup_fields(fields);
    fields.sort_by(|a, b| a.name.cmp(&b.name));

    let schemes = c
        .schemes
        .iter()
        .map(|s| format!("`{s}://`"))
        .collect::<Vec<_>>()
        .join(", ");

    let driver_note = if c.driver_options() {
        " Any other `?key=value` pair is passed through unchanged as a driver \
         option on the connection URL."
            .to_string()
    } else {
        " Unrecognised parameters are not forwarded as driver options, so any \
         other `?key=value` pair is rejected rather than silently ignored."
            .to_string()
    };

    // Object- and array-typed fields are the ones a plain `?key=value` cannot
    // express, so show the syntax using a field this connector actually has.
    // A nested struct illustrates the syntax better than an options list, so an
    // object-typed field is preferred over an array-typed one.
    let json_note = match fields
        .iter()
        .find(|f| f.ty == "object")
        .or_else(|| fields.iter().find(|f| f.ty.starts_with("array of")))
    {
        Some(f) if f.ty == "object" => format!(
            " The object-typed `{}` is set with a JSON literal, e.g. `?{}={{...}}`.",
            f.name, f.name
        ),
        Some(f) => format!(
            " The array-typed `{}` is set with a JSON literal, e.g. `?{}=[...]`.",
            f.name, f.name
        ),
        None => String::new(),
    };

    let mut out = format!(
        "<!-- GENERATED by `cargo run -p mq-bridge-app --example gen_url_docs`. Do not edit by hand. -->\n\n\
         # {title}\n\n\
         Schemes: {schemes}\n\n\
         Query parameters recognised as config fields for this connector.\
         {json_note}{driver_note}\n\n\
         | Name | Type | Required | Default | Description |\n\
         |------|------|----------|---------|-------------|\n",
        title = c.title,
        schemes = schemes,
    );

    let mut appendix = String::new();
    for f in &fields {
        let default_cell = match (&f.default, &f.nested) {
            // A struct-typed field with a resolved schema (e.g. `tls`) gets
            // its own field-by-field sub-table below instead of a JSON dump.
            (_, Some(nested_schema)) => {
                appendix.push_str(&render_nested(&f.name, &root, nested_schema));
                // GitHub keeps underscores in heading anchors, so the link must
                // match the raw field name — `#publication_tables`, not `-`.
                format!("[see below](#{})", f.name)
            }
            // Fields without a resolvable struct schema (e.g. a `Vec<(String,
            // String)>` options list) still get their default pretty-printed
            // separately so it doesn't blow up the table row.
            (Some(v @ (Value::Object(_) | Value::Array(_))), None) => {
                appendix.push_str(&format!(
                    "### `{}`\n\n```json\n{}\n```\n\n",
                    f.name,
                    serde_json::to_string_pretty(v).unwrap_or_default()
                ));
                format!("[see below](#{})", f.name)
            }
            (Some(v), None) => describe_value(v),
            (None, None) => "—".to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            f.name,
            f.ty,
            if f.required { "yes" } else { "no" },
            default_cell,
            f.description.replace('\n', " "),
        ));
    }

    if !appendix.is_empty() {
        out.push_str("\n## Struct-typed fields\n\n");
        out.push_str(&appendix);
    }
    out
}

/// Renders a struct-typed field (e.g. `tls`) as its own field-by-field
/// sub-table, the same shape as the top-level table.
fn render_nested(field_name: &str, root: &Value, schema: &Value) -> String {
    let mut required: BTreeSet<String> = BTreeSet::new();
    if let Some(arr) = schema.get("required").and_then(|r| r.as_array()) {
        required.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    let mut nested_fields: Vec<FieldDoc> = Vec::new();
    let mut visited = HashSet::new();
    collect_fields(root, schema, &required, &mut nested_fields, &mut visited);
    let mut nested_fields = dedup_fields(nested_fields);
    nested_fields.sort_by(|a, b| a.name.cmp(&b.name));

    let description = schema
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let description = sanitize_description(description);

    let mut out = format!("### `{field_name}`\n\n{description}\n\n");
    out.push_str("| Name | Type | Required | Default | Description |\n");
    out.push_str("|------|------|----------|---------|-------------|\n");
    for nf in &nested_fields {
        let default_cell = nf
            .default
            .as_ref()
            .map(describe_value)
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            nf.name,
            nf.ty,
            if nf.required { "yes" } else { "no" },
            default_cell,
            nf.description.replace('\n', " "),
        ));
    }
    out.push('\n');
    out
}

/// Rustdoc descriptions embedded in a nested field's `### field` section may
/// contain their own `#`-headings and bare ``` ``` fences (from `# Examples`
/// blocks). Demote headings so they stay below the surrounding `###` section
/// and label bare fences as `rust` (the only language rustdoc examples use).
fn sanitize_description(desc: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in desc.lines() {
        let trimmed = line.trim_start();
        if trimmed == "```" {
            out.push_str(if in_fence { "```" } else { "```rust" });
            in_fence = !in_fence;
        } else if !in_fence && trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let rest = trimmed.trim_start_matches('#');
            out.push_str(&"#".repeat((level + 3).min(6)));
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn collect_fields(
    root: &Value,
    node: &Value,
    required: &BTreeSet<String>,
    out: &mut Vec<FieldDoc>,
    visited: &mut HashSet<String>,
) {
    let Some(obj) = node.as_object() else { return };

    if let Some(reference) = obj.get("$ref").and_then(|r| r.as_str()) {
        if visited.insert(reference.to_string())
            && let Some(target) = resolve_ref(root, reference)
        {
            collect_fields(root, target, required, out, visited);
        }
        return;
    }

    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for (name, sub) in props {
            out.push(FieldDoc {
                name: name.clone(),
                ty: describe_type(root, sub),
                required: required.contains(name),
                default: sub.get("default").cloned(),
                description: sub
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                nested: resolve_object_schema(root, sub, &mut HashSet::new()).cloned(),
            });
        }
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(|a| a.as_array()) {
            for sub in arr {
                collect_fields(root, sub, required, out, visited);
            }
        }
    }
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let name = reference.rsplit('/').next()?;
    ["$defs", "definitions"]
        .iter()
        .find_map(|defs| root.get(defs).and_then(|d| d.get(name)))
}

/// Follows `$ref`s to find a struct-shaped subschema (one with `properties`)
/// for a property node, so object-typed fields (e.g. `tls`) can be expanded
/// into their own sub-table instead of documented as opaque JSON.
fn resolve_object_schema<'a>(
    root: &'a Value,
    node: &'a Value,
    visited: &mut HashSet<String>,
) -> Option<&'a Value> {
    if let Some(reference) = node.get("$ref").and_then(|r| r.as_str()) {
        if !visited.insert(reference.to_string()) {
            return None;
        }
        return resolve_object_schema(root, resolve_ref(root, reference)?, visited);
    }
    if node.get("properties").is_some() {
        return Some(node);
    }
    None
}

/// Renders a property subschema as a short human-readable type, resolving
/// `$ref`s, unwrapping `Option<T>` (`anyOf`/`oneOf` with a null member), and
/// listing string enum variants inline.
fn describe_type(root: &Value, node: &Value) -> String {
    if let Some(reference) = node.get("$ref").and_then(|r| r.as_str()) {
        return match resolve_ref(root, reference) {
            Some(target) => describe_type(root, target),
            None => reference
                .rsplit('/')
                .next()
                .unwrap_or(reference)
                .to_string(),
        };
    }

    // Unit enum variants are often rendered as `oneOf` members with a `const`
    // (rather than a flat top-level `enum` array), e.g. `{"type":"string","const":"consumer"}`.
    if let Some(c) = node.get("const").and_then(|c| c.as_str()) {
        return format!("`{c}`");
    }

    if let Some(values) = node.get("enum").and_then(|e| e.as_array()) {
        let variants: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| format!("`{s}`"))
            .collect();
        if !variants.is_empty() {
            return variants.join(" \\| ");
        }
    }

    let ty = node.get("type");
    match ty {
        Some(Value::String(s)) if s == "array" => {
            let item = node
                .get("items")
                .map(|i| describe_type(root, i))
                .unwrap_or_else(|| "any".to_string());
            format!("array of {item}")
        }
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let non_null: Vec<&str> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|s| *s != "null")
                .collect();
            if non_null == ["array"] {
                let item = node
                    .get("items")
                    .map(|i| describe_type(root, i))
                    .unwrap_or_else(|| "any".to_string());
                format!("array of {item}")
            } else {
                non_null.join(" \\| ")
            }
        }
        Some(_) | None => {
            for key in ["anyOf", "oneOf"] {
                if let Some(arr) = node.get(key).and_then(|a| a.as_array()) {
                    let variants: Vec<String> = arr
                        .iter()
                        .filter(|m| m.get("type").and_then(|t| t.as_str()) != Some("null"))
                        .map(|m| describe_type(root, m))
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !variants.is_empty() {
                        return variants.join(" \\| ");
                    }
                }
            }
            "any".to_string()
        }
    }
}

fn describe_value(v: &Value) -> String {
    match v {
        Value::String(s) if s.is_empty() => "`\"\"`".to_string(),
        Value::String(s) => format!("`{s}`"),
        Value::Null => "`null`".to_string(),
        other => format!("`{other}`"),
    }
}
