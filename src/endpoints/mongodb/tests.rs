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
fn encode_id_tags_supported_types() {
    let oid = mongodb::bson::oid::ObjectId::new();
    let uuid = mongodb::bson::Uuid::new();
    let cases = [
        (Bson::ObjectId(oid), format!("oid:{}", oid.to_hex())),
        (Bson::from(uuid), format!("uuid:{uuid}")),
        (Bson::Int64(123), "int:123".to_string()),
        (Bson::Int32(7), "int:7".to_string()),
        (Bson::String("k1".to_string()), "str:k1".to_string()),
    ];
    for (id, expected) in cases {
        assert_eq!(encode_id(&id), Some(expected));
    }
    assert_eq!(encode_id(&Bson::Boolean(true)), None);
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

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[cfg(feature = "dedup")]
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_dedup_store_states_release_and_replies() {
    use crate::middleware::deduplication::{Reservation, PENDING_TTL_SECS};
    let collection = format!("dedup_states_{}", fast_uuid_v7::gen_id());
    let store = build_mongo_dedup_store(
        "mongodb://localhost:27017",
        "mq_bridge_test",
        Some(collection),
        60,
        "states",
    )
    .await
    .unwrap();
    let now = 1_000;

    assert_eq!(
        store.reserve(b"k", now).await.unwrap(),
        Reservation::Claimed
    );
    assert_eq!(
        store.reserve(b"k", now).await.unwrap(),
        Reservation::InFlight
    );
    store.release(b"k").await;
    assert_eq!(
        store.reserve(b"k", now).await.unwrap(),
        Reservation::Claimed
    );

    store
        .mark_processed_with_response(b"k", now, b"reply")
        .await;
    assert_eq!(
        store.reserve(b"k", now).await.unwrap(),
        Reservation::Processed
    );
    assert_eq!(
        store.stored_response(b"k").await.as_deref(),
        Some(&b"reply"[..])
    );
    store.release(b"k").await;
    assert_eq!(
        store.reserve(b"k", now).await.unwrap(),
        Reservation::Processed,
        "release only drops an in-flight claim"
    );

    // Expired, reclaimed and plainly acked: the old reply must not be replayed.
    assert_eq!(
        store.reserve(b"k", now + 61).await.unwrap(),
        Reservation::Claimed
    );
    store.mark_processed(b"k", now + 61).await;
    assert_eq!(store.stored_response(b"k").await, None);

    // A crashed holder's claim lapses after the lease.
    assert_eq!(
        store.reserve(b"c", now).await.unwrap(),
        Reservation::Claimed
    );
    assert_eq!(
        store.reserve(b"c", now + PENDING_TTL_SECS).await.unwrap(),
        Reservation::Claimed
    );
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[cfg(feature = "dedup")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_dedup_store_batches_match_the_per_key_states() {
    use crate::middleware::deduplication::Reservation::{Claimed, InFlight, Processed};
    let store = build_mongo_dedup_store(
        "mongodb://localhost:27017",
        "mq_bridge_test",
        Some(format!("dedup_batch_{}", fast_uuid_v7::gen_id())),
        60,
        "batch",
    )
    .await
    .unwrap();
    let now = 1_000u64;
    let keys = |names: &[&str]| {
        names
            .iter()
            .map(|n| n.as_bytes().to_vec())
            .collect::<Vec<_>>()
    };

    store.reserve(b"held", now).await.unwrap();
    store.reserve(b"done", now).await.unwrap();
    store.mark_processed(b"done", now).await;
    store.reserve(b"old", now - 100).await.unwrap();
    store.mark_processed(b"old", now - 100).await;

    let batch = keys(&["new1", "held", "done", "old", "new2"]);
    assert_eq!(
        store.reserve_many(&batch, now).await.unwrap(),
        vec![Claimed, InFlight, Processed, Claimed, Claimed]
    );

    store.release_many(&keys(&["new1", "done", "new2"])).await;
    assert_eq!(
        store
            .reserve_many(&keys(&["new1", "done"]), now)
            .await
            .unwrap(),
        vec![Claimed, Processed]
    );
    store
        .mark_processed_many(&keys(&["new1", "new2"]), now)
        .await;
    assert_eq!(
        store
            .reserve_many(&keys(&["new1", "new2"]), now)
            .await
            .unwrap(),
        vec![Processed, Processed]
    );

    let contested: Vec<Vec<u8>> = (0..200).map(|i| format!("race{i}").into_bytes()).collect();
    let (a, b) = tokio::join!(
        store.reserve_many(&contested, now),
        store.reserve_many(&contested, now)
    );
    for (a, b) in a.unwrap().into_iter().zip(b.unwrap()) {
        assert!(
            matches!((a, b), (Claimed, InFlight) | (InFlight, Claimed)),
            "{a:?} / {b:?}"
        );
    }
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_find_answers_with_the_first_match() {
    let collection = format!("find_{}", fast_uuid_v7::gen_id());
    let config = |find: Option<&str>| MongoDbConfig {
        url: "mongodb://localhost:27017".to_string(),
        database: "mq_bridge_test".to_string(),
        collection: Some(collection.clone()),
        format: MongoDbFormat::Raw,
        id_field: Some("id".to_string()),
        find: find.map(str::to_string),
        ..Default::default()
    };
    let writer = MongoDbPublisher::new(&config(None)).await.unwrap();
    writer
        .send(CanonicalMessage::new(
            br#"{"id":"u1","name":"Ada"}"#.to_vec(),
            None,
        ))
        .await
        .unwrap();

    let finder = MongoDbPublisher::new(&config(Some(r#"{"_id": "${payload:user_id}"}"#)))
        .await
        .unwrap();
    let ask =
        |id: &str| CanonicalMessage::new(format!(r#"{{"user_id":"{id}"}}"#).into_bytes(), None);

    let Sent::Response(hit) = finder.send(ask("u1")).await.unwrap() else {
        panic!("find must answer with a response");
    };
    let doc: serde_json::Value = serde_json::from_slice(&hit.payload).unwrap();
    assert_eq!(doc["name"], "Ada");
    assert_eq!(
        hit.metadata.get("mongodb.found").map(String::as_str),
        Some("true")
    );

    let Sent::Response(miss) = finder.send(ask("nobody")).await.unwrap() else {
        panic!("find must answer with a response");
    };
    assert!(miss.payload.is_empty());
    assert_eq!(
        miss.metadata.get("mongodb.found").map(String::as_str),
        Some("false")
    );

    // Batched `$in`: `name` is not unique, so each key keeps only its first document.
    writer
        .send(CanonicalMessage::new(
            br#"{"id":"u2","name":"Ada"}"#.to_vec(),
            None,
        ))
        .await
        .unwrap();
    let batched = MongoDbPublisher::new(&config(Some(r#"{"name": {"$in": ["${payload:name}"]}}"#)))
        .await
        .unwrap();
    let by_name =
        |n: &str| CanonicalMessage::new(format!(r#"{{"name":"{n}"}}"#).into_bytes(), None);
    let answers = batched
        .lookup_batch(&[by_name("Ada"), by_name("Grace"), by_name("Ada")])
        .await
        .expect("an `$in` filter answers in batch")
        .unwrap();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0].as_ref().unwrap()["name"], "Ada");
    assert_eq!(answers[1], None);
    assert_eq!(answers[0], answers[2]);
}

#[test]
fn update_accepts_a_document_or_a_pipeline() {
    use mongodb::options::UpdateModifications;
    assert!(matches!(
        publisher::update_modifications(br#"{"$inc": {"c": 1}}"#),
        Ok(UpdateModifications::Document(_))
    ));
    assert!(matches!(
        publisher::update_modifications(br#"[{"$set": {"c": 1}}]"#),
        Ok(UpdateModifications::Pipeline(p)) if p.len() == 1
    ));
    assert!(publisher::update_modifications(b"42").is_err());
    assert!(publisher::update_modifications(b"[1]").is_err());
}

fn counter_config(collection: &str, find: &str, update: Option<&str>) -> MongoDbConfig {
    MongoDbConfig {
        url: "mongodb://localhost:27017".to_string(),
        database: "mq_bridge_test".to_string(),
        collection: Some(collection.to_string()),
        find: Some(find.to_string()),
        update: update.map(str::to_string),
        // A shared client may belong to another test's runtime, already shut down.
        shared: Some(false),
        ..Default::default()
    }
}

fn counter(answer: Option<serde_json::Value>) -> f64 {
    answer.expect("update always answers")["c"]
        .as_f64()
        .unwrap()
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_rejects_a_config_without_one_key_per_message() {
    let mut config = counter_config("unused", r#"{"_id": "${payload:k}"}"#, Some("{}"));
    config.find = None;
    assert!(MongoDbPublisher::new(&config).await.is_err());
    let config = counter_config(
        "unused",
        r#"{"_id": {"$in": ["${payload:k}"]}}"#,
        Some(r#"{"$inc": {"c": 1}}"#),
    );
    assert!(MongoDbPublisher::new(&config).await.is_err());
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_upserts_and_answers_with_the_new_document() {
    let collection = format!("update_{}", fast_uuid_v7::gen_id());
    let publisher = MongoDbPublisher::new(&counter_config(
        &collection,
        r#"{"_id": "${payload:k}"}"#,
        Some(r#"{"$inc": {"c": 1}}"#),
    ))
    .await
    .unwrap();
    for expected in [1.0, 2.0] {
        let Sent::Response(r) = publisher
            .send(CanonicalMessage::new(br#"{"k":"a"}"#.to_vec(), None))
            .await
            .unwrap()
        else {
            panic!("update must answer with a response");
        };
        let doc: serde_json::Value = serde_json::from_slice(&r.payload).unwrap();
        assert_eq!(counter(Some(doc)), expected);
        assert_eq!(
            r.metadata.get("mongodb.found").map(String::as_str),
            Some("true")
        );
    }
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_batch_keeps_per_key_order_and_skips_seen_ids() {
    let collection = format!("update_{}", fast_uuid_v7::gen_id());
    // Counts each txn id once; `recent` remembers the last 64 ids per key.
    let update = r#"[
        {"$set": {"_seen": {"$in": ["${payload:txn}", {"$ifNull": ["$recent", []]}]}}},
        {"$set": {
            "c": {"$cond": ["$_seen", "$c", {"$add": [{"$ifNull": ["$c", 0]}, 1]}]},
            "recent": {"$cond": ["$_seen", "$recent",
                {"$slice": [{"$concatArrays": [{"$ifNull": ["$recent", []]}, ["${payload:txn}"]]}, -64]}]}
        }},
        {"$unset": "_seen"}
    ]"#;
    let publisher = MongoDbPublisher::new(&counter_config(
        &collection,
        r#"{"_id": "${payload:k}"}"#,
        Some(update),
    ))
    .await
    .unwrap();
    let msg = |k: &str, txn: &str| {
        CanonicalMessage::new(format!(r#"{{"k":"{k}","txn":"{txn}"}}"#).into_bytes(), None)
    };
    let batch = vec![
        msg("a", "t1"),
        msg("a", "t2"),
        msg("b", "t3"),
        msg("a", "t4"),
        msg("a", "t2"),
        CanonicalMessage::new(br#"{"txn":"t5"}"#.to_vec(), None),
    ];
    let mut answers = publisher
        .lookup_batch(&batch)
        .await
        .expect("update answers batches")
        .unwrap();
    assert_eq!(
        answers.pop(),
        Some(None),
        "a keyless message writes nothing"
    );
    let counts: Vec<f64> = answers.into_iter().map(counter).collect();
    assert_eq!(counts, vec![1.0, 2.0, 1.0, 3.0, 3.0]);
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_rejected_message_fails_alone() {
    let collection = format!("update_{}", fast_uuid_v7::gen_id());
    let update =
        r#"[{"$set": {"c": {"$add": [{"$ifNull": ["$c", 0]}, {"$toDouble": "${payload:n}"}]}}}]"#;
    let publisher = MongoDbPublisher::new(&counter_config(
        &collection,
        r#"{"_id": "${payload:k}"}"#,
        Some(update),
    ))
    .await
    .unwrap();
    let msg = |k: &str, n: &str| {
        CanonicalMessage::new(format!(r#"{{"k":"{k}","n":"{n}"}}"#).into_bytes(), None)
    };

    let err = publisher.send(msg("a", "abc")).await.unwrap_err();
    assert!(
        matches!(err, PublisherError::NonRetryable(_)),
        "server rejection is not retryable: {err:?}"
    );

    let answers = publisher
        .lookup_batch(&[msg("a", "1"), msg("a", "abc"), msg("a", "2"), msg("b", "5")])
        .await
        .expect("update answers batches")
        .unwrap();
    assert_eq!(
        answers[1], None,
        "the rejected message is answered as not found"
    );
    let counts: Vec<f64> = [0, 2, 3].map(|i| counter(answers[i].clone())).to_vec();
    assert_eq!(counts, vec![1.0, 3.0, 5.0]);
}

#[test]
fn chainable_steps_rewrite_operators_and_refuse_what_cannot_chain() {
    use mongodb::bson::doc;
    use mongodb::options::UpdateModifications::{Document as Ops, Pipeline as Steps};
    let chain = |u| publisher::chainable_steps(u, "_mqb");

    let (steps, touched) =
        chain(Ops(doc! { "$inc": { "c": 1 }, "$unset": { "old": "" } })).unwrap();
    assert_eq!(steps.len(), 2);
    assert!(steps[0].contains_key("$set") && steps[1].contains_key("$unset"));
    assert_eq!(touched, ["c", "old"]);
    let (_, touched) = chain(Steps(vec![doc! { "$set": { "s.sum": 1, "t": 2 } }])).unwrap();
    assert_eq!(touched, ["s", "t"]);
    assert!(chain(Steps(vec![
        doc! { "$set": { "a": 1 } },
        doc! { "$unset": ["b"] }
    ]))
    .is_some());

    for refused in [
        Ops(doc! { "$setOnInsert": { "a": 1 } }),
        Ops(doc! { "$push": { "a": 1 } }),
        Ops(doc! { "$set": { "a": 1 }, "$inc": { "a.b": 1 } }),
        Ops(doc! { "$set": { "items.0": 1 } }),
        Ops(doc! { "$inc": { "s.sum": 1 } }),
        Ops(doc! { "$set": { "items.$": 1 } }),
        Ops(doc! {}),
        Steps(vec![doc! { "$replaceWith": { "a": 1 } }]),
        Steps(vec![doc! { "$set": { "_mqb": 1 } }]),
        Steps(vec![doc! { "$unset": ["x", "_mqb.y"] }]),
    ] {
        assert!(chain(refused.clone()).is_none(), "{refused:?}");
    }
}

#[test]
fn filter_reading_a_written_field_is_not_folded() {
    use mongodb::bson::doc;
    let touched = ["balance".to_string()];
    for reads in [
        doc! { "_id": "k", "balance": { "$gte": 10 } },
        doc! { "_id": "k", "balance.available": { "$gte": 10 } },
        doc! { "$and": [{ "_id": "k" }, { "balance": { "$gte": 10 } }] },
        doc! { "_id": "k", "$expr": { "$gte": ["$balance", 10] } },
        doc! { "_id": "k", "$where": "this.balance >= 10" },
        doc! { "_id": "k", "$jsonSchema": { "required": ["balance"] } },
        doc! { "$and": [{ "_id": "k" }, { "$jsonSchema": { "required": ["balance"] } }] },
    ] {
        assert!(publisher::filter_reads(&reads, &touched), "{reads:?}");
    }
    for key_only in [doc! { "_id": "k" }, doc! { "_id": "balance", "tenant": 1 }] {
        assert!(
            !publisher::filter_reads(&key_only, &touched),
            "{key_only:?}"
        );
    }
}

#[test]
fn folded_result_splits_into_one_document_per_message() {
    use mongodb::bson::doc;
    let touched = ["c".to_string(), "gone".to_string()];
    let update = publisher::folded_update(
        "_mqb",
        &touched,
        vec![
            vec![doc! { "$set": { "c": 1 } }],
            vec![doc! { "$set": { "c": 2 } }, doc! { "$unset": "gone" }],
        ],
    );
    assert_eq!(update.len(), 5, "unset, step, snapshot, two steps");
    assert_eq!(
        update[2],
        doc! { "$set": { "_mqb": { "$concatArrays": [
            { "$ifNull": ["$_mqb", []] }, [{ "c": "$c", "gone": "$gone" }]
        ] } } }
    );

    let doc = doc! { "_id": "a", "c": 2, "k": 0, "_mqb": [{ "c": 1, "gone": true }, {}] };
    let docs = publisher::unfold(doc.clone(), "_mqb", &touched, 3).unwrap();
    assert_eq!(
        docs,
        vec![
            doc! { "_id": "a", "c": 1, "k": 0, "gone": true },
            doc! { "_id": "a", "k": 0 },
            doc! { "_id": "a", "c": 2, "k": 0 },
        ]
    );
    assert!(publisher::unfold(doc, "_mqb", &touched, 2).is_none());
    assert_eq!(
        publisher::unfold(doc! { "_id": "a" }, "_mqb", &touched, 1).unwrap(),
        vec![doc! { "_id": "a" }]
    );
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_batch_field_is_validated() {
    let find = r#"{"_id": "${payload:k}"}"#;
    let mut config = counter_config("unused", find, None);
    config.update_batch_field = Some("_mqb".to_string());
    assert!(MongoDbPublisher::new(&config).await.is_err());
    for bad in ["", "a.b", "$x", "total", "_mqb.x"] {
        let mut config = counter_config("unused", find, Some(r#"{"$inc": {"c": 1}}"#));
        config.update_batch_field = Some(bad.to_string());
        assert!(MongoDbPublisher::new(&config).await.is_err(), "{bad:?}");
    }
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_batch_field_answers_like_one_call_per_message() {
    let ema = r#"[{"$set": {
        "ema": {"$add": [{"$multiply": [0.5, ${payload:x}]},
                         {"$multiply": [0.5, {"$ifNull": ["$ema", ${payload:x}]}]}]},
        "n": {"$add": [{"$ifNull": ["$n", 0]}, 1]}
    }}]"#;
    let ops = r#"{"$inc": {"c": 1, "s.sum": ${payload:x}}, "$mul": {"m": 2},
        "$min": {"lo": ${payload:x}}, "$max": {"hi": ${payload:x}},
        "$set": {"last": {"x": ${payload:x}, "tag": "$not-a-field"}}, "$unset": {"gone": ""}}"#;
    let msg = |k: &str, x: i64| {
        CanonicalMessage::new(format!(r#"{{"k":"{k}","x":{x}}}"#).into_bytes(), None)
    };
    let batches = vec![
        vec![
            msg("a", 10),
            msg("a", 20),
            msg("b", 5),
            msg("a", 30),
            msg("b", 1),
        ],
        vec![msg("a", 40)],
        (0..70).map(|i| msg("c", i % 7)).collect::<Vec<_>>(),
    ];
    let db = mongodb::Client::with_uri_str("mongodb://localhost:27017")
        .await
        .unwrap()
        .database("mq_bridge_test");
    for update in [ema, ops] {
        let folded_name = format!("update_{}", fast_uuid_v7::gen_id());
        let stored = db.collection::<mongodb::bson::Document>(&folded_name);
        let folded_name = &folded_name;
        let publisher = |folded: bool| async move {
            let name = if folded {
                folded_name.clone()
            } else {
                format!("update_{}", fast_uuid_v7::gen_id())
            };
            let mut config = counter_config(&name, r#"{"_id": "${payload:k}"}"#, Some(update));
            config.update_batch_field = folded.then(|| "_mqb".to_string());
            MongoDbPublisher::new(&config).await.unwrap()
        };
        let (plain, folded) = (publisher(false).await, publisher(true).await);
        for batch in &batches {
            let expected = plain.lookup_batch(batch).await.unwrap().unwrap();
            let answers = folded.lookup_batch(batch).await.unwrap().unwrap();
            assert_eq!(answers, expected, "update: {update}");
            assert!(answers
                .iter()
                .all(|a| a.as_ref().unwrap().get("_mqb").is_none()));
            let leftover = stored
                .count_documents(doc! { "_mqb": { "$exists": true } })
                .await
                .unwrap();
            assert_eq!(leftover, 0, "snapshots must not outlive their batch");
        }
        let Sent::Response(single) = folded.send(msg("a", 50)).await.unwrap() else {
            panic!("update must answer with a response");
        };
        let Sent::Response(expected) = plain.send(msg("a", 50)).await.unwrap() else {
            panic!("update must answer with a response");
        };
        // Operator updates add new fields sorted, update lists in order: compare values only.
        let json = |p: &[u8]| serde_json::from_slice::<serde_json::Value>(p).unwrap();
        assert_eq!(json(&single.payload), json(&expected.payload));
    }
}

/// Needs `tests/integration/docker-compose/mongodb.yml` running.
#[tokio::test]
#[ignore = "requires a MongoDB on localhost:27017"]
async fn mongo_update_batch_field_rejected_message_fails_alone() {
    let update =
        r#"[{"$set": {"c": {"$add": [{"$ifNull": ["$c", 0]}, {"$toDouble": "${payload:n}"}]}}}]"#;
    let mut config = counter_config(
        &format!("update_{}", fast_uuid_v7::gen_id()),
        r#"{"_id": "${payload:k}"}"#,
        Some(update),
    );
    config.update_batch_field = Some("_mqb".to_string());
    let publisher = MongoDbPublisher::new(&config).await.unwrap();
    let msg = |k: &str, n: &str| {
        CanonicalMessage::new(format!(r#"{{"k":"{k}","n":"{n}"}}"#).into_bytes(), None)
    };
    assert!(matches!(
        publisher.send(msg("a", "abc")).await.unwrap_err(),
        PublisherError::NonRetryable(_)
    ));
    let answers = publisher
        .lookup_batch(&[msg("a", "1"), msg("a", "abc"), msg("a", "2"), msg("b", "5")])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answers[1], None);
    let counts: Vec<f64> = [0, 2, 3].map(|i| counter(answers[i].clone())).to_vec();
    assert_eq!(counts, vec![1.0, 3.0, 5.0]);

    // A folded write leaves snapshots behind; the per-message redo must not answer with them.
    publisher
        .lookup_batch(&[msg("a", "1"), msg("a", "1")])
        .await
        .unwrap()
        .unwrap();
    let answers = publisher
        .lookup_batch(&[msg("a", "1"), msg("a", "abc")])
        .await
        .unwrap()
        .unwrap();
    assert!(answers[0].as_ref().unwrap().get("_mqb").is_none());
}

/// The client connects lazily, so these refusals need no server.
#[tokio::test]
async fn readers_refuse_a_bad_config_before_touching_the_server() {
    let config = |collection: Option<&str>| MongoDbConfig {
        url: "mongodb://127.0.0.1:1".to_string(),
        database: "d".to_string(),
        collection: collection.map(str::to_string),
        shared: Some(false),
        ..Default::default()
    };
    let refusal = |result: anyhow::Result<()>| format!("{:#}", result.unwrap_err());

    let no_collection = config(None);
    let snapshot = super::readers::MongoDbIdReader::new(&no_collection).await;
    assert!(refusal(snapshot.map(drop)).contains("Collection name is required"));
    let cdc = super::readers::MongoDbChangeStreamReader::new(&no_collection, true).await;
    assert!(refusal(cdc.map(drop)).contains("Collection name is required"));

    let mut bad_query = config(Some("c"));
    bad_query.receive_query = Some("{not json".to_string());
    let snapshot = super::readers::MongoDbIdReader::new(&bad_query).await;
    assert!(refusal(snapshot.map(drop)).contains("receive_query"));
    let cdc = super::readers::MongoDbChangeStreamReader::new(&bad_query, false).await;
    assert!(refusal(cdc.map(drop)).contains("receive_query"));

    // A stored `_id` position would skip documents a concurrent writer commits below it.
    let mut resumed = config(Some("c"));
    resumed.cursor_id = Some("pos".to_string());
    let snapshot = super::readers::MongoDbIdReader::new(&resumed).await;
    assert!(refusal(snapshot.map(drop)).contains("does not support 'cursor_id'"));
}
