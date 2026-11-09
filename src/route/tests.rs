//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

/// The publisher opens before the consumer, and a `source_position` file sink creates its
/// part directory as it opens. A route rejected for its input must not leave that behind
/// where the operator asked for a file.
#[tokio::test]
async fn a_rejected_source_position_file_sink_leaves_no_directory_behind() {
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("out.jsonl");
    let route = Route {
        input: Endpoint::new(EndpointType::Memory(MemoryConfig::new("probe-in", Some(1)))),
        output: Endpoint::new(EndpointType::File(
            FileConfig::new(sink.to_str().unwrap()).with_name_by(NameBy::SourcePosition),
        )),
        options: RouteOptions::default(),
    };
    let (_tx, rx) = async_channel::bounded(1);
    let error = route
        .run_sequentially("probe", rx, None, None, None, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("replay position"),
        "unexpected error: {error}"
    );
    assert!(
        !sink.is_dir(),
        "the failing route left {sink:?} behind as a directory"
    );
}
use super::checks::endpoint_tree_has_buffer;
use super::*;

#[test]
fn authentication_failures_are_told_apart_from_unreachable_hosts() {
    let auth = anyhow::anyhow!("connect")
        .context("error returned from database: password authentication failed for user \"app\"");
    assert!(is_authentication_failure(&auth));
    assert!(is_authentication_failure(&anyhow::anyhow!(
        "ACCESS_REFUSED - Login was refused"
    )));
    assert!(!is_authentication_failure(&anyhow::anyhow!(
        "Connection refused (os error 61)"
    )));
}

#[cfg(feature = "grpc")]
#[test]
fn a_grpc_status_is_an_authentication_failure_only_when_unauthenticated() {
    use crate::endpoints::grpc::GrpcStatusError;
    let status = |status: tonic::Status| anyhow::Error::new(GrpcStatusError::from(status));
    assert!(is_authentication_failure(&status(
        tonic::Status::unauthenticated("authentication failed")
    )));
    assert!(!is_authentication_failure(
        &status(tonic::Status::unavailable("upstream authentication failed")).context("send")
    ));
    assert!(is_authentication_failure(&status(
        tonic::Status::unavailable("ACCESS_REFUSED - Login was refused")
    )));
}
use crate::models::{
    Endpoint, EndpointType, FaultMode, FileConfig, MemoryConfig, Middleware, MongoDbConfig, NameBy,
    RandomPanicMiddleware, RouteOptions, SqlxConfig,
};
use crate::traits::{CustomMiddlewareFactory, MessageConsumer, MessagePublisher, ReceivedBatch};
use crate::CanonicalMessage;
use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn completed_route_clears_stale_connection_status() {
    for error in ["connecting", "temporary disconnect"] {
        let outcome = Arc::new(RwLock::new(None));
        let status = Arc::new(RwLock::new(EndpointStatus {
            healthy: false,
            error: Some(error.to_string()),
            ..Default::default()
        }));

        {
            let mut guard = OutcomeGuard {
                outcome: Arc::clone(&outcome),
                status: Arc::clone(&status),
                drops: Arc::new(RwLock::new(DropReport::default())),
                resolved: None,
            };
            guard.set(RouteOutcome::Completed);
        }

        let status = recover_read_lock(&status, "test_route_handle_status");
        assert!(status.healthy);
        assert_eq!(status.error, None);
        assert_eq!(
            *recover_read_lock(&outcome, "test_route_handle_outcome"),
            Some(RouteOutcome::Completed)
        );
    }
}

#[test]
fn infers_idempotent_sink_mechanisms() {
    let mongo = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new(EndpointType::MongoDb(MongoDbConfig {
            id_field: Some("${metadata:mqb.id}".to_string()),
            ..Default::default()
        })),
    );
    assert_eq!(
        mongo.inferred_idempotency_mechanism(&mongo.output),
        Some("MongoDB unique _id")
    );

    let sql = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new(EndpointType::Sqlx(SqlxConfig {
            insert_query: Some(
                "INSERT INTO orders (id) VALUES (?) ON CONFLICT (id) DO NOTHING".to_string(),
            ),
            ..Default::default()
        })),
    );
    assert_eq!(
        sql.inferred_idempotency_mechanism(&sql.output),
        Some("SQL unique-key conflict handling")
    );

    for query in [
        "INSERT INTO orders (id) VALUES (?) ON CONFLICT (id) DO UPDATE SET id = excluded.id",
        "INSERT INTO orders (id) VALUES (?) ON DUPLICATE KEY UPDATE id = VALUES(id)",
    ] {
        let updating_sql = Route::new(
            Endpoint::new_memory("delivery_in", 1),
            Endpoint::new(EndpointType::Sqlx(SqlxConfig {
                insert_query: Some(query.to_string()),
                ..Default::default()
            })),
        );
        assert_eq!(
            updating_sql.inferred_idempotency_mechanism(&updating_sql.output),
            None
        );
    }

    let ordinary = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new_memory("delivery_out", 1),
    );
    assert_eq!(
        ordinary.inferred_idempotency_mechanism(&ordinary.output),
        None
    );
}

/// A key built from source-position metadata only counts when the input stamps that family.
#[test]
fn a_position_key_is_idempotent_only_over_a_positional_input() {
    let keyed_on = |family: &str| {
        Endpoint::new(EndpointType::Sqlx(SqlxConfig {
            insert_query: Some(format!(
                "INSERT INTO t (p, o) VALUES (${{metadata:mqb.src.{family}_partition}}, \
                     ${{metadata:mqb.src.{family}_offset}}) ON CONFLICT (p, o) DO NOTHING"
            )),
            ..Default::default()
        }))
    };
    let file_input = || {
        Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
            "/tmp/orders.jsonl",
        )))
    };
    let positionless = Route::new(Endpoint::new_memory("delivery_in", 1), keyed_on("kafka"));
    assert_eq!(
        positionless.delivery_guarantee(),
        DeliveryGuarantee::AtLeastOnce
    );

    let foreign_family = Route::new(file_input(), keyed_on("kafka"));
    assert_eq!(
        foreign_family.delivery_guarantee(),
        DeliveryGuarantee::AtLeastOnce
    );

    let own_family = Route::new(file_input(), keyed_on("file"));
    assert_eq!(
        own_family.delivery_guarantee(),
        DeliveryGuarantee::EffectivelyOnce
    );
}

#[test]
fn a_ref_input_is_classified_by_what_it_resolves_to() {
    register_endpoint(
        "dg_ref_zeromq",
        Endpoint::new(EndpointType::ZeroMq(Default::default())),
    );
    let route = Route::new(
        Endpoint::new(EndpointType::Ref("dg_ref_zeromq".to_string())),
        Endpoint::new_memory("delivery_out", 1),
    );
    assert_eq!(route.delivery_guarantee(), DeliveryGuarantee::AtMostOnce);
}

#[test]
fn inputs_without_acknowledgement_are_at_most_once() {
    let zeromq = Route::new(
        Endpoint::new(EndpointType::ZeroMq(Default::default())),
        Endpoint::new_memory("delivery_out", 1),
    );
    assert_eq!(zeromq.delivery_guarantee(), DeliveryGuarantee::AtMostOnce);

    let core_nats = Route::new(
        Endpoint::new(EndpointType::Nats(crate::models::NatsConfig {
            no_jetstream: true,
            ..Default::default()
        })),
        Endpoint::new_memory("delivery_out", 1),
    );
    assert_eq!(
        core_nats.delivery_guarantee(),
        DeliveryGuarantee::AtMostOnce
    );

    let memory = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new_memory("delivery_out", 1),
    );
    assert_eq!(memory.delivery_guarantee(), DeliveryGuarantee::AtLeastOnce);
}

#[test]
fn a_required_guarantee_the_route_cannot_meet_fails_the_check() {
    let options =
        RouteOptions::default().with_required_delivery(DeliveryGuarantee::EffectivelyOnce);
    let plain = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new_memory("delivery_out", 1),
    )
    .with_options(options.clone());
    let err = plain.check("plain", None).unwrap_err().to_string();
    assert!(err.contains("requires effectively-once"), "{err}");

    let idempotent = Route::new(
        Endpoint::new_memory("delivery_in", 1),
        Endpoint::new(EndpointType::MongoDb(MongoDbConfig {
            id_field: Some("order_id".to_string()),
            ..Default::default()
        })),
    )
    .with_options(options);
    assert_eq!(
        idempotent.delivery_guarantee(),
        DeliveryGuarantee::EffectivelyOnce
    );
}

/// A custom endpoint takes part through its factory, by override or by schema annotation.
#[test]
fn custom_endpoints_declare_their_delivery() {
    #[derive(Debug)]
    struct KeyedSink;
    impl crate::traits::CustomEndpointFactory for KeyedSink {
        fn idempotent_sink(&self, config: &serde_json::Value) -> bool {
            config.get("key").is_some()
        }
    }
    #[derive(Debug)]
    struct FireAndForget;
    impl crate::traits::CustomEndpointFactory for FireAndForget {
        fn config_schema(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "type": "object", "x-mqb-acknowledges": false }))
        }
    }
    let _ = crate::extensions::register_endpoint_factory("dg_keyed_sink", Arc::new(KeyedSink));
    let _ =
        crate::extensions::register_endpoint_factory("dg_fire_and_forget", Arc::new(FireAndForget));
    let custom = |name: &str, config| {
        Endpoint::new(EndpointType::Custom {
            name: name.to_string(),
            config,
        })
    };
    let memory = || Endpoint::new_memory("delivery_x", 1);

    let keyed = Route::new(
        memory(),
        custom("dg_keyed_sink", serde_json::json!({"key": "id"})),
    );
    assert_eq!(
        keyed.delivery_guarantee(),
        DeliveryGuarantee::EffectivelyOnce
    );
    let unkeyed = Route::new(memory(), custom("dg_keyed_sink", serde_json::json!({})));
    assert_eq!(unkeyed.delivery_guarantee(), DeliveryGuarantee::AtLeastOnce);
    let lossy = Route::new(
        custom("dg_fire_and_forget", serde_json::json!({})),
        memory(),
    );
    assert_eq!(lossy.delivery_guarantee(), DeliveryGuarantee::AtMostOnce);
    let unknown = Route::new(memory(), custom("dg_not_registered", serde_json::json!({})));
    assert_eq!(unknown.delivery_guarantee(), DeliveryGuarantee::AtLeastOnce);
}

/// A sink that replaces its target each run only starts behind an input that reads everything.
#[test]
fn a_full_replay_sink_needs_an_input_that_reads_everything() {
    #[derive(Debug)]
    struct Overwrite;
    impl crate::traits::CustomEndpointFactory for Overwrite {
        fn requires_full_replay(&self, config: &serde_json::Value) -> bool {
            config.get("overwrite").is_some()
        }
    }
    #[derive(Debug)]
    struct FullSource;
    impl crate::traits::CustomEndpointFactory for FullSource {
        fn replays_from_start(&self, _config: &serde_json::Value) -> bool {
            true
        }
    }
    let _ = crate::extensions::register_endpoint_factory("fr_overwrite", Arc::new(Overwrite));
    let _ = crate::extensions::register_endpoint_factory("fr_full", Arc::new(FullSource));
    let custom = |name: &str, config| {
        Endpoint::new(EndpointType::Custom {
            name: name.to_string(),
            config,
        })
    };
    let overwrite = || custom("fr_overwrite", serde_json::json!({"overwrite": true}));
    let file = |mode| {
        Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: "/tmp/mqb-full-replay-input.jsonl".to_string(),
            mode,
            ..Default::default()
        }))
    };
    let check = |input: Endpoint, output: Endpoint| Route::new(input, output).check("fr", None);

    check(file(None), overwrite()).expect("a plain file read covers everything");
    check(custom("fr_full", serde_json::json!({})), overwrite()).expect("declared full");
    check(
        Endpoint::new_memory("fr_in", 1),
        custom("fr_overwrite", serde_json::json!({})),
    )
    .expect("an appending sink takes any input");

    let resumed = file(Some(crate::models::FileConsumerMode::GroupSubscribe {
        group_id: "g".to_string(),
        read_from_tail: false,
    }));
    for (input, output) in [
        (resumed, overwrite()),
        (Endpoint::new_memory("fr_in", 1), overwrite()),
        (
            Endpoint::new_memory("fr_in", 1),
            Endpoint::new(EndpointType::Fanout(vec![
                Endpoint::new_memory("fr_leg", 1),
                overwrite(),
            ])),
        ),
    ] {
        let err = check(input, output).unwrap_err().to_string();
        assert!(
            err.contains("'fr_overwrite' output replaces its target"),
            "{err}"
        );
    }
}

#[test]
fn a_saved_or_configured_position_is_not_a_full_replay_unless_no_resume() {
    let bulk = |read: serde_json::Value| -> Endpoint {
        serde_json::from_value(serde_json::json!({
            "http_bulk": {"url": "http://localhost:1", "read": read}
        }))
        .unwrap()
    };
    let plain = bulk(serde_json::json!({"path": "/docs"}));
    let saved = bulk(serde_json::json!({
        "path": "/docs", "cursor_id": "c", "checkpoint_store": "file:///tmp/mqb-c.json"
    }));
    let id_only = bulk(serde_json::json!({"path": "/docs", "cursor_id": "c"}));
    let started =
        bulk(serde_json::json!({"path": "/docs", "cursor": {"response": "/n", "start": 5}}));
    assert!(source_replays_from_start(&plain, false, 0));
    assert!(!source_replays_from_start(&saved, false, 0));
    assert!(
        source_replays_from_start(&id_only, false, 0),
        "no store, no checkpoint"
    );
    assert_eq!(
        source_replays_from_start(&saved, true, 0),
        cfg!(feature = "http-bulk"),
        "a plugin-served input ignores no_resume"
    );
    assert!(!source_replays_from_start(&started, true, 0));
    let meili: Endpoint = serde_json::from_value(serde_json::json!({"meilisearch": {
        "url": "meilisearch://localhost:7700", "index": "docs"
    }}))
    .unwrap();
    assert!(source_replays_from_start(&meili, false, 0));
    let sqlx: Endpoint = serde_json::from_value(serde_json::json!({"sqlx": {
        "url": "sqlite::memory:", "table": "t", "cursor_column": "id", "cursor_id": "c"
    }}))
    .unwrap();
    assert!(!source_replays_from_start(&sqlx, false, 0));
    assert_eq!(
        source_replays_from_start(&sqlx, true, 0),
        cfg!(any(feature = "sqlite", feature = "sqlx"))
    );
}

#[test]
fn a_file_sink_behind_a_saved_position_is_synced() {
    let endpoint =
        |value: serde_json::Value| -> Endpoint { serde_json::from_value(value).unwrap() };
    let cursor = endpoint(serde_json::json!({"sqlx": {
        "url": "sqlite::memory:", "table": "t", "cursor_column": "id", "cursor_id": "c"
    }}));
    let file = |fsync: &str| {
        endpoint(serde_json::json!({"file": {"path": "/tmp/mqb-synced-out.jsonl", "fsync": fsync}}))
    };
    let fsync_of = |output: &Endpoint| match &output.endpoint_type {
        EndpointType::File(config) => config.fsync,
        _ => panic!("not a file sink"),
    };
    let sync = |input: &Endpoint, output: &Endpoint, no_resume: bool| {
        sync_file_sinks_behind_saved_position("synced", input, output, false, no_resume)
    };

    let synced = sync(&cursor, &file("off"), false).expect("upgraded");
    assert_eq!(fsync_of(&synced), crate::models::FileFsync::Periodic);
    let fanout = Endpoint::new(EndpointType::Fanout(vec![
        Endpoint::new_memory("synced_leg", 1),
        file("off"),
    ]));
    match sync(&cursor, &fanout, false).map(|o| o.endpoint_type) {
        Some(EndpointType::Fanout(legs)) => {
            assert_eq!(fsync_of(&legs[1]), crate::models::FileFsync::Periodic)
        }
        other => panic!("{other:?}"),
    }
    let with_dlq = endpoint(serde_json::json!({"memory": {"topic": "synced_main"},
        "middlewares": [{"dlq": {"endpoint": {"file": {"path": "/tmp/mqb-synced-dlq.jsonl"}}}}]
    }));
    match sync(&cursor, &with_dlq, false).map(|o| o.middlewares) {
        Some(middlewares) => match &middlewares[0] {
            Middleware::Dlq(config) => {
                assert_eq!(
                    fsync_of(&config.endpoint),
                    crate::models::FileFsync::Periodic
                )
            }
            other => panic!("{other:?}"),
        },
        None => panic!("the dlq file was not upgraded"),
    }

    assert!(
        sync(&cursor, &file("batch"), false).is_none(),
        "an explicit sync stays"
    );
    #[cfg(unix)]
    {
        let device = endpoint(serde_json::json!({"file": {"path": "/dev/null"}}));
        assert!(
            sync(&cursor, &device, false).is_none(),
            "a device cannot be synced"
        );
    }
    assert_eq!(
        sync(&cursor, &file("off"), true).is_none(),
        cfg!(any(feature = "sqlite", feature = "sqlx")),
        "no_resume saves nothing where the input applies it"
    );
    assert!(sync(&Endpoint::new_memory("synced_in", 1), &file("off"), false).is_none());
    let temporary = endpoint(serde_json::json!({"postgres_cdc": {
        "url": "postgres://localhost/db", "publication": "p", "slot_name": "s",
        "temporary_slot": true
    }}));
    assert!(!input_saves_position(&temporary, false, 0));
}

#[test]
fn guarantees_order_and_parse_as_documented() {
    assert!(DeliveryGuarantee::AtMostOnce < DeliveryGuarantee::AtLeastOnce);
    assert!(DeliveryGuarantee::AtLeastOnce < DeliveryGuarantee::EffectivelyOnce);
    let parsed: RouteOptions =
        serde_json::from_str(r#"{"required_delivery": "effectively_once"}"#).unwrap();
    assert_eq!(
        parsed.required_delivery,
        Some(DeliveryGuarantee::EffectivelyOnce)
    );
    assert_eq!(DeliveryGuarantee::AtLeastOnce.to_string(), "at-least-once");

    let route: Route = serde_json::from_str(
        r#"{"required_delivery": "effectively_once",
                "input": {"memory": {"topic": "a"}}, "output": {"memory": {"topic": "b"}}}"#,
    )
    .unwrap();
    assert_eq!(
        route.options.required_delivery,
        Some(DeliveryGuarantee::EffectivelyOnce)
    );
}

#[cfg(feature = "dedup")]
#[test]
fn deduplicating_a_minted_message_id_is_warned_about() {
    let dedup = |key: Option<&str>| {
        Middleware::Deduplication(crate::models::DeduplicationMiddleware {
            store: Some("sled:///tmp/unused".to_string()),
            sled_path: None,
            ttl_seconds: 60,
            key: key.map(str::to_string),
            replay_response: false,
        })
    };
    let file_with = |key| Endpoint {
        middlewares: vec![dedup(key)],
        ..Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
            "/tmp/orders.jsonl",
        )))
    };
    let warns = |input: Endpoint| {
        Route::new(input, Endpoint::new_memory("delivery_out", 1))
            .check("dedup", None)
            .unwrap()
            .iter()
            .any(|w| w.contains("mints fresh"))
    };
    assert!(warns(file_with(None)));
    assert!(!warns(file_with(Some("${payload:id}"))));

    // `consume` unset is `capture_all`, which mints an id like the explicit modes.
    #[cfg(feature = "mongodb")]
    {
        use crate::models::{MongoConsume, MongoDbConfig};
        let mongo = |consume| Endpoint {
            middlewares: vec![dedup(None)],
            ..Endpoint::new(EndpointType::MongoDb(MongoDbConfig {
                collection: Some("orders".to_string()),
                consume,
                ..MongoDbConfig::new("mongodb://localhost:27017", "shop")
            }))
        };
        assert!(warns(mongo(None)));
        assert!(warns(mongo(Some(MongoConsume::Snapshot))));
        assert!(!warns(mongo(Some(MongoConsume::Consumer))));
    }
}

/// The object-store sink reports the mechanism it derived, so `name_by: auto` has to be
/// resolved against the route's input rather than read off the config.
#[cfg(feature = "object-store")]
#[test]
fn only_write_time_object_stores_behind_a_replaying_input_skip_fsync() {
    let endpoint =
        |value: serde_json::Value| -> Endpoint { serde_json::from_value(value).unwrap() };
    let replaying = endpoint(serde_json::json!({"file": {"path": "/tmp/mqb-in.jsonl"}}));
    let broker = Endpoint::new_memory("fsync_broker", 1);
    let bucket = |extra: serde_json::Value| {
        let mut config = serde_json::json!({"url": "file:///tmp/mqb-fsync/"});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        endpoint(serde_json::json!({ "object_store": config }))
    };
    let fsync_of = |output: &Endpoint| match &output.endpoint_type {
        EndpointType::ObjectStore(config) => config.fsync,
        other => panic!("not an object-store sink: {other:?}"),
    };
    let relax = |input: &Endpoint, output: &Endpoint| {
        relax_object_store_fsync("fsync", input, output, false, false)
    };
    let off = Some(crate::models::ObjectStoreFsync::Off);

    let write_time = bucket(serde_json::json!({"name_by": "write_time"}));
    assert_eq!(fsync_of(&relax(&replaying, &write_time).unwrap()), off);
    assert_eq!(
        fsync_of(&relax(&replaying, &bucket(serde_json::json!({}))).unwrap()),
        off
    );
    assert!(
        relax(&broker, &write_time).is_none(),
        "a broker input keeps fsync"
    );
    let remote = endpoint(
        serde_json::json!({"object_store": {"url": "s3://b/p/", "name_by": "write_time"}}),
    );
    assert!(
        relax(&replaying, &remote).is_none(),
        "only the local store syncs"
    );
    let source_position = bucket(serde_json::json!({"name_by": "source_position"}));
    assert!(relax(&replaying, &source_position).is_none());
    let batch = bucket(serde_json::json!({"name_by": "write_time", "fsync": "batch"}));
    assert!(
        relax(&replaying, &batch).is_none(),
        "an explicit choice is kept"
    );
    let fanout = Endpoint::new(EndpointType::Fanout(vec![broker.clone(), write_time]));
    match relax(&replaying, &fanout).map(|o| o.endpoint_type) {
        Some(EndpointType::Fanout(legs)) => assert_eq!(fsync_of(&legs[1]), off),
        other => panic!("{other:?}"),
    }
}

#[cfg(feature = "object-store")]
#[test]
fn auto_named_object_store_reports_the_mechanism_its_input_earns() {
    fn route_from(input: Endpoint) -> Route {
        Route::new(
            input,
            Endpoint::new(EndpointType::ObjectStore(
                crate::models::ObjectStoreConfig {
                    url: "s3://bucket/data".to_string(),
                    ..Default::default()
                },
            )),
        )
    }

    let replayable = route_from(Endpoint::new(EndpointType::File(
        crate::models::FileConfig::new("/tmp/orders.csv"),
    )));
    assert_eq!(
        replayable.inferred_idempotency_mechanism(&replayable.output),
        Some("object names carrying the source range")
    );

    // Memory carries no replay position, so `auto` falls back and claims nothing.
    let positionless = route_from(Endpoint::new_memory("delivery_in", 1));
    assert_eq!(
        positionless.inferred_idempotency_mechanism(&positionless.output),
        None
    );

    let filtered = route_from(Endpoint {
        middlewares: vec![Middleware::Filter("amount > 100".into())],
        ..Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
            "/tmp/orders.csv",
        )))
    });
    let (relaxed, _) = relax_object_naming(
        "delivery",
        filtered.source_has_position(),
        &filtered.input,
        &filtered.output,
    )
    .unwrap()
    .expect("filter relaxes object naming");
    assert_eq!(filtered.inferred_idempotency_mechanism(&relaxed), None);
}

/// A `ref` may name another `ref`. Resolving only the first hop made a chained input look
/// positionless, which silently demoted `name_by: auto` to `write_time` — the sink stopped
/// recognising a replay rather than failing where anyone would notice.
#[test]
fn a_chained_ref_input_resolves_to_the_endpoint_at_the_end_of_it() {
    fn route_with_input(name: &str) -> Route {
        Route::new(
            Endpoint::new(EndpointType::Ref(name.to_string())),
            Endpoint::new_memory("chain_out", 1),
        )
    }

    register_endpoint(
        "chain-file",
        Endpoint::new(EndpointType::File(crate::models::FileConfig::new(
            "/tmp/chain.csv",
        ))),
    );
    register_endpoint(
        "chain-middle",
        Endpoint::new(EndpointType::Ref("chain-file".to_string())),
    );
    register_endpoint(
        "chain-outer",
        Endpoint::new(EndpointType::Ref("chain-middle".to_string())),
    );
    assert!(route_with_input("chain-outer").source_has_position());

    // Unresolvable chains keep the old answer: no position, so `auto` falls back.
    assert!(!route_with_input("chain-missing").source_has_position());
    register_endpoint(
        "chain-loop",
        Endpoint::new(EndpointType::Ref("chain-loop".to_string())),
    );
    assert!(!route_with_input("chain-loop").source_has_position());

    // A chain ending somewhere positionless still reports none.
    register_endpoint("chain-memory", Endpoint::new_memory("chain_in", 1));
    register_endpoint(
        "chain-to-memory",
        Endpoint::new(EndpointType::Ref("chain-memory".to_string())),
    );
    assert!(!route_with_input("chain-to-memory").source_has_position());
}

#[test]
fn test_route_check_rejects_zero_execution_options() {
    let route = Route::new(
        Endpoint::new_memory("zero_in", 10),
        Endpoint::new_memory("zero_out", 10),
    )
    .with_options(RouteOptions {
        concurrency: 0,
        batch_size: 1,
        commit_concurrency_limit: 1,
        ..Default::default()
    });

    let err = route.check("zero_options", None).unwrap_err().to_string();
    assert!(err.contains("concurrency must be at least 1"));
}

#[test]
fn test_route_check_warns_when_buffered_writes_are_concurrent() {
    let suffix = fast_uuid_v7::gen_id_str();
    let buffered_name = format!("buffer_warning_leaf_{suffix}");
    register_endpoint(
        &buffered_name,
        Endpoint::new_memory("buffer_warning_out", 10).add_middleware(Middleware::Buffer(
            crate::models::BufferMiddleware {
                max_messages: 100,
                max_delay_ms: 10,
            },
        )),
    );
    let output = Endpoint::new(EndpointType::Fanout(vec![Endpoint::new(
        EndpointType::Ref(buffered_name),
    )]));
    let route =
        Route::new(Endpoint::new_memory("buffer_warning_in", 10), output).with_concurrency(4);

    let warnings = route.check("buffer_warning", None).unwrap();
    assert!(warnings.iter().any(|warning| {
        warning.contains("concurrent destination writes") && warning.contains("concurrency: 1")
    }));
}

#[test]
fn buffer_detection_stops_at_reference_cycles() {
    let suffix = fast_uuid_v7::gen_id_str();
    let first = format!("buffer_cycle_first_{suffix}");
    let second = format!("buffer_cycle_second_{suffix}");
    register_endpoint(&first, Endpoint::new(EndpointType::Ref(second.clone())));
    register_endpoint(&second, Endpoint::new(EndpointType::Ref(first.clone())));

    assert!(!endpoint_tree_has_buffer(
        &Endpoint::new(EndpointType::Ref(first)),
        &mut HashSet::new(),
    ));
}

#[test]
fn test_random_panic_requires_fault_injection_opt_in() {
    let fault_config = RandomPanicMiddleware {
        mode: FaultMode::Timeout,
        enabled: true,
        ..Default::default()
    };
    let input = Endpoint::new_memory("fault_policy_in", 10)
        .add_middleware(Middleware::RandomPanic(fault_config));
    let output = Endpoint::new_memory("fault_policy_out", 10);
    let route = Route::new(input, output);

    let err = route.check("fault_policy", None).unwrap_err().to_string();
    assert!(err.contains("allow_fault_injection"));
    assert!(route
        .with_fault_injection(true)
        .check("fault_policy", None)
        .is_ok());
}

#[derive(Debug, Default)]
struct CommitObservation {
    completed: Mutex<Vec<u64>>,
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
}

#[derive(Debug)]
struct CommitTrackingMiddlewareFactory {
    observation: Arc<CommitObservation>,
    // Drives the wrapped consumer's `commit_requires_order()`, letting one
    // factory exercise both the ordered (sequencer) and unordered (semaphore)
    // commit paths.
    requires_order: bool,
}

#[derive(Debug)]
struct ReorderingPublisherMiddlewareFactory;

struct CommitTrackingConsumer {
    inner: Box<dyn MessageConsumer>,
    observation: Arc<CommitObservation>,
    requires_order: bool,
}

struct ReorderingPublisher {
    inner: Box<dyn MessagePublisher>,
}

#[async_trait::async_trait]
impl CustomMiddlewareFactory for CommitTrackingMiddlewareFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(CommitTrackingConsumer {
            inner: consumer,
            observation: Arc::clone(&self.observation),
            requires_order: self.requires_order,
        }))
    }
}

#[async_trait::async_trait]
impl CustomMiddlewareFactory for ReorderingPublisherMiddlewareFactory {
    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(ReorderingPublisher { inner: publisher }))
    }
}

#[async_trait::async_trait]
impl MessageConsumer for CommitTrackingConsumer {
    fn commit_requires_order(&self) -> bool {
        self.requires_order
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        let seq = batch
            .messages
            .first()
            .and_then(|message| message.get_payload_str().parse::<u64>().ok())
            .expect("tracking test expects numeric payloads");
        let original_commit = batch.commit;
        let observation = Arc::clone(&self.observation);
        batch.commit = Box::new(move |dispositions| {
            let observation = Arc::clone(&observation);
            Box::pin(async move {
                let active_now = observation.active.fetch_add(1, Ordering::SeqCst) + 1;
                observation
                    .max_active
                    .fetch_max(active_now, Ordering::SeqCst);

                tokio::time::sleep(Duration::from_millis(20)).await;
                let result = original_commit(dispositions).await;
                observation.completed.lock().unwrap().push(seq);
                observation.active.fetch_sub(1, Ordering::SeqCst);
                result
            })
        });
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait::async_trait]
impl MessagePublisher for ReorderingPublisher {
    async fn send_batch(
        &self,
        messages: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let seq = messages
            .first()
            .and_then(|message| {
                let payload = message.get_payload_str();
                payload.parse::<u64>().ok().or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&payload).ok()?["seq"].as_u64()
                })
            })
            .expect("tracking test expects numeric payloads or a `seq` field");
        let delay_ms = 10 * (6u64.saturating_sub(seq.min(6)));
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        self.inner.send_batch(messages).await
    }

    async fn send(&self, msg: crate::CanonicalMessage) -> Result<Sent, PublisherError> {
        self.inner.send(msg).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn assert_route_commits_are_ordered_and_non_overlapping(concurrency: usize) {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let tracking_name = format!("track_commit_{}", unique_id);
    let reorder_name = format!("reorder_publish_{}", unique_id);
    let in_topic = format!("ordered_commit_in_{}", unique_id);
    let observation = Arc::new(CommitObservation::default());

    register_middleware_factory(
        &tracking_name,
        Arc::new(CommitTrackingMiddlewareFactory {
            observation: Arc::clone(&observation),
            requires_order: true,
        }),
    )
    .unwrap();
    register_middleware_factory(
        &reorder_name,
        Arc::new(ReorderingPublisherMiddlewareFactory),
    )
    .unwrap();

    let input = Endpoint::new_memory(&in_topic, 32).add_middleware(Middleware::Custom {
        name: tracking_name,
        config: serde_json::Value::Null,
    });
    let output = Endpoint::new(EndpointType::Null).add_middleware(Middleware::Custom {
        name: reorder_name,
        config: serde_json::Value::Null,
    });

    let route = Route::new(input.clone(), output)
        .with_concurrency(concurrency)
        .with_batch_size(1)
        .with_commit_concurrency_limit(1);

    let input_channel = input.channel().unwrap();
    let messages = (0..6)
        .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
        .collect();
    input_channel.fill_messages(messages).await.unwrap();
    input_channel.close();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        route.run_until_err("ordered_commit_regression", None, None),
    )
    .await
    .expect("Route should not hang while draining finite input")
    .expect("Route should complete without commit errors");
    assert_eq!(
        *observation.completed.lock().unwrap(),
        vec![0, 1, 2, 3, 4, 5],
        "Commit execution must follow receive order",
    );
    assert_eq!(
        observation.max_active.load(Ordering::SeqCst),
        1,
        "Broker-facing commit functions must never overlap",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sequential_route_commits_are_ordered_and_non_overlapping() {
    assert_route_commits_are_ordered_and_non_overlapping(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_route_commits_are_ordered_and_non_overlapping() {
    assert_route_commits_are_ordered_and_non_overlapping(4).await;
}

/// Counts the batches read from an ordered source and holds every commit until released.
#[derive(Debug)]
struct StalledCommitFactory {
    read: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

struct StalledCommitConsumer {
    inner: Box<dyn MessageConsumer>,
    read: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl CustomMiddlewareFactory for StalledCommitFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(StalledCommitConsumer {
            inner: consumer,
            read: Arc::clone(&self.read),
            release: Arc::clone(&self.release),
        }))
    }
}

#[async_trait::async_trait]
impl MessageConsumer for StalledCommitConsumer {
    fn commit_requires_order(&self) -> bool {
        true
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        if batch.messages.is_empty() {
            return Ok(batch);
        }
        self.read.fetch_add(1, Ordering::SeqCst);
        let original_commit = batch.commit;
        let release = Arc::clone(&self.release);
        batch.commit = Box::new(move |dispositions| {
            Box::pin(async move {
                let _released = release.acquire().await;
                original_commit(dispositions).await
            })
        });
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// While one ordered commit stalls, the runner stops reading instead of piling up
/// commit tasks. `allowed` is `commit_concurrency_limit` plus the batches in the runner.
async fn assert_stalled_ordered_commit_stops_reads(concurrency: usize, allowed: usize) {
    const TOTAL: usize = 60;
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let name = format!("stalled_commit_{unique_id}");
    let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    register_middleware_factory(
        &name,
        Arc::new(StalledCommitFactory {
            read: Arc::clone(&read),
            release: Arc::clone(&release),
        }),
    )
    .unwrap();

    let input = Endpoint::new_memory(&format!("stalled_in_{unique_id}"), TOTAL).add_middleware(
        Middleware::Custom {
            name,
            config: serde_json::Value::Null,
        },
    );
    let route = Route::new(input.clone(), Endpoint::new(EndpointType::Null))
        .with_concurrency(concurrency)
        .with_batch_size(1)
        .with_commit_concurrency_limit(1);
    let input_channel = input.channel().unwrap();
    let messages = (0..TOTAL)
        .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
        .collect();
    input_channel.fill_messages(messages).await.unwrap();
    input_channel.close();

    let running = tokio::spawn(async move {
        route
            .run_until_err("stalled_ordered_commit", None, None)
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stalled_at = read.load(Ordering::SeqCst);
    assert!(
        (1..=allowed).contains(&stalled_at),
        "read {stalled_at} batches while the first commit was stalled, allowed {allowed}"
    );

    release.add_permits(TOTAL);
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the route must finish once commits run again")
        .unwrap()
        .unwrap();
    assert_eq!(read.load(Ordering::SeqCst), TOTAL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_ordered_commit_stops_the_sequential_runner_reading() {
    assert_stalled_ordered_commit_stops_reads(1, 3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_ordered_commit_stops_the_concurrent_runner_reading() {
    assert_stalled_ordered_commit_stops_reads(4, 10).await;
}

#[derive(Debug)]
struct PanicOnFirstBatchFactory;

struct PanicOnFirstBatch(Box<dyn MessagePublisher>);

#[async_trait::async_trait]
impl CustomMiddlewareFactory for PanicOnFirstBatchFactory {
    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(PanicOnFirstBatch(publisher)))
    }
}

#[async_trait::async_trait]
impl MessagePublisher for PanicOnFirstBatch {
    async fn send_batch(
        &self,
        messages: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages[0].get_payload_str() == "0" {
            // Long enough for later batches to queue their commits behind this one.
            tokio::time::sleep(Duration::from_millis(100)).await;
            panic!("injected worker panic");
        }
        self.0.send_batch(messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The panicked worker's batch leaves a gap in the commit sequence; the commits queued
/// behind it must fail instead of holding the route open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicked_worker_fails_the_route_instead_of_blocking_ordered_commits() {
    assert_panicked_send_fails_the_route(4).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicked_send_fails_the_sequential_route_instead_of_blocking_ordered_commits() {
    assert_panicked_send_fails_the_route(1).await;
}

async fn assert_panicked_send_fails_the_route(concurrency: usize) {
    const TOTAL: usize = 20;
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let ordered = format!("panic_ordered_{unique_id}");
    let panicking = format!("panic_publisher_{unique_id}");
    let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    register_middleware_factory(
        &ordered,
        Arc::new(StalledCommitFactory {
            read: Arc::clone(&read),
            release: Arc::new(tokio::sync::Semaphore::new(TOTAL)),
        }),
    )
    .unwrap();
    register_middleware_factory(&panicking, Arc::new(PanicOnFirstBatchFactory)).unwrap();

    let custom = |name| Middleware::Custom {
        name,
        config: serde_json::Value::Null,
    };
    let input = Endpoint::new_memory(&format!("panic_in_{unique_id}"), TOTAL)
        .add_middleware(custom(ordered));
    let output = Endpoint::new(EndpointType::Null).add_middleware(custom(panicking));
    let route = Route::new(input.clone(), output)
        .with_concurrency(concurrency)
        .with_batch_size(1);
    let input_channel = input.channel().unwrap();
    let messages = (0..TOTAL)
        .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
        .collect();
    input_channel.fill_messages(messages).await.unwrap();
    input_channel.close();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        route.run_until_err("panicked_worker", None, None),
    )
    .await
    .expect("a worker panic must not leave the route waiting on its commit");
    assert!(result.is_err(), "a panicked worker is not a completion");
}

/// Counts reads and records each committed payload with its disposition.
#[derive(Debug)]
struct ReadAheadProbe {
    read: AtomicUsize,
    committed: Mutex<Vec<(String, bool)>>,
    sent: Mutex<Vec<String>>,
    /// One permit per send; the send of payload `fail_on` returns a retryable error.
    gate: tokio::sync::Semaphore,
    fail_on: Option<&'static str>,
}

impl ReadAheadProbe {
    fn new(fail_on: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            read: AtomicUsize::new(0),
            committed: Mutex::default(),
            sent: Mutex::default(),
            gate: tokio::sync::Semaphore::new(0),
            fail_on,
        })
    }
}

#[derive(Debug)]
struct ReadAheadProbeFactory(Arc<ReadAheadProbe>);

struct ProbedConsumer(Box<dyn MessageConsumer>, Arc<ReadAheadProbe>);
struct ProbedPublisher(Box<dyn MessagePublisher>, Arc<ReadAheadProbe>);

#[async_trait::async_trait]
impl CustomMiddlewareFactory for ReadAheadProbeFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(ProbedConsumer(consumer, Arc::clone(&self.0))))
    }

    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(ProbedPublisher(publisher, Arc::clone(&self.0))))
    }
}

#[async_trait::async_trait]
impl MessageConsumer for ProbedConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.0.receive_batch(max_messages).await?;
        if batch.messages.is_empty() {
            return Ok(batch);
        }
        self.1.read.fetch_add(1, Ordering::SeqCst);
        let payloads: Vec<String> = batch
            .messages
            .iter()
            .map(|m| m.get_payload_str().to_string())
            .collect();
        let probe = Arc::clone(&self.1);
        let inner = batch.commit;
        batch.commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let mut committed = probe.committed.lock().unwrap();
            for (payload, disposition) in payloads.into_iter().zip(&dispositions) {
                let acked = !matches!(disposition, MessageDisposition::Nack);
                committed.push((payload, acked));
            }
            inner(dispositions)
        });
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait::async_trait]
impl MessagePublisher for ProbedPublisher {
    async fn send_batch(
        &self,
        messages: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        self.1.gate.acquire().await.unwrap().forget();
        let payload = messages[0].get_payload_str().to_string();
        if self.1.fail_on == Some(payload.as_str()) {
            return Err(PublisherError::Retryable(anyhow::anyhow!("injected")));
        }
        self.1.sent.lock().unwrap().push(payload);
        self.0.send_batch(messages).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Runs a sequential route over `total` single-message batches with a probe on both ends.
fn probed_sequential_route(
    probe: &Arc<ReadAheadProbe>,
    total: usize,
) -> (Route, tokio::task::JoinHandle<()>) {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let name = format!("read_ahead_probe_{unique_id}");
    register_middleware_factory(&name, Arc::new(ReadAheadProbeFactory(Arc::clone(probe)))).unwrap();
    let custom = || Middleware::Custom {
        name: name.clone(),
        config: serde_json::Value::Null,
    };
    let input =
        Endpoint::new_memory(&format!("read_ahead_in_{unique_id}"), total).add_middleware(custom());
    let output = Endpoint::new(EndpointType::Null).add_middleware(custom());
    let route = Route::new(input.clone(), output)
        .with_concurrency(1)
        .with_batch_size(1);
    let fill = tokio::spawn(async move {
        let channel = input.channel().unwrap();
        let messages = (0..total).map(|seq| CanonicalMessage::from(seq.to_string()));
        channel.fill_messages(messages.collect()).await.unwrap();
        channel.close();
    });
    (route, fill)
}

async fn wait_for_reads(probe: &ReadAheadProbe, reads: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while probe.read.load(Ordering::SeqCst) < reads {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the runner should read ahead while a send is pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sequential_runner_reads_one_batch_ahead_and_sends_in_order() {
    const TOTAL: usize = 10;
    let probe = ReadAheadProbe::new(None);
    let (route, fill) = probed_sequential_route(&probe, TOTAL);
    fill.await.unwrap();
    let running = tokio::spawn(async move { route.run_until_err("read_ahead", None, None).await });

    wait_for_reads(&probe, 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        probe.read.load(Ordering::SeqCst),
        2,
        "one batch ahead, no more"
    );

    probe.gate.add_permits(TOTAL);
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let expected: Vec<String> = (0..TOTAL).map(|seq| seq.to_string()).collect();
    assert_eq!(*probe.sent.lock().unwrap(), expected);
    let committed = probe.committed.lock().unwrap();
    assert!(committed.iter().all(|(_, acked)| *acked), "{committed:?}");
    assert_eq!(committed.len(), TOTAL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_send_returns_the_batch_read_ahead_unsent() {
    let probe = ReadAheadProbe::new(Some("0"));
    let (route, fill) = probed_sequential_route(&probe, 4);
    fill.await.unwrap();
    let running =
        tokio::spawn(async move { route.run_until_err("read_ahead_fail", None, None).await });

    wait_for_reads(&probe, 2).await;
    probe.gate.add_permits(4);
    let result = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err(), "a failed send ends the route");
    assert!(probe.sent.lock().unwrap().is_empty());
    assert_eq!(
        *probe.committed.lock().unwrap(),
        vec![("0".to_string(), false), ("1".to_string(), false)],
    );
}

/// Counts reads that were dropped before they finished.
#[derive(Debug)]
struct CancelCountingFactory(Arc<AtomicUsize>);

struct CancelCountingConsumer(Box<dyn MessageConsumer>, Arc<AtomicUsize>);

struct UnfinishedRead<'a>(&'a AtomicUsize, bool);

impl Drop for UnfinishedRead<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl CustomMiddlewareFactory for CancelCountingFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(CancelCountingConsumer(
            consumer,
            Arc::clone(&self.0),
        )))
    }
}

#[async_trait::async_trait]
impl MessageConsumer for CancelCountingConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut guard = UnfinishedRead(&self.1, false);
        // Slow enough that the previous batch's send finishes first.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let batch = self.0.receive_batch(max_messages).await;
        guard.1 = true;
        batch
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_send_does_not_cancel_the_read_ahead() {
    const TOTAL: usize = 20;
    let cancelled = Arc::new(AtomicUsize::new(0));
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let name = format!("cancel_counting_{unique_id}");
    register_middleware_factory(&name, Arc::new(CancelCountingFactory(cancelled.clone()))).unwrap();
    let input = Endpoint::new_memory(&format!("cancel_counting_in_{unique_id}"), TOTAL)
        .add_middleware(Middleware::Custom {
            name,
            config: serde_json::Value::Null,
        });
    let channel = input.channel().unwrap();
    let messages = (0..TOTAL).map(|seq| CanonicalMessage::from(seq.to_string()));
    channel.fill_messages(messages.collect()).await.unwrap();
    channel.close();
    let route = Route::new(input, Endpoint::new(EndpointType::Null))
        .with_concurrency(1)
        .with_batch_size(1);

    tokio::time::timeout(
        Duration::from_secs(5),
        route.run_until_err("cancel_counting", None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
}

#[derive(Debug, Default)]
struct PublishObservation {
    /// Batch sequence numbers in the order their `send_batch` completed.
    arrived: Mutex<Vec<u64>>,
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
}

#[derive(Debug)]
struct OrderedPublishMiddlewareFactory {
    observation: Arc<PublishObservation>,
    /// Drives the wrapped publisher's `requires_ordered_publish()`, so one factory
    /// exercises both an order-sensitive sink (file, object_store) and a plain one.
    requires_order: bool,
}

struct OrderedPublishTracker {
    inner: Box<dyn MessagePublisher>,
    observation: Arc<PublishObservation>,
    requires_order: bool,
}

#[async_trait::async_trait]
impl CustomMiddlewareFactory for OrderedPublishMiddlewareFactory {
    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        _route_name: &str,
        _config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(OrderedPublishTracker {
            inner: publisher,
            observation: Arc::clone(&self.observation),
            requires_order: self.requires_order,
        }))
    }
}

#[async_trait::async_trait]
impl MessagePublisher for OrderedPublishTracker {
    fn requires_ordered_publish(&self) -> bool {
        self.requires_order
    }

    async fn send_batch(
        &self,
        messages: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let seq = messages
            .first()
            .and_then(|message| {
                let payload = message.get_payload_str();
                payload.parse::<u64>().ok().or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&payload).ok()?["seq"].as_u64()
                })
            })
            .expect("tracking test expects numeric payloads or a `seq` field");
        let active_now = self.observation.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.observation
            .max_active
            .fetch_max(active_now, Ordering::SeqCst);
        // Later batches are faster, so unsequenced sends land in reverse order.
        tokio::time::sleep(Duration::from_millis(
            10 * (6u64.saturating_sub(seq.min(6))),
        ))
        .await;
        let result = self.inner.send_batch(messages).await;
        self.observation.arrived.lock().unwrap().push(seq);
        self.observation.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn run_publish_order_route(requires_order: bool) -> Arc<PublishObservation> {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let tracking_name = format!("track_publish_{}", unique_id);
    let in_topic = format!("publish_order_in_{}", unique_id);
    let observation = Arc::new(PublishObservation::default());

    register_middleware_factory(
        &tracking_name,
        Arc::new(OrderedPublishMiddlewareFactory {
            observation: Arc::clone(&observation),
            requires_order,
        }),
    )
    .unwrap();

    let input = Endpoint::new_memory(&in_topic, 32);
    let output = Endpoint::new(EndpointType::Null).add_middleware(Middleware::Custom {
        name: tracking_name,
        config: serde_json::Value::Null,
    });

    let route = Route::new(input.clone(), output)
        .with_concurrency(4)
        .with_batch_size(1);

    let input_channel = input.channel().unwrap();
    let messages = (0..6)
        .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
        .collect();
    input_channel.fill_messages(messages).await.unwrap();
    input_channel.close();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        route.run_until_err("publish_order_test", None, None),
    )
    .await
    .expect("Route should not hang while draining finite input")
    .expect("Route should complete without errors");

    observation
}

// Regression (issue #71): with `concurrency > 1` the workers used to call `send_batch`
// in parallel, so whole batches reached the sink out of source order — silently
// shuffling a file/object_store export. A sink that declares
// `requires_ordered_publish()` now gets its sends sequenced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_order_sensitive_sink_receives_batches_in_source_order() {
    let observation = run_publish_order_route(true).await;

    assert_eq!(
        *observation.arrived.lock().unwrap(),
        vec![0, 1, 2, 3, 4, 5],
        "An order-sensitive sink must see batches in source order",
    );
    assert_eq!(
        observation.max_active.load(Ordering::SeqCst),
        1,
        "Sequenced sends must never overlap",
    );
}

// With `parallel` on the output the route no longer waits before `send_batch`; the marker
// waits instead. The tracker below it delays early batches, so a lost turn shows up as
// batches arriving out of order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_output_parallel_marker_keeps_an_ordered_sink_in_source_order() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let tracking_name = format!("track_marker_{unique_id}");
    let observation = Arc::new(PublishObservation::default());
    register_middleware_factory(
        &tracking_name,
        Arc::new(OrderedPublishMiddlewareFactory {
            observation: Arc::clone(&observation),
            requires_order: true,
        }),
    )
    .unwrap();
    let middleware = |value| serde_json::from_value::<Middleware>(value).unwrap();
    let input = Endpoint::new_memory(&format!("marker_in_{unique_id}"), 32);
    let mut output = Endpoint::new(EndpointType::Null);
    output.middlewares = vec![
        middleware(serde_json::json!({
            "lookup": { "from": { "static": { "body": "{\"tier\":\"gold\"}", "raw": true } }, "into": "customer" }
        })),
        middleware(serde_json::json!({ "parallel": {} })),
        Middleware::Custom {
            name: tracking_name,
            config: serde_json::Value::Null,
        },
    ];
    let route = Route::new(input.clone(), output)
        .with_concurrency(4)
        .with_batch_size(1);

    let channel = input.channel().unwrap();
    let messages = (0..6)
        .map(|seq| crate::CanonicalMessage::from(format!("{{\"seq\":{seq}}}")))
        .collect();
    channel.fill_messages(messages).await.unwrap();
    channel.close();
    tokio::time::timeout(
        Duration::from_secs(5),
        route.run_until_err("output_marker_test", None, None),
    )
    .await
    .expect("Route should not hang while draining finite input")
    .expect("Route should complete without errors");

    assert_eq!(*observation.arrived.lock().unwrap(), vec![0, 1, 2, 3, 4, 5]);
    assert_eq!(observation.max_active.load(Ordering::SeqCst), 1);
}

// The default stays unordered so concurrent sinks keep their throughput: batches may
// arrive in any order, but every one of them must arrive exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unordered_sink_still_receives_every_batch() {
    let observation = run_publish_order_route(false).await;

    let mut arrived = observation.arrived.lock().unwrap().clone();
    arrived.sort_unstable();
    assert_eq!(arrived, vec![0, 1, 2, 3, 4, 5]);
}

/// Drives a route whose consumer reports `commit_requires_order() == false`
/// and returns what the commit tracker observed. Each commit sleeps 20ms, so
/// overlapping commits push `max_active` above 1.
async fn run_unordered_commit_route(
    concurrency: usize,
    commit_concurrency_limit: usize,
    message_count: u64,
) -> Arc<CommitObservation> {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let tracking_name = format!("track_unordered_{}", unique_id);
    let in_topic = format!("unordered_commit_in_{}", unique_id);
    let observation = Arc::new(CommitObservation::default());

    register_middleware_factory(
        &tracking_name,
        Arc::new(CommitTrackingMiddlewareFactory {
            observation: Arc::clone(&observation),
            requires_order: false,
        }),
    )
    .unwrap();

    let input = Endpoint::new_memory(&in_topic, 64).add_middleware(Middleware::Custom {
        name: tracking_name,
        config: serde_json::Value::Null,
    });
    let output = Endpoint::new(EndpointType::Null);

    let route = Route::new(input.clone(), output)
        .with_concurrency(concurrency)
        .with_batch_size(1)
        .with_commit_concurrency_limit(commit_concurrency_limit);

    let input_channel = input.channel().unwrap();
    let messages = (0..message_count)
        .map(|seq| crate::CanonicalMessage::from(seq.to_string()))
        .collect();
    input_channel.fill_messages(messages).await.unwrap();
    input_channel.close();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        route.run_until_err("unordered_commit_test", None, None),
    )
    .await
    .expect("Route should not hang while draining finite input")
    .expect("Route should complete without commit errors");

    observation
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unordered_commits_run_concurrently_without_loss() {
    let observation = run_unordered_commit_route(4, 4, 6).await;

    // Every batch must still be committed exactly once (no data loss), even
    // though completion order is not guaranteed.
    let mut completed = observation.completed.lock().unwrap().clone();
    completed.sort_unstable();
    assert_eq!(
        completed,
        (0..6).collect::<Vec<u64>>(),
        "Every message must be committed exactly once",
    );

    // The whole point of the unordered path: commits overlap instead of being
    // serialized one-at-a-time by the sequencer.
    assert!(
        observation.max_active.load(Ordering::SeqCst) > 1,
        "Unordered commits must run concurrently (max_active was {})",
        observation.max_active.load(Ordering::SeqCst),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unordered_commits_respect_concurrency_limit() {
    // commit_concurrency_limit caps how many commits run at once even when the
    // route concurrency is higher.
    let observation = run_unordered_commit_route(4, 2, 8).await;

    let mut completed = observation.completed.lock().unwrap().clone();
    completed.sort_unstable();
    assert_eq!(completed, (0..8).collect::<Vec<u64>>());

    assert!(
        observation.max_active.load(Ordering::SeqCst) <= 2,
        "Concurrent commits must stay within commit_concurrency_limit (max_active was {})",
        observation.max_active.load(Ordering::SeqCst),
    );
}

// Helper function to run a fault injection test on the consumer side.
async fn run_consumer_fault_test(
    mode: FaultMode,
    expected_payload: &str,
    route_should_restart: bool,
    concurrency: usize,
) {
    let unique_suffix = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("fault_in_{}_{}_{}", mode, concurrency, unique_suffix);
    let out_topic = format!("fault_out_{}_{}_{}", mode, concurrency, unique_suffix);

    let fault_config = RandomPanicMiddleware {
        mode,
        trigger_on_message: Some(1), // Panic on the first message
        enabled: true,
        ..Default::default()
    };

    let input =
        Endpoint::new_memory(&in_topic, 10).add_middleware(Middleware::RandomPanic(fault_config));
    let output = Endpoint::new_memory(&out_topic, 10);

    let route_name = format!("fault_test_{}_{}", mode, concurrency);
    let route = Route::new(input.clone(), output.clone())
        .with_concurrency(concurrency)
        .with_fault_injection(true)
        .with_reconnect_interval_ms(100);

    // Start the route
    route
        .deploy(&route_name)
        .await
        .expect("Failed to deploy route");
    // Send a message. The consumer will inject a fault when it tries to receive it.
    let input_ch = input.channel().unwrap();
    input_ch
        .send_message("persistent_msg".into())
        .await
        .unwrap();

    if route_should_restart {
        // The route's worker will fail, then the supervisor will restart it.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    } else {
        // Route doesn't restart, just wait a bit for the (faulty) message to pass through.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Verify the outcome.
    let mut verifier = route.connect_to_output("verifier").await.unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
        .await
        .expect("Timed out waiting for message after fault")
        .expect("Stream closed while waiting for message");

    assert_eq!(received.message.get_payload_str(), expected_payload);
    (received.commit)(MessageDisposition::Ack).await.unwrap();

    // Cleanup
    Route::stop(&route_name).await;
}

// Helper function to run a fault injection test on the publisher side.
async fn run_publisher_fault_test(
    mode: FaultMode,
    expected_payload: &str,
    route_should_restart: bool,
) {
    let unique_suffix = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("pub_fault_in_{}_{}", mode, unique_suffix);
    let out_topic = format!("pub_fault_out_{}_{}", mode, unique_suffix);

    let fault_config = RandomPanicMiddleware {
        mode,
        trigger_on_message: Some(1), // Trigger on the first message
        enabled: true,
        ..Default::default()
    };

    let mut input = Endpoint::new_memory(&in_topic, 10);
    // Enable NACK on input so messages aren't lost when publisher crashes
    if let EndpointType::Memory(ref mut cfg) = input.endpoint_type {
        cfg.enable_nack = true;
    }
    // Apply fault middleware to output
    let output =
        Endpoint::new_memory(&out_topic, 10).add_middleware(Middleware::RandomPanic(fault_config));

    let route_name = format!("pub_fault_test_{}", mode);
    let route = Route::new(input.clone(), output.clone())
        .with_fault_injection(true)
        .with_reconnect_interval_ms(100);

    route
        .deploy(&route_name)
        .await
        .expect("Failed to deploy route");

    let input_ch = input.channel().unwrap();
    input_ch
        .send_message(expected_payload.into())
        .await
        .unwrap();

    if route_should_restart {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let mut verifier = route.connect_to_output("verifier").await.unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
        .await
        .expect("Timed out waiting for message after publisher fault")
        .expect("Stream closed");

    assert_eq!(received.message.get_payload_str(), expected_payload);
    (received.commit)(MessageDisposition::Ack).await.unwrap();

    Route::stop(&route_name).await;
}

// No Docker. Ignored to keep default `cargo test` focused on the fast path.
// Locally measured on 2026-06-21: this plus the sequential variant ran in ~2s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Takes too much time for regular tests"]
async fn test_route_recovery_from_faults() {
    let original_payload = "persistent_msg";

    // Test with concurrency > 1
    run_consumer_fault_test(FaultMode::Panic, original_payload, true, 2).await;
    run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 2).await;
    run_consumer_fault_test(FaultMode::Timeout, original_payload, true, 2).await;
    run_consumer_fault_test(FaultMode::Nack, original_payload, true, 2).await;

    // This fault replaces the message but does not restart the route.
    run_consumer_fault_test(FaultMode::JsonFormatError, "{invalid json}", false, 2).await;
}

// No Docker. Run with `cargo test test_route_recovery_from_faults -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Takes too much time for regular tests"]
async fn test_route_recovery_from_faults_sequential() {
    let original_payload = "persistent_msg";

    // Test with concurrency = 1
    run_consumer_fault_test(FaultMode::Panic, original_payload, true, 1).await;
    run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 1).await;
}

// No Docker. Locally measured on 2026-06-21 at ~1s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Takes too much time for regular tests"]
async fn test_publisher_recovery_from_faults() {
    let original_payload = "persistent_msg";
    // Test publisher-side faults causing restart/retry.
    // `FaultMode::Panic` is not tested here because the `MemoryConsumer` used for input
    // does not support crash-safe at-least-once delivery. A panic in the publisher
    // worker would cause the in-flight message to be lost.
    run_publisher_fault_test(FaultMode::Disconnect, original_payload, true).await;
    run_publisher_fault_test(FaultMode::Timeout, original_payload, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_route_sequencer_deadlock_fix() {
    // This test ensures that when a worker fails to send a batch (and thus drops the commit handle),
    // the sequencer doesn't deadlock waiting for that sequence number.
    // The fix ensures that even on failure, the commit function is called (with Nack) to fill the sequence gap.

    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("fail_factory_{}", unique_id);
    let in_topic = format!("deadlock_in_{}", unique_id);
    let out_topic = format!("deadlock_out_{}", unique_id);

    #[derive(Debug)]
    struct FailingMiddlewareFactory {
        fail_flag: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for FailingMiddlewareFactory {
        async fn apply_publisher(
            &self,
            publisher: Box<dyn MessagePublisher>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(FailingPublisher {
                inner: publisher,
                fail_flag: self.fail_flag.clone(),
            }))
        }
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(consumer)
        }
    }

    struct FailingPublisher {
        inner: Box<dyn MessagePublisher>,
        fail_flag: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl MessagePublisher for FailingPublisher {
        async fn send_batch(
            &self,
            messages: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            // We want to fail one batch to trigger the error path in the worker.
            // We use compare_exchange to ensure only one failure happens.
            if self
                .fail_flag
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(PublisherError::Retryable(anyhow::anyhow!(
                    "Simulated failure"
                )));
            }
            // Add a small delay for successful batches to ensure the failed one (if it created a gap)
            // would block the sequencer if the gap wasn't filled.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            self.inner.send_batch(messages).await
        }
        async fn send(
            &self,
            msg: crate::CanonicalMessage,
        ) -> Result<crate::traits::Sent, PublisherError> {
            self.inner.send(msg).await
        }
        async fn flush(&self) -> anyhow::Result<()> {
            self.inner.flush().await
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let fail_flag = Arc::new(AtomicBool::new(true));
    register_middleware_factory(
        &factory_name,
        Arc::new(FailingMiddlewareFactory {
            fail_flag: fail_flag.clone(),
        }),
    )
    .unwrap();

    let input = Endpoint::new_memory(&in_topic, 100);
    let output = Endpoint::new_memory(&out_topic, 100).add_middleware(Middleware::Custom {
        name: factory_name,
        config: serde_json::Value::Null,
    });

    // Concurrency > 1 is required to have multiple workers and potential out-of-order completion
    let route = Route::new(input.clone(), output.clone())
        .with_concurrency(2)
        .with_batch_size(1);

    // Send messages
    let input_ch = input.channel().unwrap();
    input_ch.send_message("msg1".into()).await.unwrap();
    input_ch.send_message("msg2".into()).await.unwrap();
    input_ch.send_message("msg3".into()).await.unwrap();

    // Run the route. It should fail eventually due to the simulated error,
    // but it MUST NOT deadlock.
    let run_fut = async {
        let (_shutdown_tx, shutdown_rx) = async_channel::bounded(1);
        route
            .run_until_err("deadlock_test", Some(shutdown_rx), None)
            .await
    };

    // If deadlock exists, this timeout will trigger.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_fut).await;

    match result {
        Ok(res) => {
            // We expect an error because the publisher returns Err.
            assert!(
                res.is_err(),
                "Route should have failed with simulated error"
            );
        }
        Err(_) => {
            panic!("Route deadlocked! The sequencer likely didn't receive the Nack for the failed batch.");
        }
    }
}

// Regression: once every worker had failed, the producer stayed blocked on the full
// work channel and the batches queued in it kept the commit sequencer alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_route_returns_when_every_worker_fails() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("always_fail_factory_{}", unique_id);

    #[derive(Debug)]
    struct AlwaysFailingFactory;

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for AlwaysFailingFactory {
        async fn apply_publisher(
            &self,
            _publisher: Box<dyn MessagePublisher>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(AlwaysFailingPublisher))
        }
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(consumer)
        }
    }

    struct AlwaysFailingPublisher;

    #[async_trait::async_trait]
    impl MessagePublisher for AlwaysFailingPublisher {
        async fn send_batch(
            &self,
            _messages: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            // Long enough for the producer to fill the work channel behind the workers.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Err(PublisherError::Retryable(anyhow::anyhow!("sink is down")))
        }
        async fn send(
            &self,
            _msg: crate::CanonicalMessage,
        ) -> Result<crate::traits::Sent, PublisherError> {
            Err(PublisherError::Retryable(anyhow::anyhow!("sink is down")))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    register_middleware_factory(&factory_name, Arc::new(AlwaysFailingFactory)).unwrap();

    let input = Endpoint::new_memory(&format!("all_fail_in_{}", unique_id), 100);
    let output = Endpoint::new_memory(&format!("all_fail_out_{}", unique_id), 100).add_middleware(
        Middleware::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
    );
    let route = Route::new(input.clone(), output)
        .with_concurrency(4)
        .with_batch_size(1);

    let input_ch = input.channel().unwrap();
    for i in 0..40 {
        input_ch
            .send_message(format!("msg{i}").into())
            .await
            .unwrap();
    }

    let (_shutdown_tx, shutdown_rx) = async_channel::bounded(1);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        route.run_until_err("all_workers_fail_test", Some(shutdown_rx), None),
    )
    .await
    .expect("route hung after every worker failed");
    assert!(result.is_err(), "route should report the sink failure");
}

#[tokio::test]
async fn test_sequencer_ordered_commits() {
    use std::time::Duration;
    use tokio::time::timeout;

    let (seq_tx, sequencer_handle) = spawn_sequencer(16);
    let processed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    // Send sequences out of order to ensure the sequencer enforces ordering.
    let seqs = [2u64, 0u64, 1u64, 3u64];
    let mut receivers = Vec::new();

    for seq in seqs.iter().cloned() {
        let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
        let processed_clone = processed.clone();
        let commit: BatchCommitFunc = Box::new(move |_dispositions| {
            let processed = processed_clone.clone();
            Box::pin(async move {
                // Simulate variable work durations
                tokio::time::sleep(Duration::from_millis(10 * seq)).await;
                processed.lock().unwrap().push(seq);
                Ok(())
            })
        });
        seq_tx
            .send((seq, (Vec::new(), commit, notify_tx)))
            .await
            .unwrap();
        receivers.push(notify_rx);
    }

    // Wait for all commits to complete (with timeout to catch deadlocks)
    for rx in receivers {
        let res = timeout(Duration::from_secs(2), rx)
            .await
            .expect("Sequencer notify timed out");
        assert!(res.is_ok(), "Sequencer reported an error on commit");
        assert!(res.unwrap().is_ok(), "Commit returned an error");
    }

    // Close sender to allow sequencer task to exit and await it.
    drop(seq_tx);
    let _ = sequencer_handle.await;

    let result = processed.lock().unwrap().clone();
    assert_eq!(
        result,
        vec![0u64, 1u64, 2u64, 3u64],
        "Sequencer must process commits in order"
    );
}

#[tokio::test]
async fn test_sequencer_shutdown_notifies_pending() {
    use std::time::Duration;
    use tokio::time::timeout;

    let (seq_tx, sequencer_handle) = spawn_sequencer(8);

    // Prepare two pending items for sequences 1 and 2 while sequence 0 is missing.
    let (notify_tx1, notify_rx1) = tokio::sync::oneshot::channel();
    let (notify_tx2, notify_rx2) = tokio::sync::oneshot::channel();

    let commit1: BatchCommitFunc = Box::new(|_dispositions| {
        Box::pin(async move {
            // Should not be executed because next_seq is missing (0)
            panic!("Commit should not be executed during shutdown drain");
            #[allow(unreachable_code)]
            Ok(())
        })
    });

    let commit2: BatchCommitFunc = Box::new(|_dispositions| {
        Box::pin(async move {
            panic!("Commit should not be executed during shutdown drain");
            #[allow(unreachable_code)]
            Ok(())
        })
    });

    seq_tx
        .send((1u64, (Vec::new(), commit1, notify_tx1)))
        .await
        .unwrap();
    seq_tx
        .send((2u64, (Vec::new(), commit2, notify_tx2)))
        .await
        .unwrap();

    // Trigger shutdown of the sequencer by dropping the sender.
    drop(seq_tx);

    // Sequencer should drain buffered items and reply with an error to the notifiers.
    let r1 = timeout(Duration::from_secs(1), notify_rx1)
        .await
        .expect("Timeout waiting for notify_rx1")
        .expect("Sequencer closed notify channel");
    assert!(
        r1.is_err(),
        "Pending commit should receive Err on sequencer shutdown"
    );

    let r2 = timeout(Duration::from_secs(1), notify_rx2)
        .await
        .expect("Timeout waiting for notify_rx2")
        .expect("Sequencer closed notify channel");
    assert!(
        r2.is_err(),
        "Pending commit should receive Err on sequencer shutdown"
    );

    let _ = sequencer_handle.await;
}

use crate::traits::{BoxFuture, CustomEndpointFactory, Sent};
use std::sync::Mutex;

type ConsumerBehavior =
    Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessageConsumer>, anyhow::Error> + Send + Sync>>;
type PublisherBehavior =
    Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessagePublisher>, anyhow::Error> + Send + Sync>>;

struct MockEndpointFactory {
    create_consumer_fail: bool,
    consumer_behavior: ConsumerBehavior,
    publisher_behavior: PublisherBehavior,
}

impl std::fmt::Debug for MockEndpointFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockEndpointFactory")
            .field("create_consumer_fail", &self.create_consumer_fail)
            .finish()
    }
}

impl MockEndpointFactory {
    fn new() -> Self {
        Self {
            create_consumer_fail: false,
            consumer_behavior: Arc::new(Mutex::new(|| Err(anyhow::anyhow!("Not implemented")))),
            publisher_behavior: Arc::new(Mutex::new(|| {
                Ok(Box::new(NoOpPublisher) as Box<dyn MessagePublisher>)
            })),
        }
    }
}

#[derive(Clone)]
struct NoOpPublisher;
#[async_trait::async_trait]
impl MessagePublisher for NoOpPublisher {
    async fn send_batch(
        &self,
        _: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        Ok(SentBatch::Ack)
    }
    async fn send(&self, _: crate::CanonicalMessage) -> Result<Sent, PublisherError> {
        Ok(Sent::Ack)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait::async_trait]
impl CustomEndpointFactory for MockEndpointFactory {
    async fn create_consumer(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        if self.create_consumer_fail {
            return Err(anyhow::anyhow!("Endpoint unavailable"));
        }
        (self.consumer_behavior.lock().unwrap())()
    }
    async fn create_publisher(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        (self.publisher_behavior.lock().unwrap())()
    }
}

#[derive(Clone, Default)]
struct HookState {
    consumer_connects: Arc<AtomicUsize>,
    consumer_disconnects: Arc<AtomicUsize>,
    publisher_connects: Arc<AtomicUsize>,
    publisher_disconnects: Arc<AtomicUsize>,
    shared_mutations: Arc<AtomicUsize>,
    fail_consumer_connect: Arc<AtomicBool>,
    fail_consumer_disconnect: Arc<AtomicBool>,
    fail_publisher_disconnect: Arc<AtomicBool>,
}

struct HookConsumer {
    state: HookState,
}

struct HookPublisher {
    state: HookState,
}

#[async_trait::async_trait]
impl MessageConsumer for HookConsumer {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            self.state.consumer_connects.fetch_add(1, Ordering::SeqCst);
            self.state.shared_mutations.fetch_add(1, Ordering::SeqCst);
            if self.state.fail_consumer_connect.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("consumer hook failed"));
            }
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            self.state
                .consumer_disconnects
                .fetch_add(1, Ordering::SeqCst);
            if self.state.fail_consumer_disconnect.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("consumer disconnect hook failed"));
            }
            Ok(())
        }))
    }

    async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
        Err(ConsumerError::EndOfStream)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait::async_trait]
impl MessagePublisher for HookPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            self.state.publisher_connects.fetch_add(1, Ordering::SeqCst);
            self.state.shared_mutations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            self.state
                .publisher_disconnects
                .fetch_add(1, Ordering::SeqCst);
            if self.state.fail_publisher_disconnect.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("publisher disconnect hook failed"));
            }
            Ok(())
        }))
    }

    async fn send_batch(
        &self,
        _: Vec<crate::CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn hook_route(state: HookState, concurrency: usize) -> Route {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("hooks_{}", unique_id);
    let mut factory = MockEndpointFactory::new();

    let consumer_state = state.clone();
    factory.consumer_behavior = Arc::new(Mutex::new(move || {
        Ok(Box::new(HookConsumer {
            state: consumer_state.clone(),
        }) as Box<dyn MessageConsumer>)
    }));

    let publisher_state = state;
    factory.publisher_behavior = Arc::new(Mutex::new(move || {
        Ok(Box::new(HookPublisher {
            state: publisher_state.clone(),
        }) as Box<dyn MessagePublisher>)
    }));

    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name.clone(),
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    Route::new(input, output).with_concurrency(concurrency)
}

#[tokio::test]
async fn test_lifecycle_hooks_called_once_sequentially() {
    let state = HookState::default();
    let route = hook_route(state.clone(), 1);

    let stopped_by_shutdown = route
        .run_until_err("test_lifecycle_sequential", None, None)
        .await
        .unwrap();

    assert!(!stopped_by_shutdown);
    assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
    assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
    assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(state.shared_mutations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_final_commit_failure_after_end_of_stream_fails_sequential_route() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("final_commit_failure_{unique_id}");
    let eof_seen = Arc::new(tokio::sync::Notify::new());
    let consumer_eof_seen = eof_seen.clone();
    let mut factory = MockEndpointFactory::new();
    let reads = Arc::new(AtomicUsize::new(0));
    let consumer_reads = reads.clone();
    factory.consumer_behavior = Arc::new(Mutex::new(move || {
        struct FiniteConsumer {
            reads: Arc<AtomicUsize>,
            eof_seen: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl MessageConsumer for FiniteConsumer {
            async fn receive_batch(&mut self, _: usize) -> Result<ReceivedBatch, ConsumerError> {
                if self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
                    self.eof_seen.notify_one();
                    return Err(ConsumerError::EndOfStream);
                }
                let eof_seen = self.eof_seen.clone();
                Ok(ReceivedBatch {
                    messages: vec![crate::CanonicalMessage::from("only")],
                    commit: Box::new(move |_| {
                        Box::pin(async move {
                            eof_seen.notified().await;
                            Err(anyhow::anyhow!("final commit failed"))
                        })
                    }),
                })
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(FiniteConsumer {
            reads: consumer_reads.clone(),
            eof_seen: consumer_eof_seen.clone(),
        }) as Box<dyn MessageConsumer>)
    }));

    let disconnect_outcome = Arc::new(Mutex::new(None));
    let publisher_outcome = disconnect_outcome.clone();
    factory.publisher_behavior = Arc::new(Mutex::new(move || {
        struct OutcomePublisher(Arc<Mutex<Option<DisconnectOutcome>>>);
        #[async_trait::async_trait]
        impl MessagePublisher for OutcomePublisher {
            fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
                Some(Box::pin(async move {
                    *self.0.lock().unwrap() = crate::traits::disconnect_outcome();
                    Ok(())
                }))
            }
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Ok(SentBatch::Ack)
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(OutcomePublisher(publisher_outcome.clone())) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let endpoint = |name: String| Endpoint {
        endpoint_type: EndpointType::Custom {
            name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(endpoint(factory_name.clone()), endpoint(factory_name));

    let err = route
        .run_until_err("test_final_commit_failure", None, None)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("final commit failed"));
    assert_eq!(
        *disconnect_outcome.lock().unwrap(),
        Some(DisconnectOutcome::Failed)
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_lifecycle_hooks_called_once_concurrently() {
    let state = HookState::default();
    let route = hook_route(state.clone(), 4);

    route
        .run_until_err("test_lifecycle_concurrent", None, None)
        .await
        .unwrap();

    assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
    assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
    assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_lifecycle_on_connect_failure_stops_route() {
    let state = HookState::default();
    state.fail_consumer_connect.store(true, Ordering::SeqCst);
    let route = hook_route(state.clone(), 1);

    let err = route
        .run_until_err("test_lifecycle_connect_failure", None, None)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("on_connect hook failed"));
    assert_eq!(state.publisher_connects.load(Ordering::SeqCst), 1);
    assert_eq!(state.consumer_connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_lifecycle_on_disconnect_failure_does_not_stop_route() {
    let state = HookState::default();
    state.fail_consumer_disconnect.store(true, Ordering::SeqCst);
    state
        .fail_publisher_disconnect
        .store(true, Ordering::SeqCst);
    let route = hook_route(state.clone(), 1);

    let stopped_by_shutdown = route
        .run_until_err("test_lifecycle_disconnect_failure", None, None)
        .await
        .unwrap();

    assert!(!stopped_by_shutdown);
    assert_eq!(state.consumer_disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(state.publisher_disconnects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_forward_buffered_ready_delivers_a_signal_the_loop_never_observed() {
    // The terminal arms of the reconnect loop can win the `select!` race against a
    // ready signal the run task already buffered. Forwarding it from there is what
    // keeps a route that ran to completion from being reported as one that never
    // started.
    let (iter_ready_tx, iter_ready_rx) = bounded::<()>(1);
    let (ready_tx, ready_rx) = bounded::<()>(1);
    iter_ready_tx.send(()).await.unwrap();

    let mut startup_notified = false;
    forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
    assert!(startup_notified);
    assert!(
        ready_rx.try_recv().is_ok(),
        "buffered ready must reach `run()`"
    );

    // `run()` consumes the startup channel once, so a second send would sit unread.
    iter_ready_tx.send(()).await.unwrap();
    forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
    assert!(
        ready_rx.try_recv().is_err(),
        "startup must not be notified twice"
    );
}

#[tokio::test]
async fn test_forward_buffered_ready_keeps_a_never_ready_route_a_failure() {
    let (_iter_ready_tx, iter_ready_rx) = bounded::<()>(1);
    let (ready_tx, ready_rx) = bounded::<()>(1);

    let mut startup_notified = false;
    forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
    assert!(!startup_notified);
    assert!(ready_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_start_fails_on_unavailable_endpoint() {
    // tokio::time::pause();
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("unavailable_{}", unique_id);

    let factory = Arc::new(MockEndpointFactory {
        create_consumer_fail: true,
        ..MockEndpointFactory::new()
    });
    register_endpoint_factory(&factory_name, factory).unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let output = Endpoint::new_memory("out", 10);
    let route = Route::new(input, output);

    // The route should fail to start because the input endpoint fails to create.
    // The run() method waits for a ready signal which never comes.
    let result = route.run("test_start_fail").await;
    let err = result.expect_err("must fail").to_string();
    // A stalled connect is a timeout, not a route that ended: the two now read
    // differently, and the recorded cause rides along.
    assert!(
        err.contains("failed to start: did not become ready within 5000ms")
            && err.contains("Endpoint unavailable"),
        "got: {err}"
    );
}

#[tokio::test]
async fn test_start_failure_distinguishes_an_ended_route_from_a_timeout() {
    // A drain gives up after DRAIN_MAX_RECONNECT_ATTEMPTS, so the route task ends
    // long before the startup timeout. `ready_rx` then fails immediately, and
    // calling that a 5000ms timeout sends the reader after a stall that never
    // happened.
    let factory_name = format!("ended_before_ready_{}", fast_uuid_v7::gen_id());
    register_endpoint_factory(
        &factory_name,
        Arc::new(MockEndpointFactory {
            create_consumer_fail: true,
            ..MockEndpointFactory::new()
        }),
    )
    .unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(input, Endpoint::new_memory("out", 10))
        .with_exit_on_empty(true)
        .with_reconnect_interval_ms(1);

    let err = route
        .run("test_ended_before_ready")
        .await
        .expect_err("must fail")
        .to_string();
    assert!(
        err.contains("failed to start: ended before it signalled ready")
            && err.contains("outcome: failed"),
        "got: {err}"
    );
}

#[tokio::test]
async fn test_display_name_replaces_the_route_key_in_a_start_failure() {
    let factory_name = format!("display_name_{}", fast_uuid_v7::gen_id());
    register_endpoint_factory(
        &factory_name,
        Arc::new(MockEndpointFactory {
            create_consumer_fail: true,
            ..MockEndpointFactory::new()
        }),
    )
    .unwrap();
    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(input, Endpoint::new_memory("out", 10))
        .with_exit_on_empty(true)
        .with_reconnect_interval_ms(1);

    set_route_display_name("test_display_name_long_key", "short");
    let err = route
        .run("test_display_name_long_key")
        .await
        .expect_err("must fail")
        .to_string();
    assert!(
        err.contains("Route 'short' failed to start")
            && !err.contains("test_display_name_long_key"),
        "got: {err}"
    );
    assert_eq!(
        route_display_name("no_display_name_set"),
        "no_display_name_set"
    );
    // Only the embedded route name changes, not the same word elsewhere in the text.
    assert_eq!(
        show_route_name(
            "[route:orders] Route 'orders': table orders not found".to_string(),
            "orders",
            "short"
        ),
        "[route:short] Route 'short': table orders not found"
    );
}

#[tokio::test]
async fn test_reconnect_on_consumer_error() {
    // tokio::time::pause();
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("reconnect_{}", unique_id);

    // Shared state to track connection attempts
    let connection_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = connection_attempts.clone();

    let consumer_logic = move || -> Result<Box<dyn MessageConsumer>, anyhow::Error> {
        let attempt = attempts_clone.fetch_add(1, Ordering::SeqCst);

        struct FlakyConsumer {
            attempt: usize,
        }
        #[async_trait::async_trait]
        impl MessageConsumer for FlakyConsumer {
            async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
                if self.attempt == 0 {
                    // First connection works for one batch, then fails
                    self.attempt = 999; // prevent infinite loop in this instance
                    Ok(ReceivedBatch {
                        messages: vec![crate::CanonicalMessage::from("msg1")],
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    })
                } else if self.attempt == 999 {
                    // Simulate connection drop
                    Err(ConsumerError::Connection(anyhow::anyhow!(
                        "Connection dropped"
                    )))
                } else {
                    // Subsequent connections work
                    // Sleep a bit to prevent busy loop in test
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(ReceivedBatch {
                        messages: vec![crate::CanonicalMessage::from("msg2")],
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    })
                }
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(FlakyConsumer { attempt }))
    };

    let mut factory = MockEndpointFactory::new();
    factory.consumer_behavior = Arc::new(Mutex::new(consumer_logic));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let output = Endpoint::new_memory(&format!("out_{}", unique_id), 10);
    let route = Route::new(input, output.clone());

    route.deploy("test_reconnect").await.unwrap();

    // Wait for reconnection and messages
    let mut verifier = create_consumer_from_route("verifier", &output)
        .await
        .unwrap();

    // Should receive msg1
    let msg1 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
        .await
        .expect("Timed out waiting for msg1")
        .unwrap();
    assert_eq!(msg1.message.get_payload_str(), "msg1");

    // Route encounters error, sleeps 5s (skipped by pause), reconnects.
    // Should receive msg2
    let msg2 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
        .await
        .expect("Timed out waiting for msg2")
        .unwrap();
    assert_eq!(msg2.message.get_payload_str(), "msg2");

    assert!(connection_attempts.load(Ordering::SeqCst) >= 2);
    Route::stop("test_reconnect").await;
}

#[tokio::test]
async fn test_route_handle_status_reports_async_connection_failure() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("status_fail_{}", unique_id);

    // Consumer is created successfully (so `run()` reports ready and returns Ok),
    // but every `receive_batch` fails with a retryable Connection error — mirroring
    // an endpoint that connects on a background thread and fails asynchronously.
    let consumer_logic = move || -> Result<Box<dyn MessageConsumer>, anyhow::Error> {
        struct FailingConsumer;
        #[async_trait::async_trait]
        impl MessageConsumer for FailingConsumer {
            async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
                Err(ConsumerError::Connection(anyhow::anyhow!(
                    "queue manager unreachable"
                )))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(FailingConsumer))
    };

    let mut factory = MockEndpointFactory::new();
    factory.consumer_behavior = Arc::new(Mutex::new(consumer_logic));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let output = Endpoint::new_memory(&format!("out_{}", unique_id), 10);
    let mut route = Route::new(input, output);
    // Long enough to observe the unhealthy/reconnecting state, short enough that the
    // backoff sleep (which the loop can't interrupt) doesn't slow the test down.
    route.options.reconnect_interval_ms = 2_000;

    // `run()` returns Ok because the consumer was created and ready was signalled.
    let handle = route.run("test_status_fail").await.unwrap();

    // The async receive failure should flip the live status to unhealthy.
    let mut status = handle.status();
    for _ in 0..50 {
        if !status.healthy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = handle.status();
    }

    assert!(!status.healthy, "expected route to report unhealthy status");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("queue manager unreachable"),
        "expected last connection error in status, got {:?}",
        status.error
    );

    handle.stop().await;
    let _ = handle.join().await;
}

// N3 (regression): `retry` used to wrap the handler, so a sink that retried N times
// ran the handler N times for the same message — every side effect in it repeated.
// The handler now sits outside the middlewares and runs once; only the publish is
// retried.
#[tokio::test]
async fn test_retry_does_not_re_invoke_the_handler() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("retry_handler_{}", unique_id);

    let sends = Arc::new(AtomicUsize::new(0));
    let sends_pub = sends.clone();
    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(move || {
        struct CountingFailPublisher(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl MessagePublisher for CountingFailPublisher {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(PublisherError::Retryable(anyhow::anyhow!("sink down")))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(CountingFailPublisher(sends_pub.clone())) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let handler_calls = Arc::new(AtomicUsize::new(0));
    let handler_calls_inner = handler_calls.clone();
    let handler = move |msg: crate::CanonicalMessage| {
        let calls = handler_calls_inner.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::Handled::Publish(msg))
        }
    };

    let in_topic = format!("retry_handler_in_{}", unique_id);
    let input = Endpoint::new_memory(&in_topic, 10);
    let input_ch = input.channel().unwrap();

    const ATTEMPTS: usize = 4;
    let mut output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    output
        .middlewares
        .push(Middleware::Retry(crate::models::RetryMiddleware {
            max_attempts: ATTEMPTS,
            initial_interval_ms: 1,
            max_interval_ms: 2,
            multiplier: 1.0,
        }));

    let route = Route::new(input, output).with_handler(handler);
    route.deploy("test_retry_handler_once").await.unwrap();
    input_ch.send_message("one".into()).await.unwrap();

    // Wait for the sink to have exhausted its attempts.
    for _ in 0..200 {
        if sends.load(Ordering::SeqCst) >= ATTEMPTS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        sends.load(Ordering::SeqCst) >= ATTEMPTS,
        "expected the publish itself to be retried"
    );
    assert_eq!(
        handler_calls.load(Ordering::SeqCst),
        1,
        "the handler must run once per message, not once per retry attempt"
    );

    Route::stop("test_retry_handler_once").await;
}

#[tokio::test]
async fn test_a_send_that_never_returns_reports_the_route_unhealthy() {
    let factory_name = format!("stuck_output_{}", fast_uuid_v7::gen_id());
    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(|| {
        struct StuckPublisher;
        #[async_trait::async_trait]
        impl MessagePublisher for StuckPublisher {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                std::future::pending().await
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(StuckPublisher) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let input = Endpoint::new_memory("stuck_send_in", 10);
    let input_ch = input.channel().unwrap();
    let output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let handle = Route::new(input, output)
        .run("test_stuck_send_health")
        .await
        .unwrap();
    let threshold = Duration::from_millis(50);
    assert!(handle.status_with_stall_threshold(threshold).healthy);

    input_ch.send_message("one".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let status = handle.status_with_stall_threshold(threshold);
    assert!(!status.healthy, "a stuck send must not report healthy");
    assert_eq!(status.details["state"], "send pending");
    assert!(status.details["send_pending_ms"].as_u64().unwrap() >= 50);

    handle.stop().await;
}

// N5 (regression): a drain route whose output leg is at a dead address used to
// reconnect forever — `finished: false`, no outcome, and `healthy: true` on every
// poll, because reconnecting flipped health back the moment the consumer was ready.
// It must now report unhealthy while it flaps and end as `Failed`.
#[tokio::test]
async fn test_drain_route_with_a_dead_output_fails_instead_of_reconnecting_forever() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("dead_output_{}", unique_id);

    // Constructs fine (so the route reports ready), then fails every send with a
    // connection error — an HTTP sink pointed at a closed port behaves this way.
    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(|| {
        struct DeadPublisher;
        #[async_trait::async_trait]
        impl MessagePublisher for DeadPublisher {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Err(PublisherError::Connection(anyhow::anyhow!(
                    "connection refused"
                )))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(DeadPublisher) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    // A file source, like the reported repro: it re-reads the same records after a
    // reconnect, so the failing batch comes back around every pass. That is what
    // turned "retry the connection" into "never finish".
    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("in.jsonl");
    std::fs::write(&in_path, "a\nb\nc\n").unwrap();
    let input = Endpoint::new(EndpointType::File(crate::models::FileConfig {
        path: in_path.to_str().unwrap().to_string(),
        format: crate::models::FileFormat::Raw,
        ..Default::default()
    }));

    let output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(input, output)
        .with_exit_on_empty(true)
        .with_reconnect_interval_ms(10);

    let handle = route.run("test_dead_output_drain").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("drain route reconnected forever instead of giving up");

    assert_eq!(handle.outcome(), Some(RouteOutcome::Failed));
    let status = handle.status();
    assert!(!status.healthy, "a wedged route must not report healthy");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("connection refused"),
        "status should carry the leg's error, got {:?}",
        status.error
    );

    Route::stop("test_dead_output_drain").await;
}

// A drain copies what is there: a sink that never accepted anything is not waited for.
#[tokio::test]
async fn test_drain_does_not_reconnect_before_anything_was_delivered() {
    let factory_name = format!("never_up_{}", fast_uuid_v7::gen_id());
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_pub = attempts.clone();

    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(move || {
        struct NeverUp(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl MessagePublisher for NeverUp {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(PublisherError::Retryable(anyhow::anyhow!("no such stream")))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(NeverUp(attempts_pub.clone())) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("in.jsonl");
    std::fs::write(&in_path, "a\nb\nc\n").unwrap();
    let input = Endpoint::new(EndpointType::File(crate::models::FileConfig {
        path: in_path.to_str().unwrap().to_string(),
        format: crate::models::FileFormat::Raw,
        ..Default::default()
    }));
    let output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(input, output)
        .with_exit_on_empty(true)
        .with_reconnect_interval_ms(10);

    let handle = route.run("test_drain_no_reconnect").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the drain should end on its first failure");

    assert_eq!(handle.outcome(), Some(RouteOutcome::Failed));
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "no second pass");
    Route::stop("test_drain_no_reconnect").await;
}

// Regression: with `concurrency > 1`, a source that reports `EndOfStream` used to
// fall through to `Ok(shutdown_rx.is_empty())` at the end of `run_concurrently`.
// No shutdown had been requested, so that read as "reconnect" and the outer loop
// reran the source from the top, forever — `copy --drain` from a MongoDB snapshot
// never exited and rewrote its whole sink on every pass. The sequential runner
// always got this right, so the bug only showed above concurrency 1.
#[tokio::test]
async fn test_end_of_stream_completes_a_concurrent_route_instead_of_reconnecting() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("eos_concurrent_{}", unique_id);

    // One batch, then end of stream — a finite source, like a snapshot read.
    let batches = Arc::new(AtomicUsize::new(0));
    let consumer_batches = batches.clone();
    let mut factory = MockEndpointFactory::new();
    factory.consumer_behavior = Arc::new(Mutex::new(move || {
        struct FiniteConsumer {
            batches: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl MessageConsumer for FiniteConsumer {
            async fn receive_batch(&mut self, _: usize) -> Result<ReceivedBatch, ConsumerError> {
                if self.batches.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Err(ConsumerError::EndOfStream);
                }
                Ok(ReceivedBatch {
                    messages: vec![crate::CanonicalMessage::from("only")],
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                })
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(FiniteConsumer {
            batches: consumer_batches.clone(),
        }) as Box<dyn MessageConsumer>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let input = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };
    let route = Route::new(input, Endpoint::new(EndpointType::Null))
        .with_concurrency(4)
        .with_reconnect_interval_ms(10);

    let handle = route.run("test_eos_concurrent").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a drained source must end the route, not restart it");

    assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));
    // Two reads: the batch and the end-of-stream. A restart would read again.
    assert_eq!(
        batches.load(Ordering::SeqCst),
        2,
        "the route reconnected and reread the source after end of stream"
    );

    Route::stop("test_eos_concurrent").await;
}

// N5 as originally reported: the dead leg sits inside a `fanout` next to a working
// one. `FanoutPublisher::send_batch` propagates a hard error from any leg, so this
// must reach the same bound rather than wedging at `healthy: true` forever.
#[tokio::test]
async fn test_drain_fanout_with_one_dead_leg_fails_instead_of_wedging() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("dead_leg_{}", unique_id);

    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(|| {
        struct DeadLeg;
        #[async_trait::async_trait]
        impl MessagePublisher for DeadLeg {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Err(PublisherError::Connection(anyhow::anyhow!(
                    "dead leg refused"
                )))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(DeadLeg) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("in5.jsonl");
    std::fs::write(&in_path, "a\nb\nc\nd\ne\n").unwrap();
    let good_path = dir.path().join("fan_ok.jsonl");

    let input = Endpoint::new(EndpointType::File(crate::models::FileConfig {
        path: in_path.to_str().unwrap().to_string(),
        format: crate::models::FileFormat::Raw,
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::Fanout(vec![
        Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: good_path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        })),
        Endpoint::new(EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        }),
    ]));

    let handle = Route::new(input, output)
        .with_exit_on_empty(true)
        .with_batch_size(1)
        .with_reconnect_interval_ms(10)
        .run("test_dead_fanout_leg")
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(15), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fanout with a dead leg wedged instead of giving up");

    assert_eq!(handle.outcome(), Some(RouteOutcome::Failed));
    let status = handle.status();
    assert!(!status.healthy, "a wedged fanout must not report healthy");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("dead leg refused"),
        "status should carry the failing leg's error, got {:?}",
        status.error
    );

    Route::stop("test_dead_fanout_leg").await;
}

/// A fanout leg carrying its own `retry` + `dlq` must dead-letter its failures exactly
/// like the same endpoint would as a route's sole output.
#[tokio::test]
async fn test_fanout_leg_middleware_dead_letters_its_own_failures() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("failing_leg_{}", unique_id);

    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(|| {
        struct AlwaysFails;
        #[async_trait::async_trait]
        impl MessagePublisher for AlwaysFails {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Err(PublisherError::Retryable(anyhow::anyhow!("leg refused")))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        Ok(Box::new(AlwaysFails) as Box<dyn MessagePublisher>)
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("fan_in.jsonl");
    std::fs::write(&in_path, "a\n").unwrap();
    let good_path = dir.path().join("fan_good.jsonl");
    let dlq_path = dir.path().join("fan_dlq.jsonl");

    let raw_file = |path: &std::path::Path| {
        Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        }))
    };

    let mut failing_leg = Endpoint::new(EndpointType::Custom {
        name: factory_name,
        config: serde_json::Value::Null,
    });
    failing_leg.middlewares = vec![
        crate::models::Middleware::Retry(crate::models::RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 1,
            max_interval_ms: 2,
            multiplier: 1.0,
        }),
        crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
            endpoint: raw_file(&dlq_path),
        })),
    ];

    let output = Endpoint::new(EndpointType::Fanout(vec![
        raw_file(&good_path),
        failing_leg,
    ]));

    let handle = Route::new(raw_file(&in_path), output)
        .with_exit_on_empty(true)
        .with_batch_size(1)
        .with_reconnect_interval_ms(10)
        .run("test_fanout_leg_dlq")
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(15), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("route never finished");
    Route::stop("test_fanout_leg_dlq").await;

    let dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
    assert!(
        dlq.contains('a'),
        "the fanout leg's own dlq never received the failed message, got {dlq:?}"
    );
}

/// The reported N20 shape verbatim: a real `http` leg pointed at a closed port. The mock
/// above only proves the wiring; this proves the classification an unreachable sink
/// actually produces still reaches that leg's dlq.
#[cfg(feature = "http")]
#[tokio::test]
async fn test_fanout_http_leg_dead_letters_a_connection_refusal() {
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("http_fan_in.jsonl");
    std::fs::write(&in_path, "a\n").unwrap();
    let good_path = dir.path().join("http_fan_good.jsonl");
    let dlq_path = dir.path().join("http_fan_dlq.jsonl");

    let raw_file = |path: &std::path::Path| {
        Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        }))
    };

    let mut dead_leg = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: "http://127.0.0.1:1/dead".to_string(),
        request_timeout_ms: Some(500),
        ..Default::default()
    }));
    dead_leg.middlewares = vec![
        crate::models::Middleware::Retry(crate::models::RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 1,
            max_interval_ms: 2,
            multiplier: 1.0,
        }),
        crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
            endpoint: raw_file(&dlq_path),
        })),
    ];

    let output = Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), dead_leg]));

    let handle = Route::new(raw_file(&in_path), output)
        .with_exit_on_empty(true)
        .with_batch_size(1)
        .with_reconnect_interval_ms(10)
        .run("test_fanout_http_leg_dlq")
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(30), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("route never finished");
    Route::stop("test_fanout_http_leg_dlq").await;

    let dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
    assert!(
        dlq.contains('a'),
        "the http leg's own dlq never received the failed message, got {dlq:?}"
    );
}

/// A `Connection` failure is deliberately never dead-lettered: `retry` does not retry it
/// and `dlq` propagates it so the route reconnects instead of dead-lettering a whole
/// backlog while a sink is merely down. That has to hold identically inside and outside a
/// fanout — a leg must not be the reason a message stops reaching its dlq.
#[tokio::test]
async fn test_connection_failures_bypass_the_dlq_identically_in_and_out_of_a_fanout() {
    async fn dlq_content_for(in_fanout: bool) -> String {
        let factory_name = format!("conn_fail_{}", fast_uuid_v7::gen_id());
        let mut factory = MockEndpointFactory::new();
        factory.publisher_behavior = Arc::new(Mutex::new(|| {
            struct Disconnected;
            #[async_trait::async_trait]
            impl MessagePublisher for Disconnected {
                async fn send_batch(
                    &self,
                    _: Vec<crate::CanonicalMessage>,
                ) -> Result<SentBatch, PublisherError> {
                    Err(PublisherError::Connection(anyhow::anyhow!(
                        "Simulated connection loss"
                    )))
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(Disconnected) as Box<dyn MessagePublisher>)
        }));
        register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let in_path = dir.path().join("in.jsonl");
        std::fs::write(&in_path, "a\n").unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let good_path = dir.path().join("good.jsonl");
        let raw_file = |path: &std::path::Path| {
            Endpoint::new(EndpointType::File(crate::models::FileConfig {
                path: path.to_str().unwrap().to_string(),
                format: crate::models::FileFormat::Raw,
                ..Default::default()
            }))
        };

        let mut failing = Endpoint::new(EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        });
        failing.middlewares = vec![
            crate::models::Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                max_interval_ms: 2,
                multiplier: 1.0,
            }),
            crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: raw_file(&dlq_path),
            })),
        ];
        let output = if in_fanout {
            Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), failing]))
        } else {
            failing
        };

        let route_name = format!("conn_dlq_{in_fanout}");
        let handle = Route::new(raw_file(&in_path), output)
            .with_exit_on_empty(true)
            .with_batch_size(1)
            .with_reconnect_interval_ms(10)
            .run(&route_name)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            while handle.outcome().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        Route::stop(&route_name).await;
        std::fs::read_to_string(&dlq_path).unwrap_or_default()
    }

    assert_eq!(
        dlq_content_for(true).await,
        dlq_content_for(false).await,
        "a fanout leg must dead-letter a connection failure the same way a sole output does"
    );
}

/// Same as above but driven by an `http` webhook input, which is the shape the finding
/// was reported in — a request/reply consumer, not a file drain.
#[cfg(feature = "http")]
#[tokio::test]
async fn test_fanout_leg_dlq_works_behind_an_http_webhook_input() {
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dir = tempfile::tempdir().unwrap();
    let good_path = dir.path().join("wh_good.jsonl");
    let dlq_path = dir.path().join("wh_dlq.jsonl");
    let raw_file = |path: &std::path::Path| {
        Endpoint::new(EndpointType::File(crate::models::FileConfig {
            path: path.to_str().unwrap().to_string(),
            format: crate::models::FileFormat::Raw,
            ..Default::default()
        }))
    };

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let input = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: format!("127.0.0.1:{port}"),
        fire_and_forget: true,
        ..Default::default()
    }));

    let mut dead_leg = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: "http://127.0.0.1:1/dead".to_string(),
        request_timeout_ms: Some(500),
        ..Default::default()
    }));
    dead_leg.middlewares = vec![
        crate::models::Middleware::Retry(crate::models::RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 1,
            max_interval_ms: 2,
            multiplier: 1.0,
        }),
        crate::models::Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
            endpoint: raw_file(&dlq_path),
        })),
    ];
    let output = Endpoint::new(EndpointType::Fanout(vec![raw_file(&good_path), dead_leg]));

    let _handle = Route::new(input, output)
        .run("test_fanout_webhook_dlq")
        .await
        .unwrap();

    let addr = format!("127.0.0.1:{port}");
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/"))
        .body(r#"{"order_id":"o1"}"#)
        .send()
        .await
        .expect("webhook POST failed");

    let mut dlq = String::new();
    for _ in 0..200 {
        dlq = std::fs::read_to_string(&dlq_path).unwrap_or_default();
        if dlq.contains("o1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Route::stop("test_fanout_webhook_dlq").await;

    assert!(
        dlq.contains("o1"),
        "the fanout leg's dlq never received the failed message, got {dlq:?}"
    );
}

#[cfg(feature = "http")]
#[tokio::test]
async fn referenced_http_input_survives_transient_sink_failure_sequentially_and_concurrently() {
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
    let _ = rustls::crypto::ring::default_provider().install_default();

    for concurrency in [1, 4] {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let addr = format!("127.0.0.1:{port}");
        let input_name = format!("referenced_http_{}", fast_uuid_v7::gen_id());
        register_endpoint(
            &input_name,
            Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
                url: addr.clone(),
                request_timeout_ms: Some(1_000),
                ..Default::default()
            })),
        );
        let input = Endpoint::new(EndpointType::Ref(input_name));
        let output = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "http://127.0.0.1:1/dead".to_string(),
            request_timeout_ms: Some(250),
            pass_through_status: true,
            ..Default::default()
        }));
        let route_name = format!(
            "referenced_http_listener_survives_{concurrency}_{}",
            fast_uuid_v7::gen_id()
        );
        let handle = Route::new(input, output)
            .with_concurrency(concurrency)
            .with_reconnect_interval_ms(10)
            .run(&route_name)
            .await
            .unwrap();

        for _ in 0..200 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        for attempt in 1..=2 {
            let response = reqwest::Client::new()
                .post(format!("http://{addr}/test"))
                .body("x")
                .send()
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "concurrency {concurrency} request {attempt} could not reach listener: {e}"
                    )
                });
            assert_eq!(
                    response.status(),
                    reqwest::StatusCode::BAD_GATEWAY,
                    "concurrency {concurrency} request {attempt} should receive a request-level failure"
                );
            assert_eq!(
                handle.outcome(),
                None,
                "concurrency {concurrency} route stopped after request {attempt}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Route::stop(&route_name).await;
    }
}

#[test]
fn transient_sink_failures_keep_existing_etl_reconnect_policy() {
    let synchronous_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: "127.0.0.1:8081".to_string(),
        ..Default::default()
    }));
    let fire_and_forget_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: "127.0.0.1:8080".to_string(),
        fire_and_forget: true,
        ..Default::default()
    }));
    let streamable_http = Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
        url: "127.0.0.1:8082".to_string(),
        receive_streamable: true,
        ..Default::default()
    }));
    let grpc_server = Endpoint::new(EndpointType::Grpc(crate::models::GrpcConfig {
        url: "127.0.0.1:50051".to_string(),
        server_mode: true,
        ..Default::default()
    }));
    let websocket_server = Endpoint::new(EndpointType::WebSocket(crate::models::WebSocketConfig {
        url: "127.0.0.1:9000".to_string(),
        ..Default::default()
    }));

    for input in [
        &fire_and_forget_http,
        &streamable_http,
        &grpc_server,
        &websocket_server,
    ] {
        assert!(matches!(
            transient_publish_failure_policy(input, false),
            TransientPublishFailurePolicy::StopRoute
        ));
    }
    assert!(matches!(
        transient_publish_failure_policy(&synchronous_http, false),
        TransientPublishFailurePolicy::StopRoute
    ));

    let opted_in_nested_output = Endpoint::new(EndpointType::Fanout(vec![Endpoint::new(
        EndpointType::Http(crate::models::HttpConfig {
            url: "http://127.0.0.1:9001".to_string(),
            pass_through_status: true,
            ..Default::default()
        }),
    )]));
    assert!(output_passes_through_http_status("proxy", &opted_in_nested_output).unwrap());
    let mixed_output = Endpoint::new(EndpointType::Fanout(vec![
        Endpoint::new(EndpointType::Http(crate::models::HttpConfig {
            url: "http://127.0.0.1:9001".to_string(),
            pass_through_status: true,
            ..Default::default()
        })),
        Endpoint::new(EndpointType::Kafka(crate::models::KafkaConfig {
            url: "127.0.0.1:9092".to_string(),
            ..Default::default()
        })),
    ]));
    assert!(!output_passes_through_http_status("proxy", &mixed_output).unwrap());
    assert!(matches!(
        transient_publish_failure_policy(&synchronous_http, true),
        TransientPublishFailurePolicy::ReplyBadGateway
    ));

    let suffix = fast_uuid_v7::gen_id();
    let http_name = format!("policy_http_{suffix}");
    let http_alias = format!("policy_http_alias_{suffix}");
    register_endpoint(&http_name, synchronous_http);
    register_endpoint(&http_alias, Endpoint::new(EndpointType::Ref(http_name)));
    assert!(matches!(
        transient_publish_failure_policy(&Endpoint::new(EndpointType::Ref(http_alias)), true),
        TransientPublishFailurePolicy::ReplyBadGateway
    ));

    let memory_name = format!("policy_memory_{suffix}");
    register_endpoint(&memory_name, Endpoint::new_memory("policy-memory", 1));
    for input in [
        Endpoint::new(EndpointType::Ref(memory_name)),
        Endpoint::new(EndpointType::Ref(format!("missing_policy_input_{suffix}"))),
    ] {
        assert!(matches!(
            transient_publish_failure_policy(&input, true),
            TransientPublishFailurePolicy::StopRoute
        ));
    }
}

// The flip side of the test above: gating health on "the pass stayed up" must not
// leave a route that recovered stuck reporting the failure forever.
#[tokio::test(start_paused = true)]
async fn test_route_reports_healthy_again_after_it_recovers() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let factory_name = format!("recovering_{}", unique_id);

    // Fails the first connection, then serves a working publisher.
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut factory = MockEndpointFactory::new();
    factory.publisher_behavior = Arc::new(Mutex::new(move || {
        struct OneShotFail;
        #[async_trait::async_trait]
        impl MessagePublisher for OneShotFail {
            async fn send_batch(
                &self,
                _: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Err(PublisherError::Connection(anyhow::anyhow!("first flap")))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(Box::new(OneShotFail) as Box<dyn MessagePublisher>)
        } else {
            Ok(Box::new(NoOpPublisher) as Box<dyn MessagePublisher>)
        }
    }));
    register_endpoint_factory(&factory_name, Arc::new(factory)).unwrap();

    let in_topic = format!("recovering_in_{}", unique_id);
    let input = Endpoint::new_memory(&in_topic, 10);
    let input_ch = input.channel().unwrap();
    let output = Endpoint {
        endpoint_type: EndpointType::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        },
        middlewares: vec![],
        handler: None,
    };

    let handle = Route::new(input, output)
        .with_reconnect_interval_ms(10)
        .run("test_route_recovers")
        .await
        .unwrap();
    input_ch.send_message("one".into()).await.unwrap();

    // Provoke the failure, then let the replacement connection run past STABLE_RUN.
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::sleep(STABLE_RUN + Duration::from_secs(1)).await;

    let status = handle.status();
    assert!(
        status.healthy,
        "a route that recovered must not stay unhealthy: {:?}",
        status.error
    );
    assert!(status.error.is_none(), "stale error: {:?}", status.error);

    Route::stop("test_route_recovers").await;
}

#[tokio::test]
async fn test_non_retryable_handler_error_does_not_crash_route() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("bad_input_in_{}", unique_id);
    let out_topic = format!("bad_input_out_{}", unique_id); // Not used, but good practice

    let input = Endpoint::new_memory(&in_topic, 10);
    let output = Endpoint::new_memory(&out_topic, 10);

    // A handler that fails on specific input
    let handler = |msg: crate::CanonicalMessage| async move {
        if msg.get_payload_str() == "poison" {
            Err(HandlerError::NonRetryable(anyhow::anyhow!("Invalid input")))
        } else {
            Ok(crate::Handled::Publish(msg))
        }
    };

    let route = Route::new(input.clone(), output).with_handler(handler);
    route.deploy("test_invalid_input").await.unwrap();

    let input_ch = input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    input_ch.send_message("poison".into()).await.unwrap();

    input_ch.send_message("valid".into()).await.unwrap();

    // Verify the valid message was processed and published
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(msg) = out_channel.drain_messages().pop() {
                return msg;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Timed out waiting for valid message to be processed");
    assert_eq!(received.get_payload_str(), "valid");
    Route::stop("test_invalid_input").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dlq_and_retry_batch_integration() {
    use crate::models::{DeadLetterQueueMiddleware, Middleware, RetryMiddleware};
    use crate::traits::{MessagePublisher, PublisherError, SentBatch};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Mock publisher that fails messages with even-numbered IDs
    #[derive(Clone)]
    struct PartialFailPublisher {
        attempts: Arc<Mutex<HashMap<u128, usize>>>,
    }

    #[async_trait::async_trait]
    impl MessagePublisher for PartialFailPublisher {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut failed = Vec::new();
            let mut attempts = self.attempts.lock().unwrap();

            for msg in messages {
                let msg_num: u32 = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                    .unwrap()["id"]
                    .as_u64()
                    .unwrap() as u32;

                let attempt_count = attempts.entry(msg.message_id).or_insert(0);
                *attempt_count += 1;

                if msg_num.is_multiple_of(2) {
                    // Fail even numbers
                    failed.push((
                        msg,
                        PublisherError::Retryable(anyhow::anyhow!("simulated failure")),
                    ));
                }
                // Odd numbers succeed implicitly by not being in `failed`
            }

            Ok(SentBatch::from_failures(failed))
        }
        async fn send(
            &self,
            _msg: CanonicalMessage,
        ) -> Result<crate::traits::Sent, PublisherError> {
            unimplemented!()
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let in_topic = "batch_retry_dlq_in";
    let out_topic = "batch_retry_dlq_out";
    let dlq_topic = "batch_retry_dlq_dlq";

    let input = Endpoint::new_memory(in_topic, 10);
    let dlq_endpoint = Endpoint::new_memory(dlq_topic, 10);

    let mock_publisher = PartialFailPublisher {
        attempts: Arc::new(Mutex::new(HashMap::new())),
    };

    let mut output_with_middlewares = Endpoint::new_memory(out_topic, 10);
    output_with_middlewares.middlewares = vec![
        Middleware::Retry(RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 1,
            ..Default::default()
        }),
        Middleware::Dlq(Box::new(DeadLetterQueueMiddleware {
            endpoint: dlq_endpoint.clone(),
        })),
    ];

    let route = Route::new(input.clone(), output_with_middlewares).with_batch_size(4);
    // Inject the mock publisher into the route's output
    let final_publisher = crate::middleware::apply_middlewares_to_publisher(
        Box::new(mock_publisher.clone()),
        &route.output,
        "test_route",
    )
    .await
    .unwrap();

    // We need a way to run the route with our mocked publisher.
    // The simplest way is to manually drive the core logic.
    let (work_tx, work_rx) =
        async_channel::bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(1);
    let (seq_tx, _sequencer_handle) = spawn_sequencer(1);

    // Spawn a worker to process one batch
    tokio::spawn(async move {
        if let Ok((messages, commit)) = work_rx.recv().await {
            let batch_len = messages.len();
            match final_publisher.send_batch(messages).await {
                Ok(SentBatch::Ack) => {
                    let _ = commit(vec![MessageDisposition::Ack; batch_len]).await;
                }
                Ok(SentBatch::Partial { failed, .. }) => {
                    // In a real route, we'd map responses, but here we just care about failure.
                    let dispositions = if failed.is_empty() {
                        vec![MessageDisposition::Ack; batch_len]
                    } else {
                        // This is a simplification for the test. A real implementation
                        // would map dispositions based on message IDs.
                        vec![MessageDisposition::Nack; batch_len]
                    };
                    let _ = commit(dispositions).await;
                }
                Err(_) => {
                    let _ = commit(vec![MessageDisposition::Nack; batch_len]).await;
                }
            }
        }
    });

    let mut messages = Vec::new();
    for i in 1..=4 {
        // 1 (ok), 2 (fail), 3 (ok), 4 (fail)
        messages.push(CanonicalMessage::from_json(serde_json::json!({"id": i})).unwrap());
    }
    let commit = wrap_commit(
        Box::new(|_| Box::pin(async { Ok(()) })),
        0,
        seq_tx.clone(),
        None,
    );
    work_tx.send((messages, commit)).await.unwrap();

    let dlq_channel = dlq_endpoint.channel().unwrap();

    let start = std::time::Instant::now();
    while dlq_channel.len() < 2 {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let dlq_msgs = dlq_channel.drain_messages();

    assert_eq!(dlq_msgs.len(), 2, "Expected 2 messages to go to DLQ");

    let dlq_ids: std::collections::HashSet<u32> = dlq_msgs
        .iter()
        .map(|m| {
            serde_json::from_slice::<serde_json::Value>(&m.payload).unwrap()["id"]
                .as_u64()
                .unwrap() as u32
        })
        .collect();

    assert!(dlq_ids.contains(&2));
    assert!(dlq_ids.contains(&4));

    // Verify retry attempts
    let attempts = mock_publisher.attempts.lock().unwrap();
    // Messages 2 and 4 should be tried `max_attempts` times.
    assert_eq!(attempts.values().filter(|&&c| c == 2).count(), 2);
    // Messages 1 and 3 should be tried once.
    assert_eq!(attempts.values().filter(|&&c| c == 1).count(), 2);
}

/// A sink that rejects permanently with no DLQ configured drops the batch so
/// the route is not wedged re-reading a poison message — but the drop must
/// not be silent. The route still completes; it just carries the cause, so a
/// caller polling only `outcome` cannot mistake a discarded batch for a
/// clean delivery.
#[tokio::test(flavor = "multi_thread")]
async fn test_dropped_poison_batch_is_recorded_on_the_route_status() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("drop_in_{}", unique_id);
    let out_topic = format!("drop_out_{}", unique_id);
    let input = Endpoint::new_memory(&in_topic, 10);

    let mut output = Endpoint::new_memory(&out_topic, 10);
    output.middlewares = vec![Middleware::RandomPanic(RandomPanicMiddleware {
        mode: FaultMode::JsonFormatError,
        trigger_on_message: None,
        enabled: true,
        ..Default::default()
    })];

    let input_ch = input.channel().unwrap();
    input_ch.send_message("poison".into()).await.unwrap();

    let route = Route::new(input, output)
        .with_fault_injection(true)
        .with_exit_on_empty(true);
    let handle = route.run("test_dropped_poison_recorded").await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(outcome) = handle.outcome() {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the drain finishes rather than retrying the poison batch forever");

    assert_eq!(
        outcome,
        RouteOutcome::Completed,
        "dropping the batch lets the drain finish"
    );
    let error = handle.status().error.unwrap_or_default();
    assert!(
        error.contains("dropped 1 message"),
        "the discarded batch is reported, not silently swallowed: {error:?}"
    );
    assert!(
        error.contains("JSON format error"),
        "the reported cause names the underlying rejection: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_route_dlq_integration() {
    // Setup: Input -> [Panic(Disconnect) -> Retry -> DLQ] -> Output
    // Panic(Disconnect) simulates transient failure.
    // Retry handles it up to N times.
    // If max attempts reached, DLQ catches it.
    // Note: Middleware application order is [Panic, Retry, DLQ] in list to wrap as DLQ(Retry(Panic(Endpoint))).

    let unique_id = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("dlq_in_{}", unique_id);
    let out_topic = format!("dlq_out_{}", unique_id);
    let dlq_topic = format!("dlq_target_{}", unique_id);
    let input = Endpoint::new_memory(&in_topic, 10);
    let dlq_endpoint = Endpoint::new_memory(&dlq_topic, 10);

    let mut output = Endpoint::new_memory(&out_topic, 10);
    output.middlewares = vec![
        // Inner-most: Fail always
        Middleware::RandomPanic(RandomPanicMiddleware {
            mode: FaultMode::Timeout, // Returns Retryable error, does NOT cause route restart
            trigger_on_message: None, // Fail always
            enabled: true,
            ..Default::default()
        }),
        // Middle: Retry
        Middleware::Retry(crate::models::RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 10,
            max_interval_ms: 100,
            multiplier: 1.0,
        }),
        // Outer-most: DLQ
        Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
            endpoint: dlq_endpoint.clone(),
        })),
    ];

    let route = Route::new(input.clone(), output).with_fault_injection(true);
    route.deploy("test_dlq_integration").await.unwrap();

    // Send message
    let input_ch = input.channel().unwrap();
    input_ch.send_message("fail_msg".into()).await.unwrap();

    // Verify:
    // Output channel is empty (msg failed to go there)
    // DLQ channel has message

    let dlq_ch = dlq_endpoint.channel().unwrap();

    // Wait for DLQ
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let batch = dlq_ch.drain_messages();
            if !batch.is_empty() {
                return batch[0].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Timed out waiting for DLQ");

    assert_eq!(received.get_payload_str(), "fail_msg");

    let out_ch_target = mq_bridge::endpoints::memory::get_or_create_channel(
        &mq_bridge::models::MemoryConfig::new(&out_topic, None),
    );
    assert!(out_ch_target.is_empty(), "Message should not reach target");

    Route::stop("test_dlq_integration").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_large_message_handling() {
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let in_topic = format!("large_in_{}", unique_id);
    let out_topic = format!("large_out_{}", unique_id);

    let input = Endpoint::new_memory(&in_topic, 5); // Small capacity
    let output = Endpoint::new_memory(&out_topic, 5);

    let route = Route::new(input.clone(), output.clone());
    route.deploy("test_large_msg").await.unwrap();

    let large_payload = vec![b'x'; 5 * 1024 * 1024]; // 5MB
    let input_ch = input.channel().unwrap();

    input_ch
        .send_message(large_payload.clone().into())
        .await
        .unwrap();

    let mut verifier = route.connect_to_output("verifier").await.unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
        .await
        .expect("Timed out receiving large message")
        .unwrap();

    assert_eq!(received.message.payload.len(), large_payload.len());
    assert_eq!(received.message.payload, large_payload.as_slice());

    Route::stop("test_large_msg").await;
}

#[test]
fn test_map_responses_to_dispositions_unit() {
    test_map_responses_to_dispositions_logic();
}

// Creates a temporary SQLite file with a `messages` table, mirroring the
// setup used by the sqlx endpoint tests.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
async fn setup_drain_db() -> (tempfile::TempDir, String) {
    use sqlx::Connection;
    sqlx::any::install_default_drivers();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drain.db");

    #[cfg(windows)]
    let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
    #[cfg(not(windows))]
    let url = format!("sqlite://{}", path.to_str().unwrap());

    drop(tokio::fs::File::create(&path).await.unwrap());

    let mut conn = sqlx::AnyConnection::connect(&url).await.unwrap();
    sqlx::query(
        "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                payload BLOB NOT NULL,
                locked_until DATETIME,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    (dir, url)
}

// A sqlx -> memory route with exit_on_empty drains the source table and then
// completes on its own; all rows must arrive at the output.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
#[tokio::test]
async fn test_route_exit_on_empty_drains_and_stops() {
    use crate::endpoints::sqlx::SqlxPublisher;
    use crate::models::SqlxConfig;

    let (_dir, url) = setup_drain_db().await;
    let config = SqlxConfig {
        url: url.clone(),
        table: "messages".to_string(),
        delete_after_read: true,
        ..Default::default()
    };

    const N: usize = 25;
    let publisher = SqlxPublisher::new(&config).await.unwrap();
    for i in 0..N {
        let msg = CanonicalMessage::new(format!("row-{i}").into_bytes(), None);
        publisher.send(msg).await.unwrap();
    }

    let out_topic = format!("drain_out_{}", fast_uuid_v7::gen_id());
    let input = Endpoint::new(EndpointType::Sqlx(config));
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output)
        .with_batch_size(10)
        .with_exit_on_empty(true);

    // Drain the output concurrently so the route is never blocked on
    // memory-channel backpressure while it forwards rows.
    let mut verifier = route.connect_to_output("drain_verifier").await.unwrap();
    let collector = tokio::spawn(async move {
        let mut received = Vec::new();
        while received.len() < N {
            let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                .await
                .expect("timed out draining output")
                .expect("output stream closed early");
            received.push(item.message.get_payload_str().to_string());
            (item.commit)(MessageDisposition::Ack).await.unwrap();
        }
        received
    });

    let handle = route.run("drain_test").await.unwrap();

    // A supervisor holding the handle (rather than consuming it via `join`)
    // must be able to observe the drain, so poll the borrowing accessor.
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("route did not exit on its own after draining");

    // A drained batch job must report success, not just "no longer running".
    assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));

    // The route future must complete on its own once the table is drained.
    handle.join().await.expect("route task panicked");

    // All N rows must have been forwarded to the memory output.
    let received = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("timed out collecting output")
        .expect("collector task panicked");
    assert_eq!(received.len(), N);
}

// Regression: without exit_on_empty, a drained route keeps polling and does
// not terminate on an empty batch.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
#[tokio::test]
async fn test_route_without_exit_on_empty_keeps_running() {
    use crate::models::SqlxConfig;

    let (_dir, url) = setup_drain_db().await;
    let config = SqlxConfig {
        url: url.clone(),
        table: "messages".to_string(),
        delete_after_read: true,
        ..Default::default()
    };

    let out_topic = format!("drain_out_running_{}", fast_uuid_v7::gen_id());
    let input = Endpoint::new(EndpointType::Sqlx(config));
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output).with_batch_size(10);
    let handle = route.run("no_drain_test").await.unwrap();

    // Give the route time to poll past the drained (empty) table.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The route must still be running after the source is empty.
    assert_eq!(
        handle.outcome(),
        None,
        "route exited on empty batch without exit_on_empty set"
    );

    handle.stop().await;

    // An explicit stop must be distinguishable from a drain.
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("route did not exit after stop()");
    assert_eq!(handle.outcome(), Some(RouteOutcome::Stopped));

    Route::stop("no_drain_test").await;
}

// A plain SQLite table with an `id`/`payload` schema but NO `locked_until` lease
// column — used to exercise both queue-mode failure (Issue 3) and cursor-mode
// full-table reads (Issue 2).
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
async fn setup_plain_db(rows: usize) -> (tempfile::TempDir, String) {
    use sqlx::Connection;
    sqlx::any::install_default_drivers();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.db");

    #[cfg(windows)]
    let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
    #[cfg(not(windows))]
    let url = format!("sqlite://{}", path.to_str().unwrap());

    drop(tokio::fs::File::create(&path).await.unwrap());

    let mut conn = sqlx::AnyConnection::connect(&url).await.unwrap();
    sqlx::query("CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 0..rows {
        sqlx::query("INSERT INTO items (payload) VALUES (?)")
            .bind(format!("row-{i}"))
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn.close().await.unwrap();
    (dir, url)
}

// Issue 3 (regression): a queue-mode SQLx source (`delete_after_read`, no
// `cursor_column`) on a table lacking the `locked_until` lease column hits an
// unrecoverable "no such column" error. It must fail fast (RouteOutcome::Failed)
// instead of reconnecting every reconnect_interval_ms forever.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
#[tokio::test]
async fn test_route_sqlite_missing_locked_until_fails_fast() {
    use crate::models::SqlxConfig;

    let (_dir, url) = setup_plain_db(5).await;
    let config = SqlxConfig {
        url,
        table: "items".to_string(),
        delete_after_read: true, // queue-lease mode
        ..Default::default()
    };

    let out_topic = format!("plain_fail_{}", fast_uuid_v7::gen_id());
    let input = Endpoint::new(EndpointType::Sqlx(config));
    let output = Endpoint::new_memory(&out_topic, 10);
    let route = Route::new(input, output)
        .with_exit_on_empty(true)
        // Short interval: a reconnect-loop regression times out fast instead of hanging.
        .with_reconnect_interval_ms(100);

    let handle = route.run("plain_fail_test").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("route did not fail fast on the missing-column error");
    assert_eq!(
        handle.outcome(),
        Some(RouteOutcome::Failed),
        "an unrecoverable schema error must terminate the route as Failed"
    );

    Route::stop("plain_fail_test").await;
}

// Issue 2: SQLite gets a full-table read via `cursor_column`, exactly like
// Postgres (the ETL path) — read mode is config-driven, not driver-specific. A
// cursor-mode drain of the plain table reads every row and exits Completed.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
#[tokio::test]
async fn test_route_sqlite_cursor_column_drains_full_table() {
    use crate::models::SqlxConfig;

    const N: usize = 25;
    let (_dir, url) = setup_plain_db(N).await;
    let config = SqlxConfig {
        url,
        table: "items".to_string(),
        cursor_column: Some("id".to_string()),
        cursor_id: Some(format!("plain-cur-{}", fast_uuid_v7::gen_id())),
        ..Default::default()
    };

    let out_topic = format!("plain_cursor_{}", fast_uuid_v7::gen_id());
    let input = Endpoint::new(EndpointType::Sqlx(config));
    let output = Endpoint::new_memory(&out_topic, 10);
    let route = Route::new(input, output)
        .with_batch_size(10)
        .with_exit_on_empty(true);

    let mut verifier = route
        .connect_to_output("plain_cursor_verifier")
        .await
        .unwrap();
    let collector = tokio::spawn(async move {
        let mut received = 0usize;
        while received < N {
            let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                .await
                .expect("timed out draining output")
                .expect("output stream closed early");
            received += 1;
            (item.commit)(MessageDisposition::Ack).await.unwrap();
        }
        received
    });

    let handle = route.run("plain_cursor_test").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cursor route did not drain and exit");
    assert_eq!(handle.outcome(), Some(RouteOutcome::Completed));
    handle.join().await.expect("route task panicked");

    let received = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("timed out collecting output")
        .expect("collector task panicked");
    assert_eq!(received, N);
}

// exit_on_empty must stop the route in concurrent mode too, not just sequential:
// the drain reports empty so the reconnect loop stops instead of looping forever.
#[cfg(any(feature = "sqlite", feature = "sqlx"))]
#[tokio::test]
async fn test_route_exit_on_empty_drains_and_stops_concurrent() {
    use crate::endpoints::sqlx::SqlxPublisher;
    use crate::models::SqlxConfig;

    let (_dir, url) = setup_drain_db().await;
    let config = SqlxConfig {
        url: url.clone(),
        table: "messages".to_string(),
        delete_after_read: true,
        ..Default::default()
    };

    const N: usize = 25;
    let publisher = SqlxPublisher::new(&config).await.unwrap();
    for i in 0..N {
        let msg = CanonicalMessage::new(format!("row-{i}").into_bytes(), None);
        publisher.send(msg).await.unwrap();
    }

    let out_topic = format!("drain_out_conc_{}", fast_uuid_v7::gen_id());
    let input = Endpoint::new(EndpointType::Sqlx(config));
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output)
        .with_batch_size(10)
        .with_concurrency(4)
        .with_exit_on_empty(true);

    let mut verifier = route
        .connect_to_output("drain_verifier_conc")
        .await
        .unwrap();
    let collector = tokio::spawn(async move {
        let mut received = Vec::new();
        while received.len() < N {
            let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                .await
                .expect("timed out draining output")
                .expect("output stream closed early");
            received.push(item.message.get_payload_str().to_string());
            (item.commit)(MessageDisposition::Ack).await.unwrap();
        }
        received
    });

    let handle = route.run("drain_test_conc").await.unwrap();

    // The route future must complete on its own once the table is drained.
    tokio::time::timeout(Duration::from_secs(10), handle.join())
        .await
        .expect("concurrent route did not exit on its own after draining")
        .expect("route task panicked");

    let received = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("timed out collecting output")
        .expect("collector task panicked");
    assert_eq!(received.len(), N);
}

// A file -> memory route with exit_on_empty must drain the file and then stop
// on its own. Parameterized over the two watcher-backed consumer modes:
// Consume{delete:false} (Tail backend) and Consume{delete:true} (Queue backend).
async fn run_file_exit_on_empty(delete: bool) {
    use crate::models::{FileConfig, FileConsumerMode};

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("drain.log");
    const N: usize = 25;
    let contents: String = (0..N).map(|i| format!("row-{i}\n")).collect();
    tokio::fs::write(&file_path, contents.as_bytes())
        .await
        .unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        mode: Some(FileConsumerMode::Consume { delete }),
        ..Default::default()
    }));
    let out_topic = format!("file_drain_out_{}", fast_uuid_v7::gen_id());
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output)
        .with_batch_size(10)
        .with_exit_on_empty(true);

    let mut verifier = route
        .connect_to_output("file_drain_verifier")
        .await
        .unwrap();
    let collector = tokio::spawn(async move {
        let mut received = Vec::new();
        while received.len() < N {
            let item = tokio::time::timeout(Duration::from_secs(5), verifier.receive())
                .await
                .expect("timed out draining output")
                .expect("output stream closed early");
            received.push(item.message.get_payload_str().to_string());
            (item.commit)(MessageDisposition::Ack).await.unwrap();
        }
        received
    });

    let handle = route.run("file_drain_test").await.unwrap();

    // The route future must complete on its own once the file is drained.
    tokio::time::timeout(Duration::from_secs(10), handle.join())
        .await
        .expect("file route did not exit on its own after draining")
        .expect("route task panicked");

    let received = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("timed out collecting output")
        .expect("collector task panicked");
    assert_eq!(received.len(), N);
}

#[tokio::test]
async fn test_route_exit_on_empty_file_tail() {
    run_file_exit_on_empty(false).await;
}

#[tokio::test]
async fn test_route_exit_on_empty_file_queue() {
    run_file_exit_on_empty(true).await;
}
