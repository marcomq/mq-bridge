//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;
use crate::CanonicalMessage;

#[test]
fn change_event_source_metadata_is_opt_in_and_orders_by_cluster_time() {
    use mongodb::bson::{doc, Timestamp};
    use mongodb::change_stream::event::ChangeStreamEvent;

    let event: ChangeStreamEvent<mongodb::bson::Document> = mongodb::bson::from_document(doc! {
        "_id": { "_data": "826553F1A0000000012B02" },
        "operationType": "insert",
        "ns": { "db": "shop", "coll": "orders" },
        "documentKey": { "_id": 7 },
        "fullDocument": { "_id": 7, "total": 42 },
        "clusterTime": Timestamp { time: 1_700_000_000, increment: 3 },
    })
    .expect("change event deserializes");

    let ordinals = std::sync::Mutex::new(super::readers::OrdinalCounter::default());
    let off = super::readers::MongoDbChangeStreamReader::event_to_message(
        &event,
        false,
        "shop.orders",
        &ordinals,
    )
    .expect("event carries a payload");
    assert!(!off
        .metadata
        .keys()
        .any(|key| crate::canonical_message::is_source_metadata_key(key)));

    let on = super::readers::MongoDbChangeStreamReader::event_to_message(
        &event,
        true,
        "shop.orders",
        &ordinals,
    )
    .expect("event carries a payload");
    assert_eq!(
        on.metadata
            .get("mqb.src.mongodb_namespace")
            .map(String::as_str),
        Some("shop.orders")
    );
    // (seconds << 32) | increment — the server's own oplog ordering.
    assert_eq!(
        on.metadata
            .get("mqb.src.mongodb_cluster_time")
            .map(String::as_str),
        Some(((1_700_000_000u64 << 32) | 3).to_string()).as_deref()
    );
    assert!(on.metadata.contains_key("mqb.src.mongodb_resume_token"));
    // First change seen at this cluster time.
    assert_eq!(
        on.metadata
            .get("mqb.src.mongodb_ordinal")
            .map(String::as_str),
        Some("0")
    );

    // A second change in the same transaction gets the next ordinal, so an idempotent
    // sink can name them as one contiguous range instead of colliding.
    let next = super::readers::MongoDbChangeStreamReader::event_to_message(
        &event,
        true,
        "shop.orders",
        &ordinals,
    )
    .expect("event carries a payload");
    assert_eq!(
        next.metadata
            .get("mqb.src.mongodb_ordinal")
            .map(String::as_str),
        Some("1")
    );
}

#[test]
fn snapshot_source_metadata_uses_namespace_and_document_id() {
    let mut message = CanonicalMessage::new(Vec::new(), None);
    super::readers::add_snapshot_source_metadata(
        &mut message,
        "shop.orders",
        &mongodb::bson::Bson::String("order-42".into()),
        &std::sync::Mutex::new(super::readers::OrdinalCounter::default()),
    );

    assert_eq!(
        message
            .metadata
            .get("mqb.src.mongodb_namespace")
            .map(String::as_str),
        Some("shop.orders")
    );
    assert_eq!(
        message
            .metadata
            .get("mqb.src.mongodb_document_id")
            .map(String::as_str),
        Some("\"order-42\"")
    );
    assert_eq!(
        message
            .metadata
            .get("mqb.src.mongodb_snapshot_index")
            .map(String::as_str),
        Some("0")
    );
}

#[test]
fn parse_document_takes_wrapped_fields_and_falls_back_otherwise() {
    let id = mongodb::bson::Uuid::new();
    // Wrapped: payload is unwrapped and metadata decoded.
    let msg = parse_mongodb_document(doc! {
        "_id": id, "payload": "hello", "metadata": { "kind": "greeting" }
    })
    .expect("wrapped document parses");
    assert_eq!(msg.payload.as_ref(), b"hello");
    assert_eq!(
        msg.metadata.get("kind").map(String::as_str),
        Some("greeting")
    );
    assert_eq!(msg.message_id, u128::from_be_bytes(id.bytes()));

    // Foreign document (no `payload`): serialized whole, marked raw.
    let raw =
        parse_mongodb_document(doc! { "_id": 7, "name": "ada" }).expect("foreign document parses");
    assert_eq!(
        raw.metadata
            .get("mq_bridge.original_format")
            .map(String::as_str),
        Some("raw")
    );
    assert!(serde_json::from_slice::<serde_json::Value>(&raw.payload).unwrap()["name"] == "ada");

    // Non-string metadata values still take the raw path, document intact.
    let mixed = parse_mongodb_document(doc! { "_id": id, "payload": "x", "metadata": { "n": 1 } })
        .expect("mixed-metadata document parses");
    let body: serde_json::Value = serde_json::from_slice(&mixed.payload).unwrap();
    assert_eq!(body["payload"], "x");
}

#[test]
fn resolved_consume_defaults_and_change_stream_alias() {
    use crate::models::{MongoConsume, MongoDbConfig};
    // Default: non-destructive capture of the existing collection, then changes.
    let cfg = MongoDbConfig::new("mongodb://localhost", "db");
    assert_eq!(cfg.resolved_consume(), MongoConsume::CaptureAll);
    // Deprecated `change_stream: true` (no `consume`) now maps to the change-stream reader that
    // replaced the removed subscriber mode.
    let mut legacy = MongoDbConfig::new("mongodb://localhost", "db");
    legacy.change_stream = true;
    assert_eq!(legacy.resolved_consume(), MongoConsume::CaptureNew);
    // Explicit `consume` wins over the deprecated boolean.
    let mut explicit = MongoDbConfig::new("mongodb://localhost", "db");
    explicit.change_stream = true;
    explicit.consume = Some(MongoConsume::CaptureAll);
    assert_eq!(explicit.resolved_consume(), MongoConsume::CaptureAll);
}

#[test]
fn full_document_match_prefixes_fields_and_preserves_operators() {
    // Plain field predicates (incl. field-level operators and dotted paths) get the
    // `fullDocument.` prefix; the operator value is left untouched.
    assert_eq!(
        full_document_match(&doc! { "type": "notification", "n": { "$gt": 5 } }),
        doc! { "fullDocument.type": "notification", "fullDocument.n": { "$gt": 5 } }
    );
    assert_eq!(
        full_document_match(&doc! { "address.city": "NYC" }),
        doc! { "fullDocument.address.city": "NYC" }
    );
    // Top-level logical operators are preserved and their nested predicates rewritten.
    assert_eq!(
        full_document_match(&doc! { "$or": [ { "a": 1 }, { "b": 2 } ] }),
        doc! { "$or": [ { "fullDocument.a": 1 }, { "fullDocument.b": 2 } ] }
    );
}

#[test]
fn resumable_encode_decode_roundtrips_supported_types() {
    let oid = mongodb::bson::oid::ObjectId::new();
    let uuid = mongodb::bson::Uuid::new();
    let cases = [
        Bson::ObjectId(oid),
        Bson::from(uuid),
        Bson::Int64(123),
        Bson::String("k1".to_string()),
    ];
    for id in cases {
        let encoded = encode_id(&id).expect("supported type encodes");
        assert_eq!(decode_id(&encoded), Some(id), "roundtrip for {}", encoded);
    }
    // Int32 encodes as an int and decodes back as Int64 (BSON `$gt` compares numerically).
    assert_eq!(encode_id(&Bson::Int32(7)).as_deref(), Some("int:7"));
    // Unsupported types are not persisted.
    assert_eq!(encode_id(&Bson::Boolean(true)), None);
    assert_eq!(decode_id("bogus"), None);
}

#[test]
fn resume_token_encode_decode_roundtrips() {
    // A resume token is an opaque `{ "_data": <hex string> }` document; build one directly.
    let token: ResumeToken =
        mongodb::bson::from_document(doc! { "_data": "826553F1A0000000012B02" })
            .expect("token deserializes");

    let encoded = encode_resume_token(&token).expect("token encodes");
    let decoded = decode_resume_token(&encoded).expect("token decodes");
    // Re-encoding the decoded token yields the same string (stable round-trip).
    assert_eq!(encode_resume_token(&decoded).unwrap(), encoded);
    // A malformed value decodes to None so the reader restarts cleanly instead of failing.
    assert!(decode_resume_token("not-json").is_none());
}

#[test]
fn message_to_document_strips_source_metadata_but_keeps_user_keys() {
    let mut msg = CanonicalMessage::new(b"hello".to_vec(), None);
    msg.metadata.insert("kind".to_string(), "order".to_string());
    msg.metadata
        .insert("mqb.src.kafka_offset".to_string(), "42".to_string());

    let doc = message_to_document(&msg, &MongoDbFormat::Text, None, None).unwrap();
    let metadata = doc.get_document("metadata").unwrap();

    assert_eq!(metadata.get_str("kind").unwrap(), "order");
    assert!(
        !metadata.contains_key("mqb.src.kafka_offset"),
        "source/provenance keys must not be persisted to the document"
    );
}

#[test]
fn message_to_document_id_field_sets_typed_id() {
    let msg = CanonicalMessage::new(br#"{"order_id":"A-1","qty":3}"#.to_vec(), None);
    let doc = message_to_document(&msg, &MongoDbFormat::Json, Some("order_id"), None).unwrap();
    assert_eq!(doc.get_str("_id").unwrap(), "A-1");

    // Numeric key keeps its BSON integer type.
    let msg = CanonicalMessage::new(br#"{"order_id":42}"#.to_vec(), None);
    let doc = message_to_document(&msg, &MongoDbFormat::Json, Some("order_id"), None).unwrap();
    assert_eq!(doc.get_i64("_id").unwrap(), 42);
}

#[test]
fn message_to_document_id_field_overrides_raw_id() {
    // Raw inserts the payload verbatim; id_field still wins.
    let msg = CanonicalMessage::new(br#"{"order_id":"A-1","_id":"ignored"}"#.to_vec(), None);
    let doc = message_to_document(&msg, &MongoDbFormat::Raw, Some("order_id"), None).unwrap();
    assert_eq!(doc.get_str("_id").unwrap(), "A-1");
}

#[test]
fn message_to_document_id_field_missing_or_non_json_errors() {
    for payload in [&br#"{"other":1}"#[..], b"not json", br#"{"order_id":null}"#] {
        let msg = CanonicalMessage::new(payload.to_vec(), None);
        assert!(message_to_document(&msg, &MongoDbFormat::Json, Some("order_id"), None).is_err());
    }
}

#[test]
fn message_to_document_id_template_uses_replay_stable_metadata() {
    let mut msg = CanonicalMessage::new(br#"{"order_id":"A-1"}"#.to_vec(), None);
    msg.metadata
        .insert("mqb.id".to_string(), "order-A-1".to_string());
    let template = CompiledTemplate::compile("${metadata:mqb.id}", None).unwrap();

    let doc = message_to_document(&msg, &MongoDbFormat::Json, None, Some(&template)).unwrap();

    assert_eq!(doc.get_str("_id").unwrap(), "order-A-1");
}

#[test]
fn message_to_document_id_template_must_fully_resolve() {
    let msg = CanonicalMessage::new(br#"{"order_id":"A-1"}"#.to_vec(), None);
    let template = CompiledTemplate::compile("${metadata:mqb.id}", None).unwrap();

    assert!(message_to_document(&msg, &MongoDbFormat::Json, None, Some(&template)).is_err());
}

#[test]
fn extract_id_bson_rejects_array_id() {
    // An array is not a valid MongoDB `_id`.
    assert!(extract_id_bson(br#"{"order_id":[1,2]}"#, "order_id").is_err());
}

#[test]
fn tag_outcome_off_returns_ack() {
    let msg = CanonicalMessage::new(b"x".to_vec(), None);
    assert!(matches!(
        tag_outcome(false, msg, OUTCOME_INSERTED),
        Sent::Ack
    ));
}

#[test]
fn tag_outcome_on_returns_tagged_response() {
    for outcome in [OUTCOME_INSERTED, OUTCOME_EXISTED] {
        let msg = CanonicalMessage::new(b"x".to_vec(), None);
        let id = msg.message_id;
        match tag_outcome(true, msg, outcome) {
            Sent::Response(m) => {
                assert_eq!(m.message_id, id);
                assert_eq!(
                    m.metadata.get(OUTCOME_KEY).map(String::as_str),
                    Some(outcome)
                );
            }
            Sent::Ack => panic!("expected Response when report_outcome is on"),
        }
    }
}
