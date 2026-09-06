//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::compiled::Compiled;
use super::error::{ErrorKind, TransformError};
use super::path::{CompiledPath, Seg};
use super::TRANSFORM_ERROR_KEY;
use super::{TransformConsumer, TransformPublisher};
use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
use crate::models::TransformMiddleware;
use crate::traits::{
    ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError, Received,
    ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::any::Any;
use std::sync::{Arc, Mutex};

fn config(value: Value) -> TransformMiddleware {
    serde_json::from_value(value).expect("test config should deserialize")
}

fn compiled(value: Value) -> Compiled {
    Compiled::new(&config(value)).expect("test config should compile")
}

/// Runs a payload through the engine and returns the resulting JSON.
fn run(cfg: &Compiled, payload: Value) -> Result<Value, TransformError> {
    let mut message = CanonicalMessage::from(payload.to_string());
    cfg.transform(&mut message)?;
    Ok(serde_json::from_slice(&message.payload).expect("output should be valid JSON"))
}

// --- Path parsing ---

#[test]
fn test_path_parse_accepts_dollar_prefix_dots_and_indices() {
    let path = CompiledPath::parse("$.a.b[0]").unwrap();
    assert_eq!(
        path.segs,
        vec![
            Seg::Key("a".to_string()),
            Seg::Key("b".to_string()),
            Seg::Index(0)
        ]
    );

    // The `$.` prefix is optional.
    assert_eq!(
        CompiledPath::parse("a.b").unwrap().segs,
        vec![Seg::Key("a".to_string()), Seg::Key("b".to_string())]
    );

    // Consecutive indices, and an index directly on the root.
    assert_eq!(
        CompiledPath::parse("$.a[1][2]").unwrap().segs,
        vec![Seg::Key("a".to_string()), Seg::Index(1), Seg::Index(2)]
    );
    assert_eq!(
        CompiledPath::parse("$[3]").unwrap().segs,
        vec![Seg::Index(3)]
    );
}

#[test]
fn test_path_get_returns_none_for_missing_or_wrong_shape() {
    let doc = json!({ "a": { "b": [10, 20] } });

    assert_eq!(
        CompiledPath::parse("$.a.b[1]").unwrap().get(&doc),
        Some(&json!(20))
    );
    assert_eq!(CompiledPath::parse("$.a.missing").unwrap().get(&doc), None);
    assert_eq!(CompiledPath::parse("$.a.b[9]").unwrap().get(&doc), None);
    // Indexing an object, or keying an array, simply misses rather than erroring.
    assert_eq!(CompiledPath::parse("$.a[0]").unwrap().get(&doc), None);
}

#[test]
fn test_path_parse_rejects_malformed_specs() {
    assert!(CompiledPath::parse("$.a..b").is_err());
    assert!(CompiledPath::parse("$.a[1").is_err());
    assert!(CompiledPath::parse("$.a[x]").is_err());
}

// --- Mapping stage ---

#[test]
fn test_mapping_renames_fields() {
    // The exact example from the feature request.
    let cfg = compiled(json!({
        "mapping": {
            "firstName": "$.first_name",
            "lastName": "$.last_name",
            "id": "$.user_id",
        }
    }));

    let out = run(
        &cfg,
        json!({ "first_name": "John", "last_name": "Smith", "user_id": "42" }),
    )
    .unwrap();

    assert_eq!(
        out,
        json!({ "firstName": "John", "lastName": "Smith", "id": "42" })
    );
}

#[test]
fn test_mapping_reads_nested_and_writes_nested() {
    let cfg = compiled(json!({
        "mapping": {
            "user.name": "$.profile.details.name",
            "user.city": "$.addresses[0].city",
            "flat": "$.top",
        }
    }));

    let out = run(
        &cfg,
        json!({
            "profile": { "details": { "name": "Ada" } },
            "addresses": [{ "city": "London" }, { "city": "Paris" }],
            "top": 1,
        }),
    )
    .unwrap();

    assert_eq!(
        out,
        json!({ "user": { "name": "Ada", "city": "London" }, "flat": 1 })
    );
}

#[test]
fn test_mapping_omits_absent_optional_and_uses_defaults() {
    let cfg = compiled(json!({
        "mapping": {
            "present": "$.here",
            "absent": "$.nope",
            "defaulted": { "path": "$.nope", "default": "fallback" },
        }
    }));

    let out = run(&cfg, json!({ "here": "yes" })).unwrap();

    // `absent` is omitted entirely rather than emitted as null.
    assert_eq!(out, json!({ "present": "yes", "defaulted": "fallback" }));
}

#[test]
fn test_mapping_required_missing_is_rejected() {
    let cfg = compiled(json!({
        "mapping": { "id": { "path": "$.user_id", "required": true } }
    }));

    let error = run(&cfg, json!({ "other": 1 })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::MissingRequired);
    assert!(error.to_string().contains("$.user_id"), "{error}");
}

// --- Embedded JSON (contentMediaType / contentSchema) ---

#[test]
fn test_content_schema_parses_embedded_json_and_applies_the_inner_schema() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "payload": {
                    "type": "string",
                    "contentMediaType": "application/json",
                    "contentSchema": {
                        "type": "object",
                        "properties": { "qty": { "type": "integer" } },
                    },
                },
            },
        }
    }));

    // The inner `"7"` proves the decoded document goes through the same coercion pass.
    let out = run(&cfg, json!({ "payload": "{\"qty\": \"7\"}" })).unwrap();

    assert_eq!(out, json!({ "payload": { "qty": 7 } }));
}

#[test]
fn test_content_media_type_alone_parses_without_validating() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "payload": { "type": "string", "contentMediaType": "application/json" },
            },
        }
    }));

    let out = run(&cfg, json!({ "payload": "[1, 2]" })).unwrap();

    assert_eq!(out, json!({ "payload": [1, 2] }));
}

#[test]
fn test_content_schema_rejects_a_string_that_is_not_json() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "payload": { "type": "string", "contentMediaType": "application/json" },
            },
        }
    }));

    let error = run(&cfg, json!({ "payload": "not json" })).unwrap_err();

    assert_eq!(error.kind, ErrorKind::Content);
    assert!(error.to_string().contains("$.payload"), "{error}");
}

#[test]
fn test_structured_suffix_media_type_is_parsed() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "payload": {
                    "type": "string",
                    "contentMediaType": "application/vnd.acme.order+json; charset=utf-8",
                },
            },
        }
    }));

    let out = run(&cfg, json!({ "payload": "{\"a\": 1}" })).unwrap();

    assert_eq!(out, json!({ "payload": { "a": 1 } }));
}

#[test]
fn test_unparseable_content_keywords_leave_the_string_untouched() {
    // A non-JSON media type, an encoding we do not implement, and a `contentSchema`
    // with no media type are all ignored like any other unsupported keyword, so a
    // fuller pre-existing schema stays usable.
    for schema in [
        json!({ "type": "string", "contentMediaType": "text/csv" }),
        json!({
            "type": "string",
            "contentMediaType": "application/json",
            "contentEncoding": "base64",
        }),
        json!({ "type": "string", "contentSchema": { "type": "object" } }),
    ] {
        let cfg = compiled(json!({
            "schema": { "type": "object", "properties": { "payload": schema } }
        }));

        let out = run(&cfg, json!({ "payload": "{\"a\": 1}" })).unwrap();

        assert_eq!(out, json!({ "payload": "{\"a\": 1}" }));
    }
}

#[test]
fn test_root_schema_decodes_a_double_encoded_body() {
    let cfg = compiled(json!({
        "schema": {
            "type": "string",
            "contentMediaType": "application/json",
            "contentSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
            },
        }
    }));

    let out = run(&cfg, json!("{\"id\": \"5\"}")).unwrap();

    assert_eq!(out, json!({ "id": 5 }));
}

#[test]
fn test_coerce_alone_never_turns_a_string_into_an_object() {
    // The guarantee that keeps embedded JSON opt-in: `coerce` widens scalars only.
    let cfg = compiled(json!({
        "schema": { "type": "object", "properties": { "payload": { "type": "object" } } }
    }));

    let error = run(&cfg, json!({ "payload": "{\"a\": 1}" })).unwrap_err();

    assert_eq!(error.kind, ErrorKind::Coercion);
}

// --- Coercion ---

#[test]
fn test_coercion_matrix_accepts_every_safe_conversion() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "int": { "type": "integer" },
                "float": { "type": "number" },
                "flag": { "type": "boolean" },
                "text": { "type": "string" },
            }
        }
    }));

    let out = run(
        &cfg,
        json!({ "int": "42", "float": "3.5", "flag": "true", "text": 7 }),
    )
    .unwrap();

    assert_eq!(
        out,
        json!({ "int": 42, "float": 3.5, "flag": true, "text": "7" })
    );
}

#[test]
fn test_coercion_accepts_both_boolean_spellings() {
    let cfg = compiled(json!({
        "schema": { "type": "object", "properties": { "flag": { "type": "boolean" } } }
    }));

    for (input, expected) in [("true", true), ("1", true), ("false", false), ("0", false)] {
        let out = run(&cfg, json!({ "flag": input })).unwrap();
        assert_eq!(out, json!({ "flag": expected }), "input {input}");
    }
}

#[test]
fn test_coercion_failure_reports_field_path_and_is_non_retryable() {
    let cfg = compiled(json!({
        "schema": { "type": "object", "properties": { "user_id": { "type": "integer" } } }
    }));

    let error = run(&cfg, json!({ "user_id": "abc" })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Coercion);
    assert_eq!(error.path, "$.user_id");
    assert!(error.to_string().contains("cannot coerce"), "{error}");

    // The DLQ path depends on this classification.
    let publisher_error: PublisherError = error.into();
    assert!(matches!(publisher_error, PublisherError::NonRetryable(_)));
}

#[test]
fn test_coercion_disabled_reports_type_mismatch_instead() {
    let cfg = compiled(json!({
        "coerce": false,
        "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
    }));

    let error = run(&cfg, json!({ "n": "42" })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::TypeMismatch);
}

#[test]
fn test_nested_error_path_includes_array_index() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "qty": { "type": "integer" } }
                    }
                }
            }
        }
    }));

    let error = run(
        &cfg,
        json!({ "items": [{ "qty": "1" }, { "qty": "oops" }] }),
    )
    .unwrap_err();
    assert_eq!(error.path, "$.items[1].qty");
}

// --- Defaults, required, nullable, enum ---

#[test]
fn test_defaults_are_applied_and_satisfy_required() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "required": ["status"],
            "properties": { "status": { "type": "string", "default": "new" } }
        }
    }));

    let out = run(&cfg, json!({})).unwrap();
    assert_eq!(out, json!({ "status": "new" }));
}

#[test]
fn test_defaults_can_be_disabled() {
    let cfg = compiled(json!({
        "apply_defaults": false,
        "schema": {
            "type": "object",
            "properties": { "status": { "type": "string", "default": "new" } }
        }
    }));

    assert_eq!(run(&cfg, json!({})).unwrap(), json!({}));
}

#[test]
fn test_required_without_default_is_rejected() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "integer" } }
        }
    }));

    let error = run(&cfg, json!({ "other": 1 })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::MissingRequired);
    assert_eq!(error.path, "$.id");
}

#[test]
fn test_nullable_accepts_null_in_both_spellings() {
    for schema in [
        json!({ "type": "object", "properties": { "note": { "type": "string", "nullable": true } } }),
        json!({ "type": "object", "properties": { "note": { "type": ["string", "null"] } } }),
    ] {
        let cfg = compiled(json!({ "schema": schema }));
        let out = run(&cfg, json!({ "note": null })).unwrap();
        assert_eq!(out, json!({ "note": null }));
    }
}

#[test]
fn test_non_nullable_null_falls_back_to_default_then_fails() {
    let with_default = compiled(json!({
        "schema": {
            "type": "object",
            "properties": { "n": { "type": "integer", "default": 0 } }
        }
    }));
    assert_eq!(
        run(&with_default, json!({ "n": null })).unwrap(),
        json!({ "n": 0 })
    );

    let without_default = compiled(json!({
        "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
    }));
    let error = run(&without_default, json!({ "n": null })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::TypeMismatch);
    assert_eq!(error.path, "$.n");
    assert!(
        error.to_string().contains("not nullable"),
        "null should be reported plainly, not as a coercion failure: {error}"
    );
}

#[test]
fn test_invalid_enum_value_is_rejected() {
    let cfg = compiled(json!({
        "schema": {
            "type": "object",
            "properties": { "status": { "type": "string", "enum": ["new", "done"] } }
        }
    }));

    assert!(run(&cfg, json!({ "status": "new" })).is_ok());

    let error = run(&cfg, json!({ "status": "bogus" })).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Enum);
    assert_eq!(error.path, "$.status");
}

#[test]
fn test_unknown_schema_keywords_are_ignored_not_rejected() {
    // A fuller schema can be pointed at without being rewritten.
    let cfg = compiled(json!({
        "schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "User",
            "additionalProperties": false,
            "type": "object",
            "properties": { "id": { "type": "integer", "minimum": 0 } }
        }
    }));

    assert_eq!(run(&cfg, json!({ "id": "5" })).unwrap(), json!({ "id": 5 }));
}

#[test]
fn test_mapping_then_schema_run_in_order() {
    let cfg = compiled(json!({
        "mapping": { "id": "$.user_id", "name": "$.first_name" },
        "schema": {
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": { "type": "integer" },
                "name": { "type": "string" }
            }
        }
    }));

    // "42" survives the mapping as a string, then the schema coerces it.
    let out = run(&cfg, json!({ "user_id": "42", "first_name": "John" })).unwrap();
    assert_eq!(out, json!({ "id": 42, "name": "John" }));
}

// --- Config plumbing ---

#[test]
fn test_non_json_payload_is_rejected_as_parse_error() {
    let cfg = compiled(json!({
        "schema": { "type": "object" }
    }));

    let mut message = CanonicalMessage::from("not json at all");
    let error = cfg.transform(&mut message).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Parse);
}

#[test]
fn test_rust_default_matches_parsed_empty_config() {
    // A derived Default would make these false, so `TransformMiddleware::default()` in
    // Rust would silently disable coercion while the same empty YAML enables it.
    let from_rust = TransformMiddleware::default();
    let from_config = config(json!({}));

    assert!(from_rust.coerce);
    assert!(from_rust.apply_defaults);
    assert_eq!(from_rust.coerce, from_config.coerce);
    assert_eq!(from_rust.apply_defaults, from_config.apply_defaults);
    assert_eq!(from_rust.on_error, from_config.on_error);
}

#[test]
fn test_config_with_neither_stage_is_a_noop() {
    assert!(compiled(json!({})).is_noop());
    // A stage being present is what disables the fast path.
    assert!(!compiled(json!({ "mapping": { "a": "$.b" } })).is_noop());
    assert!(!compiled(json!({ "schema": { "type": "object" } })).is_noop());
}

#[test]
fn test_schema_and_schema_file_together_are_rejected() {
    let error = Compiled::new(&config(json!({
        "schema": { "type": "object" },
        "schema_file": "/tmp/does-not-matter.json",
    })))
    .unwrap_err();
    assert!(error.to_string().contains("not both"), "{error}");
}

#[cfg(feature = "zen")]
#[test]
fn test_expression_runs_after_mapping_and_reads_metadata() {
    let cfg = compiled(json!({
        "mapping": {
            "first": "$.first_name",
            "last": "$.last_name"
        },
        "expression": "{ fullName: first + ' ' + last, source: meta.source }"
    }));
    let mut message =
        CanonicalMessage::from(json!({ "first_name": "Ada", "last_name": "Lovelace" }).to_string());
    message
        .metadata
        .insert("source".to_string(), "postgres".to_string());

    cfg.transform(&mut message).unwrap();

    assert_eq!(
        serde_json::from_slice::<Value>(&message.payload).unwrap(),
        json!({ "fullName": "Ada Lovelace", "source": "postgres" })
    );
}

#[cfg(feature = "zen")]
#[test]
fn test_schema_is_applied_after_expression() {
    let cfg = compiled(json!({
        "expression": "{ id: user_id }",
        "schema": {
            "type": "object",
            "properties": { "id": { "type": "integer" } }
        }
    }));

    assert_eq!(
        run(&cfg, json!({ "user_id": "42" })).unwrap(),
        json!({ "id": 42 })
    );
}

#[cfg(feature = "zen")]
#[test]
fn test_invalid_expression_is_rejected_at_construction() {
    let error = Compiled::new(&config(json!({ "expression": "first_name +" }))).unwrap_err();
    assert!(error.to_string().contains("invalid expression"), "{error}");
}

#[test]
fn test_schema_file_is_read_once_at_construction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("user.json");
    std::fs::write(
        &path,
        json!({ "type": "object", "properties": { "id": { "type": "integer" } } }).to_string(),
    )
    .unwrap();

    let cfg = compiled(json!({ "schema_file": path.to_str().unwrap() }));
    assert_eq!(run(&cfg, json!({ "id": "7" })).unwrap(), json!({ "id": 7 }));

    // Deleting the file afterwards must not affect the hot path.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(run(&cfg, json!({ "id": "8" })).unwrap(), json!({ "id": 8 }));
}

#[test]
fn test_missing_schema_file_fails_at_construction() {
    let error = Compiled::new(&config(json!({
        "schema_file": "/definitely/not/here.json"
    })))
    .unwrap_err();
    assert!(
        error.to_string().contains("cannot read schema file"),
        "{error}"
    );
}

#[test]
fn test_documented_yaml_config_deserializes_and_compiles() {
    // Mirrors the README example, so the documented surface stays honest.
    let yaml = r#"
middlewares:
  - transform:
      mapping:
        firstName: "$.first_name"
        lastName: "$.last_name"
        id: "$.user_id"
        "address.city": { path: "$.city", default: "unknown" }
      schema:
        type: object
        required: ["firstName", "id"]
        properties:
          firstName: { type: string }
          id: { type: integer }
          address:
            type: object
            properties:
              city: { type: string }
  - dlq:
      endpoint:
        memory: { topic: "rejected" }
memory:
  topic: "users"
"#;
    let endpoint: crate::models::Endpoint = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(endpoint.middlewares.len(), 2);

    let crate::models::Middleware::Transform(cfg) = &endpoint.middlewares[0] else {
        panic!("first middleware should be transform");
    };
    let compiled = Compiled::new(cfg).unwrap();

    let out = run(
        &compiled,
        json!({ "first_name": "John", "last_name": "Smith", "user_id": "42" }),
    )
    .unwrap();
    assert_eq!(
        out,
        json!({
            "firstName": "John",
            "lastName": "Smith",
            "id": 42,
            "address": { "city": "unknown" }
        })
    );
}

// --- Publisher attach point ---

#[tokio::test]
async fn test_publisher_forwards_transformed_payloads() {
    let inner = MemoryPublisher::new_local("transform_pub_ok", 10);
    let channel = inner.channel();
    let publisher = TransformPublisher::new(
        Box::new(inner),
        &config(json!({ "mapping": { "id": "$.user_id" } })),
    )
    .unwrap();

    publisher
        .send_batch(vec![CanonicalMessage::from(r#"{"user_id":"42"}"#)])
        .await
        .unwrap();

    let sent = channel.drain_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].get_payload_str(), r#"{"id":"42"}"#);
}

#[tokio::test]
async fn test_publisher_reports_bad_message_as_non_retryable_and_sends_the_rest() {
    let inner = MemoryPublisher::new_local("transform_pub_partial", 10);
    let channel = inner.channel();
    let publisher = TransformPublisher::new(
        Box::new(inner),
        &config(json!({
            "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
        })),
    )
    .unwrap();

    let outcome = publisher
        .send_batch(vec![
            CanonicalMessage::from(r#"{"n":"1"}"#),
            CanonicalMessage::from(r#"{"n":"abc"}"#),
            CanonicalMessage::from(r#"{"n":"3"}"#),
        ])
        .await
        .unwrap();

    match outcome {
        SentBatch::Partial { failed, .. } => {
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].0.get_payload_str(), r#"{"n":"abc"}"#);
            assert!(matches!(failed[0].1, PublisherError::NonRetryable(_)));
        }
        other => panic!("expected Partial, got {other:?}"),
    }

    // The two valid messages still went through.
    let sent = channel.drain_messages();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].get_payload_str(), r#"{"n":1}"#);
    assert_eq!(sent[1].get_payload_str(), r#"{"n":3}"#);
}

#[tokio::test]
async fn test_publisher_pass_through_policy_annotates_instead_of_failing() {
    let inner = MemoryPublisher::new_local("transform_pub_passthrough", 10);
    let channel = inner.channel();
    let publisher = TransformPublisher::new(
        Box::new(inner),
        &config(json!({
            "on_error": "pass_through",
            "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
        })),
    )
    .unwrap();

    publisher
        .send_batch(vec![CanonicalMessage::from(r#"{"n":"abc"}"#)])
        .await
        .unwrap();

    let sent = channel.drain_messages();
    assert_eq!(sent.len(), 1);
    // Payload is untouched, and the reason is carried for downstream routing.
    assert_eq!(sent[0].get_payload_str(), r#"{"n":"abc"}"#);
    assert!(sent[0].metadata.contains_key(TRANSFORM_ERROR_KEY));
}

#[tokio::test]
async fn test_noop_publisher_passes_invalid_json_straight_through() {
    let inner = MemoryPublisher::new_local("transform_pub_noop", 10);
    let channel = inner.channel();
    let publisher = TransformPublisher::new(Box::new(inner), &config(json!({}))).unwrap();

    publisher
        .send_batch(vec![CanonicalMessage::from("not json at all")])
        .await
        .unwrap();

    let sent = channel.drain_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].get_payload_str(), "not json at all");
}

#[tokio::test]
async fn test_rejected_message_reaches_the_dlq_through_the_config_wiring() {
    use crate::models::{DeadLetterQueueMiddleware, Endpoint, Middleware};

    let dlq_endpoint = Endpoint::new_memory("transform_dlq_rejects", 10);
    let inner = MemoryPublisher::new_local("transform_dlq_main", 10);
    let main_channel = inner.channel();

    // Publisher middlewares are wrapped in list order, so the *last* entry is the
    // outermost layer: `dlq` must follow `transform` to catch its rejections.
    let mut output = Endpoint::new_memory("transform_dlq_main", 10);
    output.middlewares = vec![
        Middleware::Transform(config(json!({
            "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
        }))),
        Middleware::Dlq(Box::new(DeadLetterQueueMiddleware {
            endpoint: dlq_endpoint.clone(),
        })),
    ];

    let publisher =
        crate::middleware::apply_middlewares_to_publisher(Box::new(inner), &output, "test_route")
            .await
            .unwrap();

    publisher
        .send(CanonicalMessage::from(r#"{"n":"not-a-number"}"#))
        .await
        .unwrap();
    publisher
        .send(CanonicalMessage::from(r#"{"n":"5"}"#))
        .await
        .unwrap();

    let dlq_channel = dlq_endpoint.channel().unwrap();
    let dlq_messages = dlq_channel.drain_messages();
    assert_eq!(
        dlq_messages.len(),
        1,
        "the invalid message should be dead-lettered"
    );
    // The DLQ receives the original payload, not a half-transformed one.
    assert_eq!(dlq_messages[0].get_payload_str(), r#"{"n":"not-a-number"}"#);

    let delivered = main_channel.drain_messages();
    assert_eq!(
        delivered.len(),
        1,
        "the valid message should still be delivered"
    );
    assert_eq!(delivered[0].get_payload_str(), r#"{"n":5}"#);
}

// --- Consumer attach point ---

/// Inner consumer that yields one prepared batch and records the dispositions its
/// commit is called with, so the index remapping can be asserted.
struct RecordingConsumer {
    batch: Option<Vec<CanonicalMessage>>,
    recorded: Arc<Mutex<Option<Vec<MessageDisposition>>>>,
}

#[async_trait]
impl MessageConsumer for RecordingConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        Err(ConsumerError::EndOfStream)
    }

    async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
        let messages = self.batch.take().ok_or(ConsumerError::EndOfStream)?;
        let recorded = self.recorded.clone();
        Ok(ReceivedBatch {
            messages,
            commit: Box::new(move |dispositions| {
                *recorded.lock().unwrap() = Some(dispositions);
                Box::pin(async { Ok(()) })
            }),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[tokio::test]
async fn test_consumer_drops_invalid_messages_and_remaps_commit_indices() {
    let recorded = Arc::new(Mutex::new(None));
    let inner = RecordingConsumer {
        // Index 1 is invalid and will be dropped.
        batch: Some(vec![
            CanonicalMessage::from(r#"{"n":"1"}"#),
            CanonicalMessage::from(r#"{"n":"bad"}"#),
            CanonicalMessage::from(r#"{"n":"3"}"#),
        ]),
        recorded: recorded.clone(),
    };

    let mut consumer = TransformConsumer::new(
        Box::new(inner),
        &config(json!({
            "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
        })),
    )
    .unwrap();

    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    assert_eq!(batch.messages[0].get_payload_str(), r#"{"n":1}"#);
    assert_eq!(batch.messages[1].get_payload_str(), r#"{"n":3}"#);

    // Nack the second surviving message: it must land on original index 2, and the
    // dropped index 1 must be acked rather than left to redeliver forever.
    (batch.commit)(vec![MessageDisposition::Ack, MessageDisposition::Nack])
        .await
        .unwrap();

    let dispositions = recorded.lock().unwrap().take().expect("commit was called");
    assert_eq!(dispositions.len(), 3);
    assert!(matches!(dispositions[0], MessageDisposition::Ack));
    assert!(matches!(dispositions[1], MessageDisposition::Ack));
    assert!(matches!(dispositions[2], MessageDisposition::Nack));
}

#[tokio::test]
async fn test_consumer_passes_commit_through_untouched_when_nothing_is_dropped() {
    let recorded = Arc::new(Mutex::new(None));
    let inner = RecordingConsumer {
        batch: Some(vec![
            CanonicalMessage::from(r#"{"n":"1"}"#),
            CanonicalMessage::from(r#"{"n":"2"}"#),
        ]),
        recorded: recorded.clone(),
    };

    let mut consumer = TransformConsumer::new(
        Box::new(inner),
        &config(json!({
            "schema": { "type": "object", "properties": { "n": { "type": "integer" } } }
        })),
    )
    .unwrap();

    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    (batch.commit)(vec![MessageDisposition::Nack, MessageDisposition::Ack])
        .await
        .unwrap();

    let dispositions = recorded.lock().unwrap().take().expect("commit was called");
    assert_eq!(dispositions.len(), 2);
    assert!(matches!(dispositions[0], MessageDisposition::Nack));
}

#[tokio::test]
async fn test_consumer_transforms_from_a_real_memory_endpoint() {
    let inner = MemoryConsumer::new_local("transform_consumer_in", 10);
    let channel = inner.channel();
    channel
        .send_message(CanonicalMessage::from(
            r#"{"first_name":"John","user_id":"42"}"#,
        ))
        .await
        .unwrap();

    let mut consumer = TransformConsumer::new(
        Box::new(inner),
        &config(json!({
            "mapping": { "firstName": "$.first_name", "id": "$.user_id" },
            "schema": {
                "type": "object",
                "required": ["firstName", "id"],
                "properties": { "firstName": { "type": "string" }, "id": { "type": "integer" } }
            }
        })),
    )
    .unwrap();

    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    let out: Value = serde_json::from_slice(&batch.messages[0].payload).unwrap();
    assert_eq!(out, json!({ "firstName": "John", "id": 42 }));
}

// --- coerce_empty_as_null ---

/// The same schema with and without the flag, so the default stays visible: an empty
/// string is an ordinary string unless it is opted in.
fn empty_as_null_cfg(properties: Value, on: bool) -> Compiled {
    compiled(json!({
        "schema": {"type": "object", "properties": properties},
        "coerce_empty_as_null": on,
    }))
}

#[test]
fn test_empty_string_is_a_string_unless_opted_in() {
    let props = json!({"email": {"type": "string", "nullable": true}});
    let out = run(&empty_as_null_cfg(props, false), json!({"email": ""})).unwrap();
    assert_eq!(out, json!({"email": ""}));
}

#[test]
fn test_coerce_empty_as_null_nulls_only_empty_strings() {
    let props = json!({
        "email": {"type": "string", "nullable": true},
        "name": {"type": "string"},
        "qty": {"type": "integer", "nullable": true},
    });
    let out = run(
        &empty_as_null_cfg(props, true),
        json!({"email": "", "name": " ", "qty": 0}),
    )
    .unwrap();
    // A blank is not empty and `0` is not falsy: only `""` is affected.
    assert_eq!(out, json!({"email": null, "name": " ", "qty": 0}));
}

#[test]
fn test_coerce_empty_as_null_lets_the_default_win() {
    let props = json!({"tier": {"type": "string", "default": "standard"}});
    let out = run(&empty_as_null_cfg(props, true), json!({"tier": ""})).unwrap();
    assert_eq!(out, json!({"tier": "standard"}));
}

#[test]
fn test_coerce_empty_as_null_rejects_a_non_nullable_field() {
    let props = json!({"email": {"type": "string"}});
    let err = run(&empty_as_null_cfg(props, true), json!({"email": ""})).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TypeMismatch);
    assert_eq!(err.path, "$.email");
    // The message has to name the coercion, or the null looks like it was in the payload.
    assert!(
        err.detail.contains("empty string"),
        "unhelpful detail: {}",
        err.detail
    );
}

#[test]
fn test_coerce_empty_as_null_reaches_nested_and_array_fields() {
    let props = json!({
        "user": {"type": "object", "properties": {"nick": {"type": "string", "nullable": true}}},
        "tags": {"type": "array", "items": {"type": "string", "nullable": true}},
    });
    let out = run(
        &empty_as_null_cfg(props, true),
        json!({"user": {"nick": ""}, "tags": ["a", ""]}),
    )
    .unwrap();
    assert_eq!(out, json!({"user": {"nick": null}, "tags": ["a", null]}));
}

#[test]
fn test_coerce_empty_as_null_leaves_fields_the_schema_does_not_mention() {
    let props = json!({"email": {"type": "string", "nullable": true}});
    let out = run(&empty_as_null_cfg(props, true), json!({"note": ""})).unwrap();
    assert_eq!(out, json!({"note": ""}));
}

mod fast_path_equivalence {
    use super::super::compiled::{map_sorts_keys, Compiled};
    use crate::models::{DetailedMappingRule, MappingRule, TransformMiddleware};
    use crate::CanonicalMessage;
    use serde_json::{json, Map, Value};

    /// What the three runs of a payload produced.
    struct Outcomes {
        slow: Result<String, String>,
        fast: Result<String, String>,
        /// The fast path with `sort_keys` inverted — that is, how it behaves in a build
        /// whose `serde_json/preserve_order` setting differs from this one. `mq-bridge-app`
        /// is such a build (`rmcp` pulls the feature in), so without this the configuration
        /// the binary actually ships would never be exercised by these tests.
        fast_other_order: Result<String, String>,
        eligible: bool,
    }

    fn both(schema: Value, payload: &str) -> Outcomes {
        both_with(
            TransformMiddleware {
                schema: Some(schema),
                ..Default::default()
            },
            payload,
        )
    }

    fn both_with(config: TransformMiddleware, payload: &str) -> Outcomes {
        let mut compiled = Compiled::new(&config).unwrap();
        let eligible = compiled.fast_eligible;
        let sort_keys = compiled.sort_keys;

        let run = |compiled: &Compiled| {
            let mut message = CanonicalMessage::new(payload.as_bytes().to_vec(), None);
            match compiled.transform(&mut message) {
                Ok(()) => Ok(String::from_utf8(message.payload.to_vec()).unwrap()),
                Err(e) => Err(format!("{}:{}", e.kind.as_str(), e.path)),
            }
        };

        compiled.fast_eligible = false;
        let slow = run(&compiled);

        compiled.fast_eligible = eligible;
        let fast = run(&compiled);

        compiled.sort_keys = !sort_keys;
        let fast_other_order = run(&compiled);

        Outcomes {
            slow,
            fast,
            fast_other_order,
            eligible,
        }
    }

    fn as_json(r: &Result<String, String>) -> Result<Value, String> {
        match r {
            Ok(s) => Ok(serde_json::from_str(s).expect("valid JSON out")),
            Err(e) => Err(e.clone()),
        }
    }

    /// Compares outcomes as parsed JSON: object key order and escape spelling are
    /// serialization choices, not data. Both key orderings must agree with the normal path.
    #[track_caller]
    fn assert_same_with(config: TransformMiddleware, payload: &str) {
        let out = both_with(config, payload);
        assert_eq!(
            as_json(&out.slow),
            as_json(&out.fast),
            "paths disagree on payload: {payload}"
        );
        assert_eq!(
            as_json(&out.slow),
            as_json(&out.fast_other_order),
            "paths disagree under the opposite key ordering on payload: {payload}"
        );
    }

    #[track_caller]
    fn assert_same(schema: Value, payload: &str) {
        assert_same_with(
            TransformMiddleware {
                schema: Some(schema),
                ..Default::default()
            },
            payload,
        );
    }

    /// Same as `assert_same`, and additionally requires the fast path to have been taken —
    /// so a case meant to exercise it cannot silently start falling back and still pass.
    #[track_caller]
    fn assert_same_via_fast(schema: Value, payload: &str) {
        let out = both(schema.clone(), payload);
        assert!(
            out.eligible,
            "expected the fast path to be eligible for {schema}"
        );
        assert_same(schema, payload);
    }

    /// Stronger than `assert_same`: byte for byte, for the key ordering this build uses.
    #[track_caller]
    fn assert_byte_identical_with(config: TransformMiddleware, payload: &str) {
        let out = both_with(config, payload);
        assert!(out.eligible, "expected the fast path to be eligible");
        assert_eq!(
            out.slow, out.fast,
            "byte output differs for payload: {payload}"
        );
        assert_eq!(
            out.slow, out.fast_other_order,
            "byte output differs under the opposite key ordering: {payload}"
        );
    }

    #[track_caller]
    fn assert_byte_identical(schema: Value, payload: &str) {
        let out = both(schema.clone(), payload);
        assert!(
            out.eligible,
            "expected the fast path to be eligible for {schema}"
        );
        assert_eq!(
            out.slow, out.fast,
            "byte output differs for payload: {payload}"
        );
    }

    /// A text-typed column rewritten straight from its raw span must be byte-identical to
    /// what building a `Value` for the field and re-serializing it produces — for the
    /// values that convert *and* for the ones that must still fail with the same error.
    #[test]
    fn raw_scalar_coercion_matches_the_value_path() {
        let cases: &[(&str, &[&str])] = &[
            (
                "integer",
                &[
                    "0",
                    "42",
                    "-42",
                    "007",
                    "+42",
                    " 42 ",
                    "\\t7\\n",
                    // The i64/u64 boundary, and past it.
                    "9223372036854775807",
                    "9223372036854775808",
                    "18446744073709551615",
                    "18446744073709551616",
                    // Must fail exactly as the general path fails.
                    "4.5",
                    "1e3",
                    "abc",
                    "",
                    " ",
                    "-",
                    "0x10",
                ],
            ),
            (
                "number",
                &[
                    "1.5", "-0.001", "0", "1e3", "2.5000", "0.1", " 2.5 ", "-0", "1e400", "NaN",
                    "inf", "abc", "", "1.2.3",
                ],
            ),
            (
                "boolean",
                &[
                    "true", "false", "1", "0", "TRUE", "False", "yes", "", " true ",
                ],
            ),
        ];

        for (ty, values) in cases {
            let schema = json!({ "type": "object", "properties": { "v": { "type": ty } } });
            for value in *values {
                let payload = format!(r#"{{"v":"{value}","other":"x"}}"#);
                assert_byte_identical(schema.clone(), &payload);
            }
        }
    }

    /// A value carrying escapes cannot be read from its span verbatim, so it has to fall
    /// back — and still agree.
    #[test]
    fn escaped_and_non_string_scalars_still_match() {
        for (ty, raw) in [
            // Both spell "42", but only after the escapes are decoded — the raw span
            // rewrite has to stand aside and let the general path unescape them.
            ("integer", r#""\u0034\u0032""#),
            ("integer", r#""4\u0032""#),
            ("number", r#""1.5""#),
            ("boolean", r#""true""#),
            // Already the right type, or a type no rewrite covers.
            ("integer", "42"),
            ("number", "1.5"),
            ("boolean", "true"),
            ("integer", "null"),
            ("integer", "[1]"),
            ("integer", r#"{"a":1}"#),
            ("string", r#""x""#),
            ("string", "42"),
        ] {
            let schema = json!({ "type": "object", "properties": { "v": { "type": ty } } });
            assert_byte_identical(schema, &format!(r#"{{"v":{raw},"other":"x"}}"#));
        }
    }

    /// The rewrite must stay out of the way of everything else a schema can declare.
    #[test]
    fn raw_scalar_coercion_defers_to_other_schema_keywords() {
        let payload = r#"{"v":"1","other":"x"}"#;
        for sub in [
            json!({ "type": "integer", "enum": [1, 2] }),
            json!({ "type": "integer", "enum": [7] }),
            json!({ "type": "boolean", "enum": [true] }),
            json!({ "type": "integer", "contentMediaType": "application/json" }),
        ] {
            let schema = json!({ "type": "object", "properties": { "v": sub } });
            assert_byte_identical(schema, payload);
        }

        // A sub-schema default takes the whole root off the fast path, because filling one
        // needs to know which fields are absent — which copying spans never learns.
        assert_same(
            json!({
                "type": "object",
                "properties": { "v": { "type": "integer", "default": 5 } }
            }),
            payload,
        );

        // `coerce: false` makes a mistyped field an error, which only `apply` phrases.
        assert_byte_identical_with(
            TransformMiddleware {
                schema: Some(json!({
                    "type": "object",
                    "properties": { "v": { "type": "integer" } }
                })),
                coerce: false,
                ..Default::default()
            },
            payload,
        );

        // An empty string is read as null first, so it belongs to the general path.
        assert_same_with(
            TransformMiddleware {
                schema: Some(json!({
                    "type": "object",
                    "properties": { "v": { "type": ["integer", "null"] } }
                })),
                coerce_empty_as_null: true,
                ..Default::default()
            },
            r#"{"v":"","other":"x"}"#,
        );
    }

    /// `transform_fast` writes keys itself instead of going through a `serde_json::Map`, so
    /// it consults `map_sorts_keys` to order them the way the normal path would. That probe
    /// has to describe the `Map` this build actually compiled in, whichever it is.
    #[test]
    fn map_sort_probe_matches_reality() {
        let mut map = Map::new();
        map.insert("b".to_string(), Value::from(1));
        map.insert("a".to_string(), Value::from(2));
        let serialized = serde_json::to_string(&Value::Object(map)).unwrap();
        assert_eq!(
            map_sorts_keys(),
            serialized.starts_with(r#"{"a""#),
            "map_sorts_keys disagrees with how this build's Map serialises: {serialized}"
        );
    }
    fn scalars() -> Value {
        json!({"type":"object","properties":{
        "s":{"type":"string"},
        "i":{"type":"integer"},
        "n":{"type":"number"},
        "b":{"type":"boolean"},
        "o":{"type":"object"},
        "a":{"type":"array"}}})
    }

    #[test]
    fn values_already_matching_their_type_are_untouched() {
        assert_same_via_fast(
            scalars(),
            r#"{"s":"x","i":42,"n":1.5,"b":true,"o":{"k":[1,2]},"a":[1,"two",null]}"#,
        );
    }

    #[test]
    fn every_coercion_agrees() {
        assert_same_via_fast(
            scalars(),
            r#"{"s":7,"i":"42","n":"1.5","b":"true","o":{},"a":[]}"#,
        );
        assert_same_via_fast(scalars(), r#"{"b":"0","i":"-8","n":"-2.5e3"}"#);
    }

    #[test]
    fn a_float_is_not_an_integer_even_though_it_starts_like_one() {
        // The byte check must not wave `1.5` or `1e3` through as integers.
        assert_same_via_fast(scalars(), r#"{"i":1.5}"#);
        assert_same_via_fast(scalars(), r#"{"i":1e3}"#);
        assert_same_via_fast(scalars(), r#"{"i":-0.0}"#);
    }

    #[test]
    fn coercion_failures_agree() {
        assert_same_via_fast(scalars(), r#"{"i":"not-a-number"}"#);
        assert_same_via_fast(scalars(), r#"{"b":"maybe"}"#);
        assert_same_via_fast(scalars(), r#"{"i":{}}"#);
    }

    #[test]
    fn embedded_documents_agree() {
        let schema = json!({"type":"object","properties":{
        "p":{"type":"string","contentMediaType":"application/json"}}});
        assert_same_via_fast(schema.clone(), r#"{"p":"{\"a\":1,\"b\":[1,2]}"}"#);
        assert_same_via_fast(schema.clone(), r#"{"p":"[1,2,3]"}"#);
        assert_same_via_fast(schema.clone(), r#"{"p":"null"}"#);
        assert_same_via_fast(schema.clone(), r#"{"p":"\"just a string\""}"#);
        // Malformed embedded JSON must fail the same way on both paths.
        assert_same_via_fast(schema.clone(), r#"{"p":"{not json}"}"#);
        // Escapes inside the embedded document, including a surrogate pair.
        assert_same_via_fast(schema.clone(), r#"{"p":"{\"e\":\"a\\\"b\\nc\"}"}"#);
        assert_same_via_fast(schema, r#"{"p":"{\"e\":\"\\ud83d\\ude00\"}"}"#);
    }

    #[test]
    fn a_content_schema_still_validates_the_decoded_document() {
        let schema = json!({"type":"object","properties":{
        "p":{"type":"string","contentMediaType":"application/json",
             "contentSchema":{"type":"object","properties":{"n":{"type":"integer"}}}}}});
        assert_same(schema.clone(), r#"{"p":"{\"n\":\"5\"}"}"#);
        assert_same(schema, r#"{"p":"{\"n\":\"oops\"}"}"#);
    }

    #[test]
    fn nested_schemas_agree() {
        let schema = json!({"type":"object","properties":{
        "outer":{"type":"object","properties":{
            "inner":{"type":"integer"},
            "deep":{"type":"object","properties":{"x":{"type":"boolean"}}}}},
        "list":{"type":"array","items":{"type":"integer"}}}});
        assert_same_via_fast(
            schema.clone(),
            r#"{"outer":{"inner":"3","deep":{"x":"true"}},"list":["1","2"]}"#,
        );
        // A single violation is reported identically.
        assert_same_via_fast(schema, r#"{"outer":{"inner":"bad"},"list":["1","2"]}"#);
    }

    /// A documented, deliberate difference. The normal path looks for violations in
    /// schema order; the fast path finds them in the order the payload lists its fields,
    /// which is not the same when keys are not sorted. A message violating the schema in
    /// more than one place is rejected either way — only the field named in the error
    /// differs. Making these agree would mean transforming in schema order and emitting in
    /// payload order, i.e. buffering every field, which costs more than the diagnostic is
    /// worth. Asserted so it cannot change unnoticed.
    #[test]
    fn known_difference_which_violation_is_reported_when_several() {
        let schema = json!({"type":"object","properties":{
        "outer":{"type":"object","properties":{"inner":{"type":"integer"}}},
        "list":{"type":"array","items":{"type":"integer"}}}});
        let out = both(schema, r#"{"outer":{"inner":"bad"},"list":["1","x"]}"#);
        // Sorted keys visit `list` first; insertion order reaches `outer` first. Either
        // way the message is rejected — only the field named in the error differs.
        let sorted = Err("coercion:$.list[1]".to_string());
        let insertion = Err("coercion:$.outer.inner".to_string());
        // The normal path walks the schema's (sorted) properties, so it always names `list`.
        assert_eq!(out.slow, sorted);
        // The fast path follows the payload order this build's `Map` would use; the other
        // ordering is what a build with the opposite `preserve_order` setting produces.
        let (build_order, other_order) = if map_sorts_keys() {
            (&sorted, &insertion)
        } else {
            (&insertion, &sorted)
        };
        assert_eq!(&out.fast, build_order);
        assert_eq!(&out.fast_other_order, other_order);
    }

    #[test]
    fn enums_agree() {
        let schema = json!({"type":"object","properties":{
        "e":{"type":"string","enum":["a","b"]}}});
        assert_same_via_fast(schema.clone(), r#"{"e":"a"}"#);
        assert_same_via_fast(schema, r#"{"e":"z"}"#);
    }

    #[test]
    fn nullability_agrees() {
        let schema = json!({"type":"object","properties":{
        "n":{"type":["string","null"]},
        "s":{"type":"string"}}});
        assert_same_via_fast(schema.clone(), r#"{"n":null,"s":"x"}"#);
        // Null against a non-nullable field must fail identically.
        assert_same_via_fast(schema, r#"{"n":"x","s":null}"#);
    }

    #[test]
    fn fields_the_schema_never_mentions_are_carried_through() {
        assert_same_via_fast(
            scalars(),
            r#"{"s":"x","extra":{"deep":[1,{"k":"v"}]},"another":null}"#,
        );
    }

    #[test]
    fn string_escapes_and_unicode_survive_the_byte_copy() {
        assert_same_via_fast(
            scalars(),
            r#"{"s":"tab\there \"quoted\" \\ back / slash é 😀"}"#,
        );
    }

    #[test]
    fn shapes_that_must_fall_back_still_agree() {
        // Root-level `required` and defaults need to know what is absent.
        let required = json!({"type":"object","required":["a"],
                          "properties":{"a":{"type":"string"}}});
        assert_same(required.clone(), r#"{"a":"x"}"#);
        assert_same(required, r#"{"b":"x"}"#);

        let defaulted = json!({"type":"object","properties":{
        "a":{"type":"string","default":"filled"}}});
        assert_same(defaulted.clone(), r#"{"b":1}"#);
        assert_same(defaulted, r#"{"a":null}"#);

        // A non-object payload has no fields to walk.
        assert_same(scalars(), r#"[1,2,3]"#);
        assert_same(scalars(), r#""bare string""#);

        // Duplicate keys collapse in a `Value`; copying spans would emit both.
        assert_same(scalars(), r#"{"s":"first","s":"second"}"#);

        // An escaped key cannot be borrowed, so the fast path declines it. These are
        // genuinely escaped: a quote, a newline, and a \u sequence inside the key.
        assert_same(scalars(), r#"{"a\"b":1,"s":"x"}"#);
        assert_same(scalars(), r#"{"a\nb":1,"s":"x"}"#);
        assert_same(scalars(), r#"{"a\u0041b":1,"s":"x"}"#);
        // A key that looks like it could close the object or inject a field.
        assert_same(scalars(), r#"{"a\":1,\"injected":1}"#);
    }

    #[test]
    fn integers_beyond_i64_agree() {
        // A `Value` holds integers as i64/u64, so a 24-digit id is not an `integer` and
        // must be rejected identically rather than waved through on a byte check.
        assert_same(scalars(), r#"{"i":999999999999999999999999}"#);
        assert_same(scalars(), r#"{"i":-999999999999999999999999}"#);
        // Just past i64::MAX but still a u64: accepted by both.
        assert_same(scalars(), r#"{"i":9223372036854775808}"#);
        assert_same(scalars(), r#"{"i":18446744073709551615}"#);
    }

    #[test]
    fn numbers_beyond_f64_are_rejected_by_both() {
        // Both paths reject these; only the reported path differs, because the fast path
        // can name the offending field where a whole-payload parse cannot.
        for payload in [r#"{"n":1e400}"#, r#"{"n":-1e400}"#] {
            let out = both(scalars(), payload);
            assert!(out.slow.is_err(), "normal path accepted {payload}");
            assert!(out.fast.is_err(), "fast path accepted {payload}");
        }
    }

    /// A documented, deliberate difference. A number too large for f64 sitting in a field
    /// the schema never mentions is copied through as bytes, because the fast path never
    /// parses fields it has nothing to say about — where a whole-payload parse rejects the
    /// message. Catching this would mean parsing every field and giving up the entire
    /// point of the fast path. Asserted so it cannot change unnoticed.
    #[test]
    fn known_difference_unrepresentable_number_in_an_unmentioned_field() {
        let out = both(scalars(), r#"{"unmentioned":1e400}"#);
        assert!(out.slow.is_err(), "normal path used to reject this");
        assert_eq!(out.fast, Ok(r#"{"unmentioned":1e400}"#.to_string()));
    }

    #[test]
    fn byte_output_is_identical_for_ordinary_payloads() {
        assert_byte_identical(scalars(), r#"{"s":"x","i":"42","n":"1.5","b":"true"}"#);
        assert_byte_identical(scalars(), r#"{"z":1,"a":2,"m":{"nested":[1,2]},"s":7}"#);
        assert_byte_identical(scalars(), r#"{"s":"quote \" and back \\ slash é"}"#);
    }

    #[test]
    fn malformed_payloads_agree() {
        assert_same(scalars(), r#"{"s":}"#);
        assert_same(scalars(), r#"not json at all"#);
        assert_same(scalars(), r#""#);
    }

    #[test]
    fn a_root_schema_without_properties_is_still_consistent() {
        assert_same(json!({"type":"object"}), r#"{"anything":[1,2]}"#);
        assert_same(json!({}), r#"{"anything":[1,2]}"#);
    }

    /// `coerce_empty_as_null` makes `""` mean something other than itself, which is
    /// exactly the assumption `is_passthrough` and `is_plain_content_decode` make when
    /// they wave a string through untouched.
    #[test]
    fn empty_strings_agree_under_coerce_empty_as_null() {
        // No property-level `default` here: that alone rules the fast path out, and this
        // test is about the shortcut still being taken.
        let schema = json!({"type":"object","properties":{
            "plain":{"type":"string"},
            "loose":{},
            "nullable":{"type":"string","nullable":true},
            "doc":{"type":"string","contentMediaType":"application/json"},
            "listed":{"type":"string","enum":["a","b"]}}});
        let cases = [
            r#"{"plain":""}"#,
            r#"{"loose":""}"#,
            r#"{"nullable":""}"#,
            r#"{"doc":""}"#,
            r#"{"listed":""}"#,
            r#"{"unmentioned":""}"#,
            r#"{"nullable":"","plain":"kept"}"#,
        ];
        for payload in cases {
            let config = TransformMiddleware {
                schema: Some(schema.clone()),
                coerce_empty_as_null: true,
                ..Default::default()
            };
            let out = both_with(config, payload);
            assert!(
                out.eligible,
                "the fast path should stay available: {payload}"
            );
            assert_eq!(
                as_json(&out.slow),
                as_json(&out.fast),
                "paths disagree on payload: {payload}"
            );
            assert_eq!(
                as_json(&out.slow),
                as_json(&out.fast_other_order),
                "paths disagree under the opposite key ordering on payload: {payload}"
            );
        }
    }

    // --- Mapping-only projections ---

    fn projection(rules: &[(&str, &str)]) -> TransformMiddleware {
        TransformMiddleware {
            mapping: rules
                .iter()
                .map(|(out, path)| (out.to_string(), MappingRule::Path(path.to_string())))
                .collect(),
            ..Default::default()
        }
    }

    /// The benchmark's row: seven fields, four kept, and the one dropped field is the
    /// biggest — which the fast path must never parse.
    fn row() -> &'static str {
        concat!(
            r#"{"id":41,"first_name":"Ada","country":"UK","amount":12.5,"#,
            r#""created_at":"2026-08-06T10:00:00Z","active":true,"#,
            r#""attributes":"{\"tier\":\"gold\",\"tags\":[\"a\",\"b\"]}"}"#
        )
    }

    fn four_of_seven() -> TransformMiddleware {
        projection(&[
            ("id", "$.id"),
            ("first_name", "$.first_name"),
            ("country", "$.country"),
            ("amount", "$.amount"),
        ])
    }

    #[test]
    fn a_projection_takes_the_fast_path() {
        assert_byte_identical_with(four_of_seven(), row());
    }

    #[test]
    fn renamed_and_reordered_outputs_agree() {
        let config = projection(&[
            ("z_name", "$.first_name"),
            ("a_id", "$.id"),
            ("m_when", "$.created_at"),
        ]);
        assert_byte_identical_with(config, row());
    }

    #[test]
    fn a_projection_carries_whole_subtrees_through() {
        let payload = r#"{"keep":{"deep":[1,{"k":"v"}],"é":"😀"},"drop":[1,2,3]}"#;
        assert_byte_identical_with(projection(&[("keep", "$.keep")]), payload);
    }

    #[test]
    fn an_absent_optional_field_is_left_out() {
        assert_byte_identical_with(four_of_seven(), r#"{"id":1,"country":"UK"}"#);
        // Nothing matched at all: an empty object, not a failure.
        assert_byte_identical_with(four_of_seven(), r#"{"other":1}"#);
    }

    #[test]
    fn a_missing_required_field_fails_identically() {
        let config = TransformMiddleware {
            mapping: [
                (
                    "id".to_string(),
                    MappingRule::Detailed(DetailedMappingRule {
                        path: "$.id".to_string(),
                        default: None,
                        required: true,
                    }),
                ),
                ("country".to_string(), MappingRule::Path("$.country".into())),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_byte_identical_with(config.clone(), r#"{"id":1,"country":"UK"}"#);
        assert_same_with(config, r#"{"country":"UK"}"#);
    }

    #[test]
    fn a_duplicated_source_key_resolves_to_its_last_value() {
        // A `Value` parse collapses duplicates last-wins; picking spans has to match.
        assert_byte_identical_with(
            projection(&[("id", "$.id")]),
            r#"{"id":1,"other":9,"id":2}"#,
        );
    }

    #[test]
    fn projections_that_must_fall_back_still_agree() {
        // A nested output key, a source path below the top level, and a default: each
        // alone keeps the general path.
        assert_same_with(projection(&[("a.b", "$.id")]), row());
        assert_same_with(
            projection(&[("tier", "$.nested.tier")]),
            r#"{"nested":{"tier":"gold"}}"#,
        );
        assert_same_with(projection(&[("first", "$.list[0]")]), r#"{"list":[7,8]}"#);
        assert_same_with(
            TransformMiddleware {
                mapping: [(
                    "id".to_string(),
                    MappingRule::Detailed(DetailedMappingRule {
                        path: "$.id".to_string(),
                        default: Some(json!(0)),
                        required: false,
                    }),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            r#"{"country":"UK"}"#,
        );

        // A schema alongside the mapping: the coercion stage still has to run.
        let mut with_schema = four_of_seven();
        with_schema.schema = Some(json!({"type":"object","properties":{"id":{"type":"string"}}}));
        assert_same_with(with_schema, row());

        // Shapes the span walk cannot read.
        assert_same_with(four_of_seven(), r#"[1,2,3]"#);
        assert_same_with(four_of_seven(), r#""bare string""#);
        assert_same_with(four_of_seven(), r#"{"id":}"#);
        assert_same_with(four_of_seven(), r#"not json at all"#);
        // An escaped key cannot be borrowed, so the fast path declines the payload.
        assert_same_with(four_of_seven(), r#"{"a\"b":1,"id":2}"#);
        assert_same_with(four_of_seven(), r#"{"id":1,"country":"UK"}"#);
    }

    #[test]
    fn an_output_key_needing_escapes_is_written_correctly() {
        assert_byte_identical_with(projection(&[(r#"quote"and\slash"#, "$.id")]), r#"{"id":1}"#);
    }

    /// A documented, deliberate difference, and the same one `transform_fast` already
    /// carries: a number too large for f64 is copied through as bytes, because a
    /// projection never parses the fields it moves — where a whole-payload parse rejects
    /// the message. Asserted so it cannot change unnoticed.
    #[test]
    fn known_difference_unrepresentable_number_in_a_projected_payload() {
        for payload in [r#"{"id":1e400}"#, r#"{"id":1,"dropped":1e400}"#] {
            let out = both_with(four_of_seven(), payload);
            assert!(out.slow.is_err(), "normal path used to reject {payload}");
            assert!(out.fast.is_ok(), "fast path rejected {payload}");
        }
    }

    /// The general path moves picked values out of the input instead of cloning them, but
    /// only when no rule reads through another's path. These overlap, so it must clone.
    #[test]
    fn overlapping_source_paths_still_see_the_whole_input() {
        let payload = r#"{"a":{"b":1,"c":2}}"#;
        let out = both_with(projection(&[("whole", "$.a"), ("part", "$.a.b")]), payload);
        assert_eq!(
            as_json(&out.slow),
            Ok(json!({"whole":{"b":1,"c":2},"part":1}))
        );
    }

    /// Disjoint deep paths do get moved rather than cloned; the result must not change.
    #[test]
    fn disjoint_deep_paths_agree_when_moved() {
        let payload = r#"{"a":{"b":[1,2],"c":{"d":"x"}},"e":9}"#;
        let out = both_with(
            projection(&[("b", "$.a.b"), ("d", "$.a.c.d"), ("e", "$.e")]),
            payload,
        );
        assert_eq!(as_json(&out.slow), Ok(json!({"b":[1,2],"d":"x","e":9})));
    }
}
