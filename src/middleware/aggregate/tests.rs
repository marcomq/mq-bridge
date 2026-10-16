use super::*;
use serde_json::json;

const SUM: &str = "{ sum: (state.sum ?? 0) + amount, n: (state.n ?? 0) + 1 }";

fn aggregate(yaml: &str) -> Aggregate {
    Aggregate::new(&serde_yaml_ng::from_str(yaml).unwrap()).unwrap()
}

fn error(yaml: &str) -> String {
    match Aggregate::new(&serde_yaml_ng::from_str(yaml).unwrap()) {
        Ok(_) => panic!("config must be rejected"),
        Err(e) => e.to_string(),
    }
}

fn msg(payload: Value) -> CanonicalMessage {
    CanonicalMessage::from_json(payload).unwrap()
}

/// Folds the payloads as one batch; `None` marks a failed message.
fn fold(aggregate: &Aggregate, payloads: Vec<Value>) -> Vec<Option<Value>> {
    aggregate
        .fold(payloads.into_iter().map(msg).collect())
        .into_iter()
        .map(|r| r.ok().map(|m| serde_json::from_slice(&m.payload).unwrap()))
        .collect()
}

fn sum_by_card() -> Aggregate {
    aggregate(&format!(
        "{{ consistency: single_writer, key: '${{payload:card}}', into: stats, \
         expression: '{SUM}' }}"
    ))
}

#[test]
fn state_is_kept_per_key_and_across_batches() {
    let agg = sum_by_card();
    let out = fold(
        &agg,
        vec![
            json!({"card": "a", "amount": 10}),
            json!({"card": "b", "amount": 1}),
            json!({"card": "a", "amount": 2.5}),
        ],
    );
    assert_eq!(
        out[0].as_ref().unwrap()["stats"],
        json!({"sum": 10, "n": 1})
    );
    assert_eq!(out[1].as_ref().unwrap()["stats"], json!({"sum": 1, "n": 1}));
    assert_eq!(
        out[2].as_ref().unwrap()["stats"],
        json!({"sum": 12.5, "n": 2})
    );

    let out = fold(&agg, vec![json!({"card": "a", "amount": 1})]);
    assert_eq!(
        out[0].as_ref().unwrap()["stats"],
        json!({"sum": 13.5, "n": 3})
    );
}

#[test]
fn previous_emits_the_state_before_the_message() {
    let agg = aggregate(&format!(
        "{{ key: '${{payload:card}}', into: stats, emit: previous, expression: '{SUM}' }}"
    ));
    let out = fold(
        &agg,
        vec![
            json!({"card": "a", "amount": 10}),
            json!({"card": "a", "amount": 5}),
        ],
    );
    assert_eq!(out[0].as_ref().unwrap()["stats"], Value::Null);
    assert_eq!(
        out[1].as_ref().unwrap()["stats"],
        json!({"sum": 10, "n": 1})
    );
}

/// A weighted sum and its weight give an average the first value does not dominate.
#[test]
fn output_shapes_what_the_message_carries() {
    let agg = aggregate(
        "key: '${payload:card}'
into: avg
expression: '{ s: (state.s ?? 0) * 0.99 + amount, w: (state.w ?? 0) * 0.99 + 1 }'
output: 'state.s / state.w'",
    );
    let mut payloads = vec![json!({"card": "a", "amount": 500})];
    payloads.extend((0..100).map(|_| json!({"card": "a", "amount": 50})));
    let out = fold(&agg, payloads);
    assert_eq!(out[0].as_ref().unwrap()["avg"], json!(500));
    let last = out[100].as_ref().unwrap()["avg"].as_f64().unwrap();
    assert!((last - 52.6).abs() < 0.1, "got {last}");
}

#[test]
fn entries_write_nested_paths_and_keep_other_fields_verbatim() {
    let agg = aggregate(
        "entries:
  - { key: '${payload:card}', into: features.card, expression: '(state ?? 0) + 1' }
  - { key: '${payload:user}', into: features.user, expression: '(state ?? 0) + amount' }",
    );
    let raw = br#"{"card":"a","user":7,"amount":10.50,"features":{"kept":true}}"#;
    let out = agg.fold(vec![CanonicalMessage::new(raw.to_vec(), None)]);
    let payload = String::from_utf8(out[0].as_ref().unwrap().payload.to_vec()).unwrap();
    assert!(payload.starts_with(r#"{"card":"a","user":7,"amount":10.50,"#));
    let doc: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(doc["features"]["kept"], json!(true));
    assert_eq!(doc["features"]["user"], json!(10.5));
}

#[test]
fn scalar_state_counts() {
    let agg = aggregate("{ key: '${payload:card}', into: seen, expression: '(state ?? 0) + 1' }");
    let out = fold(&agg, vec![json!({"card": "a"}), json!({"card": "a"})]);
    assert_eq!(out[1].as_ref().unwrap()["seen"], json!(2));
}

#[test]
fn a_failing_message_changes_no_state() {
    let agg = aggregate(&format!(
        "entries:
  - {{ key: '${{payload:card}}', into: a, expression: '{SUM}' }}
  - {{ key: '${{payload:user}}', into: b, expression: '{SUM}' }}"
    ));
    let out = fold(
        &agg,
        vec![
            json!({"card": "a", "user": "u", "amount": 1}),
            json!({"card": "a", "amount": 1}),
            json!({"card": "a", "user": "u", "amount": "x"}),
            json!({"card": "a", "user": "u", "amount": 1}),
        ],
    );
    assert!(out[1].is_none(), "missing key");
    assert!(out[2].is_none(), "expression error");
    assert_eq!(out[3].as_ref().unwrap()["a"], json!({"sum": 2, "n": 2}));

    let out = agg.fold(vec![CanonicalMessage::new(b"not json".to_vec(), None)]);
    assert!(matches!(out[0], Err((_, PublisherError::NonRetryable(_)))));
}

#[test]
fn expressions_read_metadata() {
    let agg =
        aggregate("{ key: '${metadata:tenant}', into: last, expression: '{ topic: meta.topic }' }");
    let message = msg(json!({"amount": 1})).with_metadata(HashMap::from([
        ("tenant".to_string(), "t1".to_string()),
        ("topic".to_string(), "orders".to_string()),
    ]));
    let out = agg.fold(vec![message]);
    let doc: Value = serde_json::from_slice(&out[0].as_ref().unwrap().payload).unwrap();
    assert_eq!(doc["last"], json!({"topic": "orders"}));
}

#[test]
fn escaped_payload_keys_take_the_full_parse() {
    let agg = sum_by_card();
    let raw = br#"{"card":"a","amount":1,"a\"b":2}"#;
    let out = agg.fold(vec![CanonicalMessage::new(raw.to_vec(), None)]);
    let doc: Value = serde_json::from_slice(&out[0].as_ref().unwrap().payload).unwrap();
    assert_eq!(doc["a\"b"], json!(2));
    assert_eq!(doc["stats"]["n"], json!(1));
}

#[test]
fn invalid_config_is_rejected() {
    assert!(error("{}").contains("list `entries`"));
    assert!(error("{ key: '${payload:a}', into: x }").contains("set together"));
    assert!(error("{ key: fixed, into: x, expression: '1' }").contains("must read the message"));
    assert!(error("{ key: '${payload:a}', into: x, expression: '1 +' }").contains("`expression`"));
    assert!(error("{ key: '${payload:a}', into: 'x..y', expression: '1' }").contains("`into`"));
    let entry = "{ key: '${payload:a}', into: x, fields: { n: count } }";
    assert!(error(&format!("entries: [{entry}, {entry}]")).contains("same `into`"));
}

/// `cargo test --release --features aggregate,yaml --lib aggregate_fold_bench -- --ignored --nocapture`
#[test]
#[ignore = "microbench, run explicitly"]
fn aggregate_fold_bench() {
    const DIMENSIONS: [&str; 5] = [
        "user_id",
        "card_id",
        "merchant_id",
        "terminal_id",
        "country",
    ];
    const EMAS: &str = "{ fast: (state.fast ?? amount) * 0.9 + amount * 0.1, \
        mid: (state.mid ?? amount) * 0.99 + amount * 0.01, \
        slow: (state.slow ?? amount) * 0.999 + amount * 0.001, n: (state.n ?? 0) + 1 }";
    let count = 200_000;
    let keys = 10_000;
    let cardinality = [keys, keys, keys / 10, keys / 5, 15];
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let payloads: Vec<Vec<u8>> = (0..count)
        .map(|_| {
            let mut doc = Map::new();
            for (name, c) in DIMENSIONS.iter().zip(cardinality) {
                doc.insert(name.to_string(), json!(next() % c));
            }
            doc.insert("amount".into(), json!((next() % 100_000) as f64 / 100.0));
            serde_json::to_vec(&doc).unwrap()
        })
        .collect();
    let config = |dimensions: &[&str]| {
        let entries: Vec<String> = dimensions
            .iter()
            .map(|d| {
                format!(
                    "  - {{ key: '${{payload:{d}}}', into: features.{d}, expression: '{EMAS}' }}"
                )
            })
            .collect();
        format!("entries:\n{}", entries.join("\n"))
    };
    let fields = |dimensions: &[&str]| {
        let entries: Vec<String> = dimensions
            .iter()
            .map(|d| {
                format!(
                    "  - {{ key: '${{payload:{d}}}', into: {d}_features, fields: {{ fast: \
                     'ema(amount, 0.1)', mid: 'ema(amount, 0.01)', slow: 'ema(amount, 0.001)', \
                     n: count }} }}"
                )
            })
            .collect();
        format!("entries:\n{}", entries.join("\n"))
    };
    for (name, config) in [
        ("1 key x 3 EMAs", config(&DIMENSIONS[1..2])),
        ("5 keys x 3 EMAs", config(&DIMENSIONS[..])),
        ("fields: 1 key x 3 EMAs", fields(&DIMENSIONS[1..2])),
        ("fields: 5 keys x 3 EMAs", fields(&DIMENSIONS[..])),
    ] {
        let mut nanos: Vec<u128> = (0..3)
            .map(|_| {
                let agg = aggregate(&config);
                let batches: Vec<Vec<CanonicalMessage>> = payloads
                    .chunks(1024)
                    .map(|c| {
                        c.iter()
                            .map(|p| CanonicalMessage::new(p.clone(), None))
                            .collect()
                    })
                    .collect();
                let start = std::time::Instant::now();
                for batch in batches {
                    let out = agg.fold(batch);
                    assert!(out.iter().all(Result::is_ok));
                    std::hint::black_box(out);
                }
                start.elapsed().as_nanos()
            })
            .collect();
        nanos.sort_unstable();
        let per_msg = nanos[1] as f64 / count as f64;
        println!(
            "{name:<26} {:>10.0} msg/s {per_msg:>8.0} ns/msg",
            1e9 / per_msg
        );
    }
}

/// Hands out one batch, then nothing, and records the dispositions it is committed with.
struct OneBatch {
    batch: Option<Vec<CanonicalMessage>>,
    committed: std::sync::Arc<Mutex<Vec<MessageDisposition>>>,
}

#[async_trait]
impl MessageConsumer for OneBatch {
    async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
        let messages = self.batch.take().unwrap_or_default();
        let committed = self.committed.clone();
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                committed.lock().unwrap().extend(dispositions);
                Ok(())
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[tokio::test]
async fn consumer_drops_and_acks_a_message_it_cannot_fold() {
    let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let inner = OneBatch {
        batch: Some(vec![
            msg(json!({"card": "a", "amount": 1})),
            msg(json!({"amount": 1})),
            msg(json!({"card": "a", "amount": 2})),
        ]),
        committed: committed.clone(),
    };
    let config = serde_yaml_ng::from_str(&format!(
        "{{ consistency: single_writer, key: '${{payload:card}}', into: stats, \
         expression: '{SUM}' }}"
    ))
    .unwrap();
    let mut consumer = AggregateConsumer::new(Box::new(inner), &config, "test")
        .await
        .unwrap();

    let ReceivedBatch { messages, commit } = consumer.receive_batch(10).await.unwrap();
    assert_eq!(messages.len(), 2);
    let last: Value = serde_json::from_slice(&messages[1].payload).unwrap();
    assert_eq!(last["stats"], json!({"sum": 3, "n": 2}));

    commit(vec![MessageDisposition::Ack, MessageDisposition::Nack])
        .await
        .unwrap();
    assert!(matches!(
        committed.lock().unwrap().as_slice(),
        [
            MessageDisposition::Ack,
            MessageDisposition::Ack,
            MessageDisposition::Nack
        ]
    ));
}

#[tokio::test]
async fn publisher_fails_only_the_message_it_cannot_fold() {
    let inner = Box::new(crate::endpoints::structural::null::NullPublisher);
    let config = serde_yaml_ng::from_str(&format!(
        "{{ consistency: single_writer, key: '${{payload:card}}', into: stats, \
         expression: '{SUM}' }}"
    ))
    .unwrap();
    let publisher = AggregatePublisher::new(inner, &config, "test")
        .await
        .unwrap();
    let sent = publisher
        .send_batch(vec![
            msg(json!({"card": "a", "amount": 1})),
            msg(json!({"amount": 1})),
        ])
        .await
        .unwrap();
    let SentBatch::Partial { failed, .. } = sent else {
        panic!("one message must fail");
    };
    assert_eq!(failed.len(), 1);
    assert!(matches!(failed[0].1, PublisherError::NonRetryable(_)));
}

/// The examples of docs/REFERENCE.md, run rather than only parsed.
#[test]
fn documented_examples_behave_as_described() {
    let agg = aggregate(
        "key: '${payload:card_id}'
into: card
expression: '{ total: (state.total ?? 0) + amount, count: (state.count ?? 0) + 1 }'",
    );
    let out = agg.fold(vec![CanonicalMessage::new(
        br#"{"card_id": 7, "amount": 20}"#.to_vec(),
        None,
    )]);
    let doc: Value = serde_json::from_slice(&out[0].as_ref().unwrap().payload).unwrap();
    assert_eq!(
        doc,
        json!({"card_id": 7, "amount": 20, "card": {"total": 20, "count": 1}})
    );

    let agg = aggregate(
        "entries:
  - key: '${payload:merchant_id}'
    into: features.merchant
    emit: previous
    expression: '{ max: max([state.max ?? amount, amount]) }'",
    );
    let out = fold(
        &agg,
        vec![
            json!({"merchant_id": 1, "amount": 5}),
            json!({"merchant_id": 1, "amount": 3}),
            json!({"merchant_id": 1, "amount": 9}),
        ],
    );
    assert_eq!(
        out[0].as_ref().unwrap()["features"]["merchant"],
        Value::Null
    );
    assert_eq!(
        out[2].as_ref().unwrap()["features"]["merchant"],
        json!({"max": 5})
    );
}

#[test]
fn documented_fields_example_behaves_as_described() {
    let agg = aggregate(
        "key: '${payload:card_id}'
into: card
fields:
  n: count
  total: sum(amount)
  high: max(amount)
  avg: ema(amount, 0.01)",
    );
    let out = agg.fold(vec![CanonicalMessage::new(
        br#"{"card_id": 7, "amount": 20}"#.to_vec(),
        None,
    )]);
    assert_eq!(
        out[0].as_ref().unwrap().payload.as_ref(),
        br#"{"card_id": 7, "amount": 20,"card":{"avg":20.0,"high":20.0,"n":1,"total":20.0}}"#
    );
}

fn shared(store: &std::sync::Arc<dyn StateStore>) -> Aggregate {
    let mut aggregate = sum_by_card();
    aggregate.store = Some(store.clone());
    aggregate
}

/// Folds `batches` of `per_batch` messages over three cards and returns the output payloads.
async fn fold_cards(aggregate: &Aggregate, batches: usize, per_batch: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for batch in 0..batches {
        let messages = (0..per_batch)
            .map(|i| msg(json!({"card": format!("c{}", (batch + i) % 3), "amount": 1})))
            .collect();
        for folded in aggregate.fold_batch(messages).await.unwrap().0 {
            out.push(serde_json::from_slice(&folded.ok().unwrap().payload).unwrap());
        }
    }
    out
}

fn stored_n(rows: &HashMap<String, (String, i64)>, card: &str) -> i64 {
    let state: Value = serde_json::from_str(&rows[&format!("stats:{card}")].0).unwrap();
    state["n"].as_i64().unwrap()
}

#[tokio::test]
async fn two_instances_on_one_store_lose_no_update() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    let store: std::sync::Arc<dyn StateStore> = memory.clone();
    let (a, b) = (shared(&store), shared(&store));
    // The store yields inside every call, so the two instances interleave and conflict.
    let (out_a, out_b) = tokio::join!(fold_cards(&a, 20, 9), fold_cards(&b, 20, 9));

    let rows = memory.rows.lock().unwrap();
    for card in ["c0", "c1", "c2"] {
        assert_eq!(stored_n(&rows, card), 120, "every message counted once");
    }
    // Each count is handed out exactly once across both instances.
    let mut counts: Vec<i64> = out_a
        .iter()
        .chain(&out_b)
        .filter(|p| p["card"] == "c0")
        .map(|p| p["stats"]["n"].as_i64().unwrap())
        .collect();
    counts.sort_unstable();
    assert_eq!(counts, (1..=120).collect::<Vec<i64>>());
}

#[tokio::test]
async fn a_shared_store_keeps_no_state_of_a_message_whose_output_has_no_place() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    let mut agg = aggregate(&format!(
        "{{ on_error: fail, consistency: single_writer, key: '${{payload:card}}', \
         into: features.stats, expression: '{SUM}' }}"
    ));
    agg.store = Some(memory.clone());
    // `features` is a number in the first message, so `features.stats` cannot be set.
    let batch = vec![
        msg(json!({"card": "a", "amount": 1, "features": 5})),
        msg(json!({"card": "a", "amount": 1})),
    ];
    let (out, _) = agg.fold_batch(batch).await.unwrap();
    assert!(out[0].is_err());
    let second: Value = serde_json::from_slice(&out[1].as_ref().ok().unwrap().payload).unwrap();
    assert_eq!(second["features"]["stats"]["n"], json!(1));
    let rows = memory.rows.lock().unwrap();
    let state: Value = serde_json::from_str(&rows["features.stats:a"].0).unwrap();
    assert_eq!(state["n"], json!(1), "a redelivery would count it again");
}

#[tokio::test]
async fn a_store_failure_folds_nothing_and_nacks_the_batch() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    memory
        .failures
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let inner = OneBatch {
        batch: Some(vec![msg(json!({"card": "a", "amount": 1}))]),
        committed: committed.clone(),
    };
    let store: std::sync::Arc<dyn StateStore> = memory.clone();
    let mut consumer = AggregateConsumer {
        inner: Box::new(inner),
        aggregate: shared(&store),
    };
    assert!(consumer.receive_batch(10).await.is_err());
    assert!(matches!(
        committed.lock().unwrap().as_slice(),
        [MessageDisposition::Nack]
    ));
    assert!(memory.rows.lock().unwrap().is_empty());

    // The redelivered batch starts from the stored state, which the failure left alone.
    let out = fold_cards(&consumer.aggregate, 1, 3).await;
    assert_eq!(out[0]["stats"], json!({"sum": 1, "n": 1}));
}

#[test]
fn a_stored_state_keeps_every_digit() {
    let json =
        r#"{"s":0.1234567890123456789012345678,"n":3,"tags":["a\"b",true,null],"o":{"x":-1.50}}"#;
    let mut out = Vec::new();
    Stored::parse(json).unwrap().write_json(&mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), json);
    assert!(Stored::parse("{\"s\":1e400}").is_err());
}

/// Two instances on one store count every message once, and a third starts from their state.
#[cfg(any(feature = "sqlx", feature = "mongodb"))]
async fn check_shared_store(url: &str) {
    let config: AggregateMiddleware = serde_yaml_ng::from_str(&format!(
        "{{ store: '{url}', key: '${{payload:card}}', into: stats, expression: '{SUM}' }}"
    ))
    .unwrap();
    let a = Aggregate::connect(&config, "route").await.unwrap();
    let b = Aggregate::connect(&config, "route").await.unwrap();
    let (out_a, out_b) = tokio::join!(fold_cards(&a, 10, 9), fold_cards(&b, 10, 9));
    assert_eq!(out_a.len() + out_b.len(), 180);

    let restarted = Aggregate::connect(&config, "route").await.unwrap();
    let out = fold_cards(&restarted, 1, 1).await;
    assert_eq!(out[0]["stats"], json!({"sum": 61, "n": 61}));
}

#[cfg(feature = "sqlx")]
#[tokio::test]
async fn a_sqlite_store_is_shared_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agg.db");
    std::fs::File::create(&path).unwrap();
    // Windows needs the three-slash form and forward slashes.
    #[cfg(windows)]
    let url = format!("sqlite:///{}", path.to_string_lossy().replace('\\', "/"));
    #[cfg(not(windows))]
    let url = format!("sqlite://{}", path.display());
    check_shared_store(&url).await;
}

// Live: MQB_AGG_STORE_URL=postgres://postgres:pw@localhost:55432/t/agg_test (or a
// mongodb://localhost:57017/t/agg_test URL) cargo test --features aggregate,sqlx,mongodb,yaml \
//   --lib live_store -- --ignored --nocapture. The table or collection must be empty.
#[cfg(any(feature = "sqlx", feature = "mongodb"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a database"]
async fn a_live_store_is_shared_and_survives_a_restart() {
    let Ok(url) = std::env::var("MQB_AGG_STORE_URL") else {
        eprintln!("MQB_AGG_STORE_URL not set; skipping");
        return;
    };
    check_shared_store(&url).await;
}

/// Throughput through a store: five keys with three moving averages each, like the fold bench.
#[cfg(any(feature = "sqlx", feature = "mongodb"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "bench, needs a database"]
async fn a_live_store_bench() {
    let Ok(url) = std::env::var("MQB_AGG_STORE_URL") else {
        eprintln!("MQB_AGG_STORE_URL not set; skipping");
        return;
    };
    let batch: usize = std::env::var("MQB_AGG_BATCH").map_or(1024, |b| b.parse().unwrap());
    let ema = "{ f: (state.f ?? 0) * 0.9 + amount, m: (state.m ?? 0) * 0.99 + amount, \
               s: (state.s ?? 0) * 0.999 + amount }";
    let dims = ["user", "card", "merchant", "terminal", "country"];
    let entries: Vec<String> = dims
        .iter()
        .map(|d| {
            format!("{{ key: '${{payload:{d}}}', into: 'features.{d}', expression: '{ema}' }}")
        })
        .collect();
    // MQB_AGG_MODE=single_writer and MQB_AGG_FIELDS=1 select the other variants.
    let mode = std::env::var("MQB_AGG_MODE").unwrap_or_else(|_| "shared".into());
    let plain: Vec<String> = dims
        .iter()
        .map(|d| {
            format!(
                "{{ key: '${{payload:{d}}}', into: '{d}_features', fields: {{ f: 'ema(amount, \
                 0.1)', m: 'ema(amount, 0.01)', s: 'ema(amount, 0.001)' }} }}"
            )
        })
        .collect();
    let entries = match std::env::var("MQB_AGG_FIELDS").is_ok() {
        true => plain,
        false => entries,
    };
    for (name, entries) in [("1 key", &entries[..1]), ("5 keys", &entries[..])] {
        let config: AggregateMiddleware = serde_yaml_ng::from_str(&format!(
            "{{ store: '{url}', consistency: {mode}, entries: [{}] }}",
            entries.join(", ")
        ))
        .unwrap();
        let aggregate = Aggregate::connect(&config, "bench").await.unwrap();
        let total = 40 * batch;
        let started = std::time::Instant::now();
        let mut last = None;
        for b in 0..total / batch {
            let messages = (0..batch)
                .map(|i| {
                    let n = (b * batch + i) * 7919 % 10_000;
                    msg(
                        json!({"user": n, "card": n + 1, "merchant": n % 500, "terminal": n % 2000,
                        "country": n % 50, "amount": 12.5}),
                    )
                })
                .collect();
            let (folded, flush) = aggregate.fold_batch(messages).await.unwrap();
            assert!(folded.iter().all(Result::is_ok));
            last = Some(flush);
        }
        // Everything is stored once the last batch is.
        last.unwrap().wait().await.unwrap();
        let rate = total as f64 / started.elapsed().as_secs_f64();
        println!("{name}, batch {batch}: {rate:.0} msg/s");
    }
}

const FIELDS: &str = "{ n: count, total: sum(amount), low: min(amount), high: max(amount), \
    avg: mean(amount), recent: last(amount), ema: 'ema(amount, 0.5)' }";

fn fields_by_card(into: &str) -> Aggregate {
    aggregate(&format!(
        "{{ key: '${{payload:card}}', into: {into}, fields: {FIELDS} }}"
    ))
}

#[test]
fn fields_compute_the_built_in_aggregates() {
    for into in ["stats", "features.card"] {
        let agg = fields_by_card(into);
        let out = fold(
            &agg,
            vec![
                json!({"card": "a", "amount": 4}),
                json!({"card": "b", "amount": 100}),
                json!({"card": "a", "amount": "1.0"}),
            ],
        );
        let at = |doc: &Value| into.split('.').fold(doc.clone(), |d, k| d[k].clone());
        // The first value is the mean of what was seen, not a biased start.
        assert_eq!(
            at(out[0].as_ref().unwrap()),
            json!({"avg": 4.0, "ema": 4.0, "high": 4.0, "low": 4.0, "n": 1, "recent": 4.0, "total": 4.0})
        );
        // ema: (4 * 0.5 + 1) / (0.5 + 1) = 2
        assert_eq!(
            at(out[2].as_ref().unwrap()),
            json!({"avg": 2.5, "ema": 2.0, "high": 4.0, "low": 1.0, "n": 2, "recent": 1.0, "total": 5.0})
        );
        assert_eq!(out[2].as_ref().unwrap()["amount"], json!("1.0"));
    }
}

#[test]
fn fields_compute_variance_and_standard_deviation() {
    let agg = aggregate(
        "{ key: '${payload:k}', into: s, fields: { sd: stddev(x), var: variance(x), \
         esd: 'ema_stddev(x, 0.5)', evar: 'ema_variance(x, 0.5)', flat: 'ema_variance(x, 1)' } }",
    );
    let values = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
    let out = fold(
        &agg,
        values.iter().map(|x| json!({"k": "a", "x": x})).collect(),
    );
    let at = |i: usize, name: &str| out[i].as_ref().unwrap()["s"][name].clone();
    let near = |value: Value, expected: f64| (value.as_f64().unwrap() - expected).abs() < 1e-9;
    // One value has no spread.
    for name in ["sd", "var", "esd", "evar"] {
        assert_eq!(at(0, name), Value::Null);
    }
    // Sample variance of all eight: 32 / 7.
    assert!(near(at(7, "var"), 32.0 / 7.0));
    assert!(near(at(7, "sd"), (32.0f64 / 7.0).sqrt()));
    // Weights 0.5 and 1 on 2 and 4: mean 10/3, squares 4/3, freedom 2/3.
    assert!(near(at(1, "evar"), 2.0));
    assert!(near(at(1, "esd"), 2.0f64.sqrt()));
    // The recent values weigh more than the first.
    assert!(at(7, "evar").as_f64().unwrap() > at(3, "evar").as_f64().unwrap());
    // alpha 1 keeps the last value only.
    assert_eq!(at(7, "flat"), Value::Null);
}

#[test]
fn a_key_combines_several_fields() {
    let agg = aggregate("{ key: '${payload:k}:${payload:hour}', into: s, fields: { n: count } }");
    let out = fold(
        &agg,
        vec![
            json!({"k": "a", "hour": 1}),
            json!({"k": "a", "hour": 1}),
            json!({"k": "a", "hour": 2}),
        ],
    );
    assert_eq!(out[1].as_ref().unwrap()["s"]["n"], json!(2));
    assert_eq!(out[2].as_ref().unwrap()["s"]["n"], json!(1));
}

#[test]
fn fields_fail_a_message_without_a_number_and_keep_the_state() {
    let agg = fields_by_card("stats");
    let out = fold(
        &agg,
        vec![
            json!({"card": "a", "amount": 4}),
            json!({"card": "a", "amount": "n/a"}),
            json!({"card": "a"}),
            json!({"amount": 1}),
            json!({"card": "a", "amount": 6}),
        ],
    );
    assert!(out[1].is_none() && out[2].is_none() && out[3].is_none());
    assert_eq!(out[4].as_ref().unwrap()["stats"]["n"], json!(2));
    assert_eq!(out[4].as_ref().unwrap()["stats"]["total"], json!(10.0));
}

#[test]
fn fields_append_to_the_payload_and_replace_an_existing_field() {
    let agg =
        aggregate("{ key: '${payload:card}', into: stats, emit: previous, fields: { n: count } }");
    let raw = br#"{ "card" : "a", "amount":1.50 } "#;
    let out = agg.fold(vec![
        CanonicalMessage::new(raw.to_vec(), None),
        CanonicalMessage::new(raw.to_vec(), None),
        CanonicalMessage::new(br#"{"card":"a","stats":"old","x\"y":1}"#.to_vec(), None),
        CanonicalMessage::new(br#"{"card":"a","stats":"old"}"#.to_vec(), None),
    ]);
    let text = |i: usize| String::from_utf8(out[i].as_ref().unwrap().payload.to_vec()).unwrap();
    assert_eq!(text(0), r#"{ "card" : "a", "amount":1.50 ,"stats":null}"#);
    assert_eq!(
        text(1),
        r#"{ "card" : "a", "amount":1.50 ,"stats":{"n":1}}"#
    );
    let doc: Value = serde_json::from_str(&text(2)).unwrap();
    assert_eq!(doc, json!({"card": "a", "stats": {"n": 2}, "x\"y": 1}));
    let doc: Value = serde_json::from_str(&text(3)).unwrap();
    assert_eq!(doc, json!({"card": "a", "stats": {"n": 3}}));
}

#[test]
fn fields_and_expressions_mix_and_read_nested_values() {
    let agg = aggregate(&format!(
        "entries:
  - {{ key: '${{payload:card}}', into: stats, expression: '{SUM}' }}
  - {{ key: '${{payload:card}}', into: fast, fields: {{ total: sum(order.amount) }} }}"
    ));
    let out = fold(
        &agg,
        vec![
            json!({"card": 7, "amount": 1, "order": {"amount": 2.5}}),
            json!({"card": 7, "amount": 1, "order": {"amount": 2.5}}),
        ],
    );
    let doc = out[1].as_ref().unwrap();
    assert_eq!(doc["stats"], json!({"sum": 2, "n": 2}));
    assert_eq!(doc["fast"], json!({"total": 5.0}));
}

#[test]
fn invalid_fields_are_rejected() {
    let with = |fields: &str| {
        error(&format!(
            "{{ key: '${{payload:a}}', into: x, fields: {fields} }}"
        ))
    };
    assert!(with("{ a: median(x) }").contains("invalid field"));
    assert!(with("{ a: sum }").contains("invalid field"));
    assert!(with("{ a: 'sum(x, 1)' }").contains("invalid field"));
    assert!(with("{ a: 'ema(x)' }").contains("invalid field"));
    assert!(with("{ a: 'ema(x, 1.5)' }").contains("alpha"));
    assert!(with("{ a: 'stddev(x, 0.5)' }").contains("invalid field"));
    assert!(with("{ a: ema_stddev(x) }").contains("invalid field"));
    assert!(with("{ a: 'ema_variance(x, 0)' }").contains("alpha"));
    assert!(with("{}").contains("must not be empty"));
    assert!(
        error("{ key: '${payload:a}', into: x, expression: '1', fields: { n: count } }")
            .contains("either")
    );
}

#[tokio::test]
async fn fields_states_are_shared_through_a_store() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    let store: std::sync::Arc<dyn StateStore> = memory.clone();
    let instance = || {
        let mut aggregate = fields_by_card("stats");
        aggregate.store = Some(store.clone());
        aggregate
    };
    let (a, b) = (instance(), instance());
    let (out_a, out_b) = tokio::join!(fold_cards(&a, 20, 9), fold_cards(&b, 20, 9));
    let highest = (out_a.iter().chain(&out_b))
        .filter(|p| p["card"] == "c0")
        .map(|p| p["stats"]["n"].as_i64().unwrap())
        .max();
    assert_eq!(highest, Some(120));
}

fn write_behind(
    store: &std::sync::Arc<store::MemoryStateStore>,
    mut aggregate: Aggregate,
) -> Aggregate {
    aggregate.write_behind(store.clone());
    aggregate
}

#[tokio::test]
async fn single_writer_stores_behind_and_a_restart_continues() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    for aggregate in [sum_by_card(), fields_by_card("stats")] {
        memory.rows.lock().unwrap().clear();
        let plain = aggregate.plain;
        let first = write_behind(&memory, aggregate);
        let mut last = None;
        for _ in 0..5 {
            let messages = (0..6)
                .map(|i| msg(json!({"card": i % 2, "amount": 1})))
                .collect();
            let (folded, flush) = first.fold_batch(messages).await.unwrap();
            assert!(folded.iter().all(Result::is_ok));
            last = Some(flush);
        }
        last.unwrap().wait().await.unwrap();
        drop(first);

        let restarted = match plain {
            true => fields_by_card("stats"),
            false => sum_by_card(),
        };
        let restarted = write_behind(&memory, restarted);
        let (folded, flush) = restarted
            .fold_batch(vec![msg(json!({"card": 0, "amount": 1}))])
            .await
            .unwrap();
        let doc: Value = serde_json::from_slice(&folded[0].as_ref().ok().unwrap().payload).unwrap();
        assert_eq!(doc["stats"]["n"], json!(16));
        flush.wait().await.unwrap();
    }
}

#[tokio::test]
async fn single_writer_acks_after_the_states_are_stored() {
    use std::sync::atomic::Ordering;
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    memory.paused.store(true, Ordering::Relaxed);
    let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let inner = OneBatch {
        batch: Some(vec![msg(json!({"card": "a", "amount": 1}))]),
        committed: committed.clone(),
    };
    let mut consumer = AggregateConsumer {
        inner: Box::new(inner),
        aggregate: write_behind(&memory, sum_by_card()),
    };
    let ReceivedBatch { messages, commit } = consumer.receive_batch(10).await.unwrap();
    assert_eq!(messages.len(), 1);
    let commit = tokio::spawn(commit(vec![MessageDisposition::Ack]));
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        committed.lock().unwrap().is_empty(),
        "acked before the flush"
    );

    memory.paused.store(false, Ordering::Relaxed);
    commit.await.unwrap().unwrap();
    assert!(matches!(
        committed.lock().unwrap().as_slice(),
        [MessageDisposition::Ack]
    ));
    assert_eq!(stored_n(&memory.rows.lock().unwrap(), "a"), 1);
}

#[tokio::test]
async fn single_writer_nacks_a_batch_whose_flush_failed_and_recounts_it() {
    use std::sync::atomic::Ordering;
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    let aggregate = write_behind(&memory, sum_by_card());
    let batch = || vec![msg(json!({"card": "a", "amount": 1}))];
    let (_, flush) = aggregate.fold_batch(batch()).await.unwrap();
    flush.wait().await.unwrap();

    memory.failures.store(1, Ordering::Relaxed);
    let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let inner = OneBatch {
        batch: Some(batch()),
        committed: committed.clone(),
    };
    let mut consumer = AggregateConsumer {
        inner: Box::new(inner),
        aggregate,
    };
    let ReceivedBatch { commit, .. } = consumer.receive_batch(10).await.unwrap();
    assert!(commit(vec![MessageDisposition::Ack]).await.is_err());
    assert!(matches!(
        committed.lock().unwrap().as_slice(),
        [MessageDisposition::Nack]
    ));

    // The redelivered message is counted once: memory was dropped, the store holds n = 1.
    let (folded, flush) = consumer.aggregate.fold_batch(batch()).await.unwrap();
    let doc: Value = serde_json::from_slice(&folded[0].as_ref().ok().unwrap().payload).unwrap();
    assert_eq!(doc["stats"]["n"], json!(2));
    flush.wait().await.unwrap();
    assert_eq!(stored_n(&memory.rows.lock().unwrap(), "a"), 2);
}

/// Hands out prepared batches, then empty ones.
#[cfg(feature = "dedup")]
struct Batches(std::vec::IntoIter<Vec<CanonicalMessage>>);

#[cfg(feature = "dedup")]
#[async_trait]
impl MessageConsumer for Batches {
    async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
        let messages = self.0.next().unwrap_or_default();
        let commit: BatchCommitFunc = Box::new(|_| Box::pin(async { Ok(()) }));
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `cargo test --release --features aggregate,dedup,yaml --lib aggregate_chain_bench -- --ignored --nocapture`
#[cfg(feature = "dedup")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "bench, run explicitly"]
async fn aggregate_chain_bench() {
    const DIMENSIONS: [&str; 5] = [
        "user_id",
        "card_id",
        "merchant_id",
        "terminal_id",
        "country",
    ];
    let count = 200_000;
    let batch: usize = std::env::var("MQB_AGG_BATCH").map_or(1024, |b| b.parse().unwrap());
    let keys = 10_000;
    let cardinality = [keys, keys, keys / 10, keys / 5, 15];
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let payloads: Vec<Vec<u8>> = (0..count)
        .map(|i| {
            let mut doc = Map::new();
            doc.insert("tx_id".into(), json!(format!("tx-{i}")));
            for (name, c) in DIMENSIONS.iter().zip(cardinality) {
                doc.insert(name.to_string(), json!(next() % c));
            }
            doc.insert("amount".into(), json!((next() % 100_000) as f64 / 100.0));
            serde_json::to_vec(&doc).unwrap()
        })
        .collect();

    let entries: Vec<String> = DIMENSIONS
        .iter()
        .map(|d| {
            format!(
                "{{ key: '${{payload:{d}}}', into: {d}_features, fields: {{ fast: \
                 'ema(amount, 0.1)', mid: 'ema(amount, 0.01)', slow: 'ema(amount, 0.001)', \
                 n: count }} }}"
            )
        })
        .collect();
    let aggregate = format!(
        "- aggregate: {{ consistency: single_writer, entries: [{}] }}",
        entries.join(", ")
    );
    let dedup = |key: &str| {
        format!("- deduplication: {{ store: 'memory://chain#', ttl_seconds: 3600{key} }}")
    };
    let dedup_id = dedup("");
    let dedup_payload = dedup(", key: '${payload:tx_id}'");
    let all = [
        "tx_id",
        "user_id",
        "card_id",
        "merchant_id",
        "terminal_id",
        "country",
        "amount",
    ];
    let schema = "- transform: { schema: { type: object, properties: { amount: { type: number }, \
                  country: { type: string } } } }"
        .to_string();
    let mapping = format!(
        "- transform: {{ mapping: {{ {} }} }}",
        all.map(|f| format!("{f}: '$.{f}'")).join(", ")
    );
    let expression = format!(
        "- transform: {{ expression: '{{ {}, amount_eur: amount * 0.9 }}' }}",
        all.map(|f| format!("{f}: {f}")).join(", ")
    );

    let lookup =
        "- lookup: { from: { static: { body: '{\"tier\":\"gold\",\"user\":${payload:user_id}}', \
                  raw: true } }, into: profile }"
            .to_string();
    // On an input the last entry sees the message first.
    let variants: Vec<(&str, Vec<&String>)> = vec![
        ("no middleware", vec![]),
        ("dedup (message_id)", vec![&dedup_id]),
        ("dedup (payload key)", vec![&dedup_payload]),
        ("transform schema", vec![&schema]),
        ("transform mapping", vec![&mapping]),
        ("transform expression", vec![&expression]),
        ("lookup (static)", vec![&lookup]),
        ("aggregate", vec![&aggregate]),
        ("dedup(id) > aggregate", vec![&aggregate, &dedup_id]),
        (
            "dedup(payload) > aggregate",
            vec![&aggregate, &dedup_payload],
        ),
        (
            "aggregate > dedup(payload)",
            vec![&dedup_payload, &aggregate],
        ),
        ("schema > aggregate", vec![&aggregate, &schema]),
        ("aggregate > schema", vec![&schema, &aggregate]),
        ("mapping > aggregate", vec![&aggregate, &mapping]),
        ("lookup > aggregate", vec![&aggregate, &lookup]),
        (
            "dedup(payload) > schema > aggregate",
            vec![&aggregate, &schema, &dedup_payload],
        ),
        (
            "dedup(payload) > mapping > aggregate",
            vec![&aggregate, &mapping, &dedup_payload],
        ),
        (
            "dedup > schema > lookup > aggregate",
            vec![&aggregate, &lookup, &schema, &dedup_payload],
        ),
    ];
    let mut run = 0;
    for (name, chain) in variants {
        let mut nanos = Vec::new();
        for _ in 0..3 {
            run += 1;
            let yaml: Vec<String> = chain
                .iter()
                .map(|m| m.replace('#', &run.to_string()))
                .collect();
            let middlewares = match yaml.is_empty() {
                true => Vec::new(),
                false => {
                    let list: Value = serde_yaml_ng::from_str(&yaml.join("\n")).unwrap();
                    serde_json::from_value(list).unwrap()
                }
            };
            let endpoint = crate::models::Endpoint {
                middlewares,
                ..Default::default()
            };
            let batches: Vec<Vec<CanonicalMessage>> = payloads
                .chunks(batch)
                .map(|c| {
                    c.iter()
                        .map(|p| CanonicalMessage::new(p.clone(), None))
                        .collect()
                })
                .collect();
            let mut consumer = crate::middleware::apply_middlewares_to_consumer(
                Box::new(Batches(batches.into_iter())),
                &endpoint,
                "chain_bench",
            )
            .await
            .unwrap();
            let start = std::time::Instant::now();
            let mut received = 0;
            loop {
                let ReceivedBatch { messages, commit } =
                    consumer.receive_batch(batch).await.unwrap();
                if messages.is_empty() {
                    break;
                }
                received += messages.len();
                commit(vec![MessageDisposition::Ack; messages.len()])
                    .await
                    .unwrap();
                std::hint::black_box(messages);
            }
            assert_eq!(received, count);
            nanos.push(start.elapsed().as_nanos());
        }
        nanos.sort_unstable();
        let per_msg = nanos[1] as f64 / count as f64;
        println!(
            "{name:<38} {:>10.0} msg/s {per_msg:>8.0} ns/msg",
            1e9 / per_msg
        );
    }
}

const COUNT_BY_K: &str = "key: '${payload:k}', into: s, fields: { n: count }";

#[test]
fn max_keys_forgets_the_least_recently_used_states() {
    let agg = aggregate(&format!("{{ {COUNT_BY_K}, max_keys: 4 }}"));
    let one = |k: &str| fold(&agg, vec![json!({"k": k})])[0].as_ref().unwrap()["s"]["n"].clone();
    assert_eq!(one("hot"), json!(1));
    for i in 0..20 {
        one(&format!("cold{i}"));
        // A key in use survives any number of others.
        assert_eq!(one("hot"), json!(i + 2));
    }
    assert_eq!(one("cold0"), json!(1), "an unused state is forgotten");
    let states = agg.states.lock().unwrap();
    assert!(states[0].len() <= 4 + 1);
}

#[test]
fn max_keys_of_zero_keeps_every_state() {
    let agg = aggregate(&format!("{{ {COUNT_BY_K}, max_keys: 0 }}"));
    for i in 0..50 {
        fold(&agg, vec![json!({"k": i})]);
    }
    assert_eq!(agg.states.lock().unwrap()[0].len(), 50);
}

#[tokio::test]
async fn single_writer_reloads_a_state_it_dropped_from_memory() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    for config in [
        format!("{{ {COUNT_BY_K}, max_keys: 4 }}"),
        "{ key: '${payload:k}', into: s, max_keys: 4, expression: '{ n: (state.n ?? 0) + 1 }' }"
            .to_string(),
    ] {
        memory.rows.lock().unwrap().clear();
        let agg = write_behind(&memory, aggregate(&config));
        let one = |k: String| {
            let agg = &agg;
            async move {
                let (folded, flush) = agg.fold_batch(vec![msg(json!({"k": k}))]).await.unwrap();
                flush.wait().await.unwrap();
                // Lets the writer trim after the flush.
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                let doc: Value =
                    serde_json::from_slice(&folded[0].as_ref().ok().unwrap().payload).unwrap();
                doc["s"]["n"].clone()
            }
        };
        assert_eq!(one("first".into()).await, json!(1));
        for i in 0..20 {
            one(format!("cold{i}")).await;
        }
        let held = agg.writer.as_ref().unwrap().lock().states[0].len();
        assert!(held <= 4 + 1, "{held} states in memory");
        // Dropped from memory, not from the store: the count continues.
        assert_eq!(one("first".into()).await, json!(2));
        assert_eq!(one("cold3".into()).await, json!(2));
    }
}

const TWO_ENTRIES: &str = "entries:
  - { key: '${payload:card}', into: card_stats, fields: { n: count, total: sum(amount) } }
  - { key: '${payload:shop}', into: shop_stats, fields: { n: count, fee: sum(fee) } }";

fn fold_messages(aggregate: &Aggregate, payloads: Vec<Value>) -> Vec<Folded> {
    aggregate.fold(payloads.into_iter().map(msg).collect())
}

#[test]
fn a_missing_field_names_itself_and_fails_the_message() {
    let agg = aggregate(TWO_ENTRIES);
    let out = fold_messages(&agg, vec![json!({"card": "a", "shop": "s", "amount": 1})]);
    let Err((_, PublisherError::NonRetryable(e))) = &out[0] else {
        panic!("the message must fail");
    };
    assert_eq!(
        e.to_string(),
        "aggregate: field 'fee' is missing or not a number"
    );
}

#[test]
fn skip_leaves_out_only_the_entry_that_cannot_be_computed() {
    let agg = aggregate(&format!("on_error: skip\n{TWO_ENTRIES}"));
    let out = fold_messages(
        &agg,
        vec![
            json!({"card": "a", "shop": "s", "amount": 1, "fee": 2}),
            json!({"card": "a", "shop": "s", "amount": 1}),
            json!({"card": "a", "amount": 1, "fee": 2}),
            json!({"card": "a", "shop": "s", "amount": 1, "fee": 2}),
        ],
    );
    let doc = |i: usize| -> Value {
        serde_json::from_slice(&out[i].as_ref().ok().unwrap().payload).unwrap()
    };
    let skipped = |i: usize| {
        out[i]
            .as_ref()
            .ok()
            .unwrap()
            .metadata
            .get(SKIPPED_KEY)
            .cloned()
    };
    assert_eq!(skipped(0), None);
    for i in [1, 2] {
        assert_eq!(skipped(i).as_deref(), Some("shop_stats"));
        assert_eq!(doc(i)["card_stats"]["n"], json!(i + 1));
        assert_eq!(doc(i)["shop_stats"], Value::Null);
    }
    // The skipped entry's state did not move.
    assert_eq!(doc(3)["shop_stats"], json!({"fee": 4.0, "n": 2}));
    assert_eq!(doc(3)["card_stats"]["n"], json!(4));
}

#[test]
fn skip_passes_on_a_message_no_entry_can_fold() {
    let mixed = "on_error: skip
entries:
  - { key: '${payload:card}', into: a, expression: '(state ?? 0) + amount' }
  - { key: '${payload:card}', into: b, fields: { total: sum(amount) } }";
    for yaml in [format!("on_error: skip\n{TWO_ENTRIES}"), mixed.to_string()] {
        let agg = aggregate(&yaml);
        let out = agg.fold(vec![
            CanonicalMessage::new(b"not json".to_vec(), None),
            msg(json!({"other": 1})),
        ]);
        let first = out[0].as_ref().ok().unwrap();
        assert_eq!(first.payload.as_ref(), b"not json");
        assert!(first.metadata[SKIPPED_KEY].contains(','));
        let second = out[1].as_ref().ok().unwrap();
        assert_eq!(second.payload.as_ref(), br#"{"other":1}"#);
        assert!(second.metadata.contains_key(SKIPPED_KEY));
    }
}

#[tokio::test]
async fn skip_works_against_a_shared_store() {
    let store: std::sync::Arc<dyn StateStore> =
        std::sync::Arc::new(store::MemoryStateStore::default());
    let mut agg = aggregate(&format!("on_error: skip\n{TWO_ENTRIES}"));
    agg.store = Some(store);
    let batch = vec![
        msg(json!({"card": "a", "amount": 1, "fee": 2})),
        msg(json!({"card": "a", "shop": "s", "amount": 1, "fee": 2})),
    ];
    let (out, _) = agg.fold_batch(batch).await.unwrap();
    let first = out[0].as_ref().ok().unwrap();
    assert_eq!(first.metadata[SKIPPED_KEY], "shop_stats");
    let second: Value = serde_json::from_slice(&out[1].as_ref().ok().unwrap().payload).unwrap();
    assert_eq!(second["card_stats"]["n"], json!(2));
    assert_eq!(second["shop_stats"]["n"], json!(1));
}

#[tokio::test]
async fn consumer_nacks_a_message_it_cannot_fold_under_fail() {
    let committed = std::sync::Arc::new(Mutex::new(Vec::new()));
    let inner = OneBatch {
        batch: Some(vec![
            msg(json!({"card": "a", "amount": 1})),
            msg(json!({"amount": 1})),
        ]),
        committed: committed.clone(),
    };
    let config = serde_yaml_ng::from_str(&format!(
        "{{ on_error: fail, consistency: single_writer, key: '${{payload:card}}', into: stats, \
         expression: '{SUM}' }}"
    ))
    .unwrap();
    let mut consumer = AggregateConsumer::new(Box::new(inner), &config, "test")
        .await
        .unwrap();
    let ReceivedBatch { messages, commit } = consumer.receive_batch(10).await.unwrap();
    assert_eq!(messages.len(), 1);
    commit(vec![MessageDisposition::Ack]).await.unwrap();
    assert!(matches!(
        committed.lock().unwrap().as_slice(),
        [MessageDisposition::Ack, MessageDisposition::Nack]
    ));
}

#[test]
fn ema_by_half_life_weights_by_elapsed_time() {
    for into in ["stats", "features.card"] {
        let agg = aggregate(&format!(
            "{{ key: '${{payload:card}}', into: {into}, time: ts, fields: {{ avg: 'ema(amount, 10s)' }} }}"
        ));
        let out = fold_messages(
            &agg,
            vec![
                json!({"card": "a", "amount": 0, "ts": 1000}),
                json!({"card": "a", "amount": 30, "ts": 1010}),
                json!({"card": "a", "amount": 30, "ts": "1970-01-01T00:16:50Z"}),
                json!({"card": "a", "amount": 30, "ts": 1005}),
            ],
        );
        let at = |i: usize| -> f64 {
            let doc: Value =
                serde_json::from_slice(&out[i].as_ref().ok().unwrap().payload).unwrap();
            into.split('.').fold(doc, |d, k| d[k].clone())["avg"]
                .as_f64()
                .unwrap()
        };
        let late = |i: usize| {
            out[i]
                .as_ref()
                .ok()
                .unwrap()
                .metadata
                .get(LATE_KEY)
                .cloned()
        };
        assert_eq!(at(0), 0.0);
        // One half-life later the first value weighs half: 30 / 1.5.
        assert!((at(1) - 20.0).abs() < 1e-9);
        // The same instant as an RFC 3339 time: nothing decays, 60 / 2.5.
        assert!((at(2) - 24.0).abs() < 1e-9);
        assert_eq!(late(2), None);
        // Five seconds back: flagged, and it weighs 0.5^(5/10).
        assert_eq!(late(3).as_deref(), Some(into));
        let w = 0.5f64.sqrt();
        assert!((at(3) - (60.0 + 30.0 * w) / (2.5 + w)).abs() < 1e-9);
    }
}

const TRENDS: &str = "{ key: '${payload:k}', into: s, time: ts, fields: { \
    avg: 'ema(x, 10s)', spread: 'ema_stddev(x, 10s)', n: 'decayed_count(10s)', \
    total: 'decayed_sum(x, 10s)', last: last(x), all: count } }";

fn folded(agg: &Aggregate, payloads: Vec<Value>) -> Vec<Value> {
    fold_messages(agg, payloads)
        .into_iter()
        .map(|m| serde_json::from_slice(&m.ok().unwrap().payload).unwrap())
        .collect()
}

#[test]
fn late_messages_give_the_same_state_as_in_order_ones() {
    let at = |x: i64, ts: i64| json!({"k": "a", "x": x, "ts": ts});
    let in_order = vec![at(4, 100), at(9, 103), at(1, 107), at(6, 115), at(3, 121)];
    let shuffled = vec![at(6, 115), at(4, 100), at(3, 121), at(1, 107), at(9, 103)];
    let a = folded(&aggregate(TRENDS), in_order).pop().unwrap()["s"].clone();
    let b = folded(&aggregate(TRENDS), shuffled).pop().unwrap()["s"].clone();
    for field in ["avg", "spread", "n", "total"] {
        let (a, b) = (a[field].as_f64().unwrap(), b[field].as_f64().unwrap());
        assert!((a - b).abs() < 1e-9, "{field}: {a} != {b}");
    }
    assert_eq!(a["last"], json!(3.0));
    assert_eq!(b["last"], json!(3.0));
    assert_eq!(b["all"], json!(5));
}

#[test]
fn decayed_count_settles_at_rate_times_half_life_over_ln2() {
    let agg = aggregate(
        "{ key: '${payload:k}', into: s, time: ts, emit: previous, \
           fields: { n: 'decayed_count(1m)', total: 'decayed_sum(x, 1m)' } }",
    );
    let mut docs = folded(
        &agg,
        (0..1200)
            .map(|t| json!({"k": 1, "x": 2, "ts": t}))
            .collect(),
    );
    // Before this message, faded to its time: the sum of q^i for i in 1..1200.
    let q = 0.5f64.powf(1.0 / 60.0);
    let expected = (q - q.powi(1200)) / (1.0 - q);
    let last = docs.pop().unwrap()["s"].clone();
    assert!((last["n"].as_f64().unwrap() - expected).abs() < 1e-6);
    assert!((last["total"].as_f64().unwrap() - 2.0 * expected).abs() < 1e-6);
    assert_eq!(docs[0]["s"], Value::Null);
    // The value read a minute after the last message has halved.
    let later = folded(&agg, vec![json!({"k": 1, "x": 2, "ts": 1259})]);
    let n = later[0]["s"]["n"].as_f64().unwrap();
    assert!((n - (expected + 1.0) / 2.0).abs() < 1e-6);
}

#[test]
fn overwrite_metadata_folds_a_late_message_as_the_newest() {
    let agg = aggregate("{ key: '${payload:k}', into: s, time: ts, fields: { v: last(x) } }");
    let late = |overwrite: bool| {
        let mut m = msg(json!({"k": 1, "x": 7, "ts": 50}));
        if overwrite {
            m.metadata.insert(OVERWRITE_KEY.into(), "true".into());
        }
        m
    };
    let out = agg.fold(vec![
        msg(json!({"k": 1, "x": 1, "ts": 9e9})),
        late(false),
        late(true),
        msg(json!({"k": 1, "x": 3, "ts": 60})),
    ]);
    let v = |i: usize| -> Value {
        let doc: Value = serde_json::from_slice(&out[i].as_ref().ok().unwrap().payload).unwrap();
        doc["s"]["v"].clone()
    };
    assert_eq!(v(1), json!(1.0));
    assert!(out[1]
        .as_ref()
        .ok()
        .unwrap()
        .metadata
        .contains_key(LATE_KEY));
    assert_eq!(v(2), json!(7.0));
    assert!(!out[2]
        .as_ref()
        .ok()
        .unwrap()
        .metadata
        .contains_key(LATE_KEY));
    // The clock was set back to 50, so 60 is in order again.
    assert_eq!(v(3), json!(3.0));
}

#[test]
fn decayed_fields_need_a_half_life() {
    for spec in ["decayed_count(0.1)", "decayed_sum(x, 0.5)"] {
        let yaml = format!("{{ key: '${{payload:k}}', into: s, fields: {{ a: '{spec}' }} }}");
        assert!(error(&yaml).contains("half-life"), "{spec}");
    }
    assert!(
        error("{ key: '${payload:k}', into: s, fields: { a: 'decayed_sum(x)' } }")
            .contains("invalid field")
    );
}

#[test]
fn time_is_validated_and_a_message_without_one_fails() {
    assert!(
        error("{ key: '${payload:k}', into: s, time: ts, expression: '1' }").contains("`time`")
    );
    assert!(
        error("{ key: '${payload:k}', into: s, fields: { a: 'ema(x, 5x)' } }")
            .contains("invalid field")
    );
    let agg = aggregate("{ key: '${payload:k}', into: s, time: ts, fields: { n: count } }");
    let out = fold_messages(&agg, vec![json!({"k": 1}), json!({"k": 1, "ts": "soon"})]);
    assert!(out.iter().all(Result::is_err));
}

#[test]
fn timestamps_parse_like_chrono() {
    for text in [
        "2026-10-03T12:34:56Z",
        "2026-02-28T23:59:59.250+02:00",
        "1999-12-31T00:00:00-05:30",
        "2024-02-29T06:00:00.5Z",
    ] {
        let expected = chrono::DateTime::parse_from_rfc3339(text).unwrap();
        let expected = expected.timestamp_millis() as f64 / 1000.0;
        assert_eq!(timestamp(text), Some(expected), "{text}");
    }
    assert_eq!(
        timestamp("2026-10-03 12:34:56"),
        timestamp("2026-10-03T12:34:56Z")
    );
    assert_eq!(timestamp("1700000000000"), Some(1_700_000_000.0));
    assert_eq!(timestamp("1700000000.5"), Some(1_700_000_000.5));
    for bad in [
        "",
        "soon",
        "2026-13-03T12:34:56Z",
        "2026-10-03T12:34:56+0200",
        "2026-10-03",
    ] {
        assert_eq!(timestamp(bad), None, "{bad}");
    }
    assert_eq!(seconds("500ms"), Some(0.5));
    assert_eq!(seconds("2h"), Some(7200.0));
    assert_eq!(seconds("0s"), None);
}

#[test]
fn a_state_stored_before_time_was_set_gains_a_clock() {
    let agg = aggregate("{ key: '${payload:k}', into: s, time: ts, fields: { n: count } }");
    let Stored::Floats(slots) = agg.entries[0].parse_state("[3.0]").unwrap() else {
        panic!("a fields state");
    };
    assert_eq!(slots.as_ref(), [3.0, 0.0]);
    assert!(agg.entries[0].parse_state("[3.0,1.0,2.0]").is_err());
}

#[tokio::test]
async fn read_only_reads_the_stored_states_and_changes_none() {
    let memory = std::sync::Arc::new(store::MemoryStateStore::default());
    let store: std::sync::Arc<dyn StateStore> = memory.clone();
    let yaml = "key: '${payload:card}'\ninto: stats\nfields: { n: count }";
    let mut writer = aggregate(yaml);
    writer.store = Some(store.clone());
    let batch = || vec![msg(json!({"card": "a"})), msg(json!({"card": "a"}))];
    writer.fold_batch(batch()).await.unwrap();
    let before = memory.rows.lock().unwrap().clone();

    for (emit, expected) in [("updated", 3), ("previous", 2)] {
        let mut reader = aggregate(&format!("read_only: true\nemit: {emit}\n{yaml}"));
        reader.store = Some(store.clone());
        let (out, _) = reader.fold_batch(batch()).await.unwrap();
        for folded in out {
            let doc: Value = serde_json::from_slice(&folded.ok().unwrap().payload).unwrap();
            assert_eq!(doc["stats"]["n"], json!(expected));
        }
    }
    assert_eq!(*memory.rows.lock().unwrap(), before);

    let config = serde_yaml_ng::from_str(&format!("read_only: true\n{yaml}"));
    let unstored = Aggregate::connect(&config.unwrap(), "test").await;
    assert!(unstored
        .err()
        .unwrap()
        .to_string()
        .contains("`read_only` needs a `store`"));
}

#[tokio::test]
async fn states_in_memory_only_are_opt_in() {
    let yaml = "key: '${payload:card}'\ninto: stats\nfields: { n: count }";
    let config = serde_yaml_ng::from_str(yaml).unwrap();
    let unstored = Aggregate::connect(&config, "test").await;
    let error = unstored.err().unwrap();
    assert!(error.is::<crate::errors::InvalidConfig>());
    assert!(error.to_string().contains("`consistency: single_writer`"));

    let config = serde_yaml_ng::from_str(&format!("consistency: single_writer\n{yaml}")).unwrap();
    let opted_in = Aggregate::connect(&config, "test").await.unwrap();
    assert!(opted_in.store.is_none() && opted_in.writer.is_none());
}
