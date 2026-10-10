//! `copy` against real brokers and databases, where `cli_copy_test.rs` uses
//! files and in-process fixtures.
//!
//! Every test is `#[ignore]`d, so `cargo test` stays local and offline. The
//! services are the engine's own integration stacks in
//! `tests/integration/docker-compose`, so a stack that is already up serves
//! these too and the run costs only its own time:
//!
//! ```text
//! crates/cli/tests/docker/run_docker_tests.sh          # default set, ~50s cold
//! crates/cli/tests/docker/run_docker_tests.sh all      # all nine
//! ```
//!
//! By hand, the backend name has to filter the test names as well as select the
//! service: `backend!` skips by returning, so an excluded test still reports
//! `ok` and a run against the wrong service looks green.
//!
//! ```text
//! docker compose -f tests/integration/docker-compose/kafka.yml up -d --wait
//! MQB_TEST_BACKEND=kafka cargo test -p mq-bridge-app \
//!     --test cli_copy_docker_test -- --ignored --test-threads=1 kafka
//! ```
//!
//! These cover what the local suite cannot reach: that a seeded topic drains
//! exactly once, that a consumer group or SQL cursor resumes where the last run
//! stopped, that `--batch-size`/`--concurrency` neither drop nor duplicate a row
//! against a live server, and that payloads survive each backend's storage model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- CLI driving

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mq-bridge-app"))
}

fn copy_with_options(from: &str, to: &str, options: &[&str]) -> Output {
    let mut command = cli();
    command.args(["copy", "--from", from, "--to", to, "--drain"]);
    command.args(options);
    command.output().expect("run CLI copy")
}

/// Runs a copy, failing with the case name and stderr. Every leg of a backend
/// matrix goes through here, so a failure names which leg broke.
fn copy_ok(case: &str, from: &str, to: &str, options: &[&str]) -> Output {
    let output = copy_with_options(from, to, options);
    assert!(
        output.status.success(),
        "{case} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

// ------------------------------------------------------------------ selection

/// Honours `MQB_TEST_BACKEND` as `mq_bridge::test_utils::should_run` does. The
/// engine helper is not reachable from this test target, so the rule is repeated.
fn should_run(backend: &str) -> bool {
    let filter = std::env::var("MQB_TEST_BACKEND")
        .unwrap_or_default()
        .to_lowercase();
    filter.is_empty() || backend.to_lowercase().contains(&filter)
}

/// Skips the body when `MQB_TEST_BACKEND` excludes this backend. A skip is a
/// pass: only one service runs at a time, so the rest must not fail to connect.
macro_rules! backend {
    ($name:expr) => {
        if !should_run($name) {
            eprintln!("skipping {}: excluded by MQB_TEST_BACKEND", $name);
            return;
        }
    };
}

// ------------------------------------------------------------------- fixtures

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mq-bridge-app-cli-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A name no other test or earlier run holds. Topics, queues and tables outlive
/// the test that made them, so a fixed name would let leftovers count as a
/// result. Underscores only: it must be legal as a SQL identifier too.
fn unique(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &id[..12])
}

/// `count` distinct JSON rows, so a dropped or duplicated row shows up in the
/// assertion instead of hiding behind identical payloads.
fn numbered_rows(count: usize) -> Vec<String> {
    (0..count).map(row).collect()
}

fn row(index: usize) -> String {
    format!(
        r#"{{"id":{index},"name":"row-{index}","amount":{}}}"#,
        index * 10
    )
}

fn seed_rows(dir: &TestDir, name: &str, rows: &[String]) -> PathBuf {
    let path = dir.path().join(name);
    let mut body = rows.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(&path, body).expect("seed row source");
    path
}

fn raw_uri(path: impl AsRef<Path>) -> String {
    format!("file://{}?format=raw", path.as_ref().display())
}

fn read_rows(path: impl AsRef<Path>) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|body| body.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Rows in a canonical order. A route writes with several workers, so which row
/// lands first is not part of the contract; that each lands once is.
fn sorted(rows: &[String]) -> Vec<String> {
    let mut sorted = rows.to_vec();
    sorted.sort();
    sorted
}

/// Reports the first difference and the count rather than dumping every row.
fn assert_rows_eq<T: PartialEq + std::fmt::Debug>(actual: &[T], expected: &[T], case: &str) {
    if let Some((index, (got, want))) = actual
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (got, want))| got != want)
    {
        panic!("{case}: row {index} differs\n  copied:   {got:?}\n  expected: {want:?}");
    }
    assert_eq!(
        actual.len(),
        expected.len(),
        "{case}: copied {} rows, expected {}",
        actual.len(),
        expected.len()
    );
}

/// The seeded rows the shared `amount >= 500` filter must keep.
fn above_500(rows: &[String]) -> Vec<String> {
    let kept: Vec<String> = rows
        .iter()
        .filter(|row| {
            let value: serde_json::Value = serde_json::from_str(row).expect("seeded row is JSON");
            value["amount"].as_i64().expect("amount is a number") >= 500
        })
        .cloned()
        .collect();
    assert!(
        !kept.is_empty() && kept.len() < rows.len(),
        "the filter must keep some rows and drop others to prove anything"
    );
    kept
}

// --------------------------------------------------------- the shared matrix

/// The three things every store-and-forward backend must get right, against one
/// freshly named topic/stream/collection: a byte-exact round trip, a `--filter`
/// over what the backend returned, and an awkward `--batch-size`/`--concurrency`
/// pair that still moves each row exactly once.
///
/// Phases 2 and 3 need the same rows again, and backends offer that in one of
/// two ways — see [`Replay`].
fn run_backend_matrix(
    backend: &str,
    rows: usize,
    sink: &str,
    source: &dyn Fn(&str) -> String,
    replay: Replay,
) {
    let dir = TestDir::new();
    let seeded = numbered_rows(rows);
    let input = seed_rows(&dir, "input.jsonl", &seeded);

    let seed = |case: &str| {
        copy_ok(
            &format!("{backend}: {case}"),
            &raw_uri(&input),
            sink,
            &["--batch-size", "64"],
        );
    };
    let phase = |case: &str, options: &[&str], expected: &[String]| {
        let out = dir.path().join(format!("{case}.jsonl"));
        copy_ok(
            &format!("{backend}: {case}"),
            &source(case),
            &raw_uri(&out),
            options,
        );
        assert_rows_eq(
            &sorted(&read_rows(&out)),
            &sorted(expected),
            &format!("{backend}: {case}"),
        );
    };
    let reseed = |case: &str| {
        if matches!(replay, Replay::Reseed) {
            seed(case);
        }
    };

    seed("seed");
    phase("roundtrip", &[], &seeded);
    reseed("re-seed for the filtered read");
    phase(
        "filtered",
        &["--filter", "amount >= 500"],
        &above_500(&seeded),
    );
    reseed("re-seed for the batched read");
    phase(
        "batched",
        &["--batch-size", "7", "--concurrency", "3"],
        &seeded,
    );
}

/// How a backend gives the matrix the same rows a second time.
#[derive(Clone, Copy)]
enum Replay {
    /// The messages are still stored and a fresh consumer group re-reads them
    /// from the start (see [`per_phase_source`]).
    Group,
    /// The previous phase's read cannot be repeated, so the phase writes the
    /// rows again (see [`fixed_source`]). Two different causes: AMQP hands each
    /// message to one reader and drops it, while NATS keeps its messages but
    /// reads them through a durable consumer that resumes at its ack floor.
    Reseed,
}

/// A source whose consumer group is unique per phase, so each phase reads the
/// whole backlog instead of resuming after the one before it.
fn per_phase_source(base: &str, group_param: &str, stem: &str) -> impl Fn(&str) -> String + use<> {
    let (base, group_param, stem) = (base.to_string(), group_param.to_string(), stem.to_string());
    move |phase: &str| format!("{base}&{group_param}={stem}_{phase}")
}

/// A source that ignores the phase, for reads that always start from the beginning.
fn fixed_source(uri: &str) -> impl Fn(&str) -> String + use<> {
    let uri = uri.to_string();
    move |_phase: &str| uri.clone()
}

// ============================================================== SQL backends

#[cfg(any(feature = "full", feature = "sqlx"))]
mod sql {
    use super::*;

    pub const POSTGRES: &str = "postgres://testuser:testpass@localhost:5432/testdb";
    pub const MYSQL: &str = "mysql://testuser:testpass@localhost:3306/testdb";
    pub const MARIADB: &str = "mysql://testuser:testpass@localhost:3307/testdb";

    /// Decodes the base16 a `BYTEA`/`BLOB` column becomes on the way out — there
    /// is no JSON scalar for bytes, so asserting on it pins that encoding too.
    pub fn unhex(hex: &str) -> String {
        assert!(
            hex.len().is_multiple_of(2),
            "base16 has an even length, got {hex:?}"
        );
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("payload is base16"))
            .collect();
        String::from_utf8(bytes).expect("payload is UTF-8")
    }

    /// Reads a table back through the CLI's own cursor source, as `(id, payload)`.
    fn read_back(dir: &TestDir, name: &str, source: &str, options: &[&str]) -> Vec<(i64, String)> {
        let out = dir.path().join(name);
        copy_ok("sql read-back", source, &raw_uri(&out), options);
        read_rows(&out)
            .iter()
            .map(|line| {
                let row: serde_json::Value =
                    serde_json::from_str(line).unwrap_or_else(|e| panic!("row {line:?}: {e}"));
                let id = row["id"].as_i64().expect("auto-created table has an id");
                (
                    id,
                    unhex(row["payload"].as_str().expect("payload is base16")),
                )
            })
            .collect()
    }

    /// A bulk insert into a table the copy creates itself, then a cursor read
    /// back that has to reproduce every payload under a distinct key.
    pub fn round_trip(flavour: &str, base: &str) {
        let dir = TestDir::new();
        let table = unique("cli_rt");
        let seeded = numbered_rows(120);
        let input = seed_rows(&dir, "input.jsonl", &seeded);

        let sink = format!("{base}?table={table}&auto_create_table=true");
        copy_ok(
            &format!("{flavour}: insert"),
            &raw_uri(&input),
            &sink,
            &["--batch-size", "32"],
        );

        let source = format!("{base}?table={table}&cursor_column=id");
        let back = read_back(&dir, "out.jsonl", &source, &[]);

        let payloads: Vec<String> = back.iter().map(|(_, p)| p.clone()).collect();
        assert_rows_eq(
            &sorted(&payloads),
            &sorted(&seeded),
            &format!("{flavour}: round trip"),
        );

        let ids: std::collections::BTreeSet<i64> = back.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids.len(),
            back.len(),
            "{flavour}: the auto-created key repeated itself"
        );
    }

    /// A resumed cursor copy must neither re-emit what it moved nor skip what
    /// arrived in between.
    pub fn cursor_resume(flavour: &str, base: &str) {
        let dir = TestDir::new();
        let table = unique("cli_resume");
        let sink = format!("{base}?table={table}&auto_create_table=true");
        let checkpoint = dir.path().join("cursor.json");
        // `cursor_id` names the checkpoint and the file store carries it between
        // the two CLI processes; without it the second run starts over.
        let source = format!(
            "{base}?table={table}&cursor_column=id&cursor_id=cli_resume&checkpoint_store={}",
            nested(&format!("file://{}", checkpoint.display()))
        );

        let first = numbered_rows(50);
        copy_ok(
            &format!("{flavour}: seed first"),
            &raw_uri(seed_rows(&dir, "first.jsonl", &first)),
            &sink,
            &[],
        );
        let run_one = read_back(&dir, "run1.jsonl", &source, &["--resume"]);
        assert_eq!(
            run_one.len(),
            first.len(),
            "{flavour}: the first resumable run moved {} of {} rows",
            run_one.len(),
            first.len()
        );

        // These arrive only after the first run committed its cursor.
        let second: Vec<String> = (50..80).map(row).collect();
        copy_ok(
            &format!("{flavour}: seed second"),
            &raw_uri(seed_rows(&dir, "second.jsonl", &second)),
            &sink,
            &[],
        );

        let run_two = read_back(&dir, "run2.jsonl", &source, &["--resume"]);
        let payloads: Vec<String> = run_two.iter().map(|(_, p)| p.clone()).collect();
        assert_rows_eq(
            &sorted(&payloads),
            &sorted(&second),
            &format!("{flavour}: the resumed run"),
        );
    }
}

/// A nested endpoint URI, escaped to travel as a query value.
#[cfg(any(feature = "full", feature = "sqlx"))]
fn nested(uri: &str) -> String {
    url::form_urlencoded::byte_serialize(uri.as_bytes()).collect()
}

#[cfg(any(feature = "full", feature = "sqlx"))]
#[test]
#[ignore = "requires docker compose"]
fn postgres_round_trips_every_row_through_a_table_the_copy_created() {
    backend!("postgres");
    sql::round_trip("postgres", sql::POSTGRES);
}

#[cfg(any(feature = "full", feature = "sqlx"))]
#[test]
#[ignore = "requires docker compose"]
fn postgres_resume_moves_each_row_exactly_once_across_runs() {
    backend!("postgres");
    sql::cursor_resume("postgres", sql::POSTGRES);
}

#[cfg(any(feature = "full", feature = "sqlx"))]
#[test]
#[ignore = "requires docker compose"]
fn mysql_round_trips_every_row_through_a_table_the_copy_created() {
    backend!("mysql");
    sql::round_trip("mysql", sql::MYSQL);
}

#[cfg(any(feature = "full", feature = "sqlx"))]
#[test]
#[ignore = "requires docker compose"]
fn mysql_resume_moves_each_row_exactly_once_across_runs() {
    backend!("mysql");
    sql::cursor_resume("mysql", sql::MYSQL);
}

/// MariaDB shares the endpoint with MySQL, so this is a wire-compatibility
/// check rather than a second copy of the MySQL test.
#[cfg(any(feature = "full", feature = "sqlx"))]
#[test]
#[ignore = "requires docker compose"]
fn mariadb_round_trips_every_row_through_a_table_the_copy_created() {
    backend!("mariadb");
    sql::round_trip("mariadb", sql::MARIADB);
}

// ==================================================================== Kafka

#[cfg(any(feature = "full", feature = "kafka"))]
#[test]
#[ignore = "requires docker compose"]
fn kafka_round_trips_every_row_and_filters_and_batches_it() {
    backend!("kafka");
    let topic = unique("cli_kafka");
    let sink = format!("kafka://localhost:9092?topic={topic}");
    // A fresh group per phase: with a `group_id` the consumer starts at the
    // earliest offset, so reusing one group would leave later phases nothing.
    let source = per_phase_source(&sink, "group_id", &format!("g_{topic}"));
    run_backend_matrix("kafka", 120, &sink, &source, Replay::Group);
}

/// The broker-side counterpart of the SQL cursor test: the group's committed
/// offsets are what the second run resumes from.
#[cfg(any(feature = "full", feature = "kafka"))]
#[test]
#[ignore = "requires docker compose"]
fn kafka_consumer_group_resumes_where_the_previous_run_stopped() {
    backend!("kafka");
    let dir = TestDir::new();
    let topic = unique("cli_kafka_resume");
    let sink = format!("kafka://localhost:9092?topic={topic}");
    let source = format!("{sink}&group_id=g_{topic}");

    let first = numbered_rows(40);
    copy_ok(
        "kafka resume: seed first",
        &raw_uri(seed_rows(&dir, "first.jsonl", &first)),
        &sink,
        &[],
    );
    let run_one = dir.path().join("run1.jsonl");
    copy_ok("kafka resume: first run", &source, &raw_uri(&run_one), &[]);
    assert_rows_eq(
        &sorted(&read_rows(&run_one)),
        &sorted(&first),
        "kafka resume: the first run",
    );

    let second: Vec<String> = (100..130).map(row).collect();
    copy_ok(
        "kafka resume: seed second",
        &raw_uri(seed_rows(&dir, "second.jsonl", &second)),
        &sink,
        &[],
    );
    let run_two = dir.path().join("run2.jsonl");
    copy_ok("kafka resume: second run", &source, &raw_uri(&run_two), &[]);
    assert_rows_eq(
        &sorted(&read_rows(&run_two)),
        &sorted(&second),
        "kafka resume: the second run saw only what arrived after the first",
    );
}

// ===================================================================== NATS

#[cfg(any(feature = "full", feature = "nats"))]
#[test]
#[ignore = "requires docker compose"]
fn nats_jetstream_round_trips_every_row_and_filters_and_batches_it() {
    backend!("nats");
    let stream = unique("CLI_NATS");
    // The subject sits under the stream name deliberately: a JetStream publisher
    // given a wildcard-free subject creates the stream with `<stream>.>` as its
    // only subject, so a subject outside that prefix would not be captured.
    let subject = format!("{stream}.data");
    let sink = format!("nats://localhost:4222?subject={subject}&stream={stream}");
    // The durable consumer's name is derived from stream+subject, so all three
    // phases share it and each resumes at the previous phase's ack floor — the
    // stream still holds every message (retention is Limits), but this consumer
    // will not redeliver them. Hence `Reseed`, and hence no `deliver_policy`:
    // it only applies when the durable is created, so it would be a no-op here.
    // Anything that changed the durable's name would make phases 2 and 3 read
    // the whole accumulated backlog instead.
    run_backend_matrix("nats", 120, &sink, &fixed_source(&sink), Replay::Reseed);
}

// ================================================================== MongoDB

#[cfg(any(feature = "full", feature = "mongodb"))]
const MONGO: &str = "mongodb://localhost:27017?database=testdb";

#[cfg(any(feature = "full", feature = "mongodb"))]
#[test]
#[ignore = "requires docker compose"]
fn mongodb_round_trips_every_document_and_filters_and_batches_it() {
    backend!("mongodb");
    let collection = unique("cli_mongo");
    let sink = format!("{MONGO}&collection={collection}&format=raw&id_field=id");
    // `snapshot` reads the existing documents and ends, which is what `--drain`
    // needs. The default `capture_all` tails a change stream, and that requires
    // a replica set the compose file does not run.
    let source = format!("{MONGO}&collection={collection}&format=raw&consume=snapshot");

    let dir = TestDir::new();
    let seeded = numbered_rows(120);
    copy_ok(
        "mongodb: seed",
        &raw_uri(seed_rows(&dir, "input.jsonl", &seeded)),
        &sink,
        &["--batch-size", "64"],
    );

    let out = dir.path().join("out.jsonl");
    copy_ok("mongodb: round trip", &source, &raw_uri(&out), &[]);
    assert_rows_eq(
        &sorted(&strip_mongo_ids(&read_rows(&out))),
        &sorted(&seeded),
        "mongodb: round trip",
    );

    let filtered = dir.path().join("filtered.jsonl");
    copy_ok(
        "mongodb: filtered read",
        &source,
        &raw_uri(&filtered),
        &["--filter", "amount >= 500", "--batch-size", "7"],
    );
    assert_rows_eq(
        &sorted(&strip_mongo_ids(&read_rows(&filtered))),
        &sorted(&above_500(&seeded)),
        "mongodb: filtered read",
    );
}

/// `format=raw` stores the payload as the document, so a read back carries the
/// `_id` Mongo assigned; everything else must match what was sent.
#[cfg(any(feature = "full", feature = "mongodb"))]
fn strip_mongo_ids(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|line| {
            let mut doc: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("document {line:?}: {e}"));
            doc.remove("_id");
            // Rebuilt in the seeded rows' shape, so the comparison is on content
            // rather than key order or spacing.
            format!(
                r#"{{"id":{},"name":{},"amount":{}}}"#,
                doc["id"], doc["name"], doc["amount"]
            )
        })
        .collect()
}

/// `id_field` keys the document by the row's own id, so a re-run upserts instead
/// of storing everything twice — the idempotency an ETL re-run depends on.
#[cfg(any(feature = "full", feature = "mongodb"))]
#[test]
#[ignore = "requires docker compose"]
fn mongodb_id_field_makes_a_repeated_copy_idempotent() {
    backend!("mongodb");
    let dir = TestDir::new();
    let collection = unique("cli_mongo_idem");
    let sink = format!("{MONGO}&collection={collection}&format=raw&id_field=id");
    let source = format!("{MONGO}&collection={collection}&format=raw&consume=snapshot");
    let seeded = numbered_rows(60);
    let input = seed_rows(&dir, "input.jsonl", &seeded);

    for run in 1..=2 {
        copy_ok(
            &format!("mongodb idempotency: run {run}"),
            &raw_uri(&input),
            &sink,
            &[],
        );
    }

    let out = dir.path().join("out.jsonl");
    copy_ok(
        "mongodb idempotency: read back",
        &source,
        &raw_uri(&out),
        &[],
    );
    assert_rows_eq(
        &sorted(&strip_mongo_ids(&read_rows(&out))),
        &sorted(&seeded),
        "mongodb: copying the same rows twice stored them twice",
    );
}

// ============================================================ Redis Streams

#[cfg(any(feature = "full", feature = "redis-streams"))]
#[test]
#[ignore = "requires docker compose"]
fn redis_streams_round_trips_every_row_and_filters_and_batches_it() {
    backend!("redis");
    let stream = unique("cli_redis");
    let sink = format!("redis://localhost:6379?stream={stream}");
    // `read_from_start` makes a new group begin at the head of the stream rather
    // than its tail, so every phase replays what was seeded.
    let base = format!("{sink}&read_from_start=true");
    let source = per_phase_source(&base, "group", &format!("g_{stream}"));
    run_backend_matrix("redis", 120, &sink, &source, Replay::Group);
}

// ======================================================== AMQP (RabbitMQ)

/// A queue read is destructive, so each phase re-seeds rather than replaying.
#[cfg(any(feature = "full", feature = "amqp"))]
#[test]
#[ignore = "requires docker compose"]
fn amqp_round_trips_every_row_and_filters_and_batches_it() {
    backend!("amqp");
    let queue = unique("cli_amqp");
    let uri = format!("amqp://guest:guest@localhost:5672?queue={queue}");
    run_backend_matrix("amqp", 120, &uri, &fixed_source(&uri), Replay::Reseed);
}

// ===================================================================== MQTT

/// MQTT keeps nothing for a topic nobody subscribes to, so this is the one
/// backend that cannot be seeded first: the reader has to be running before the
/// writer publishes.
#[cfg(all(unix, any(feature = "full", feature = "mqtt")))]
#[test]
#[ignore = "requires docker compose"]
fn mqtt_delivers_to_a_subscriber_that_was_already_running() {
    backend!("mqtt");
    let dir = TestDir::new();
    let topic = unique("cli_mqtt");
    let out = dir.path().join("out.jsonl");
    let base = format!("mqtt://localhost:1883?topic={topic}&qos=1");
    let source = format!("{base}&client_id={}", unique("sub"));
    let sink = format!("{base}&client_id={}", unique("pub"));

    let _reader = BackgroundCopy(
        cli()
            .args(["copy", "--from", &source, "--to", &raw_uri(&out)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start background MQTT reader"),
    );

    // The broker only routes to an established subscription, and the reader's
    // output is discarded so there is no readiness line to synchronise on.
    // Republishing is safe -- the assertion below is on the distinct set, and
    // QoS 1 already permits duplicates -- so retry until it lands rather than
    // sleeping long enough to be sure.
    let seeded = numbered_rows(40);
    let input = raw_uri(seed_rows(&dir, "input.jsonl", &seeded));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        copy_ok("mqtt: publish", &input, &sink, &[]);
        let settle = Instant::now() + Duration::from_millis(500);
        while Instant::now() < settle && distinct(&read_rows(&out)).len() < seeded.len() {
            std::thread::sleep(Duration::from_millis(25));
        }
        if distinct(&read_rows(&out)).len() >= seeded.len() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the MQTT subscriber never received the published rows"
        );
    }

    // QoS 1 is at-least-once, so a redelivery is protocol-legal and the count is
    // not fixed. What must hold is that every row arrived and nothing else did.
    assert_rows_eq(
        &distinct(&read_rows(&out)),
        &sorted(&seeded),
        "mqtt: what the subscriber received",
    );
}

/// Kills the spawned copy on drop, so a failed assertion or a deadline loop
/// running out cannot leave a background reader running.
#[cfg(all(unix, any(feature = "full", feature = "mqtt")))]
struct BackgroundCopy(std::process::Child);

#[cfg(all(unix, any(feature = "full", feature = "mqtt")))]
impl Drop for BackgroundCopy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(all(unix, any(feature = "full", feature = "mqtt")))]
fn distinct(rows: &[String]) -> Vec<String> {
    let set: std::collections::BTreeSet<&String> = rows.iter().collect();
    set.into_iter().cloned().collect()
}

// ====================================================================== TLS

/// A broker started for one test with a certificate made for that run, removed
/// on drop. The shared stacks have no TLS listener, and adding one would make
/// every other user of them generate certificates first.
#[cfg(unix)]
struct TlsContainer {
    name: String,
    /// The certificate clients have to trust; it is its own CA.
    ca_file: PathBuf,
}

#[cfg(unix)]
impl TlsContainer {
    /// Mounts a directory holding `server.pem`, `server.key` and `files` at `/tls`
    /// and waits for `ready` in the container's log.
    fn start(
        dir: &TestDir,
        name: &str,
        port: u16,
        files: &[(&str, &str)],
        image_and_command: &[&str],
        ready: &str,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let mount = dir.path().join("tls");
        std::fs::create_dir(&mount).expect("create the tls directory");
        let generated =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
                .expect("generate a certificate");
        let ca_file = mount.join("server.pem");
        std::fs::write(&ca_file, generated.cert.pem()).expect("write certificate");
        std::fs::write(
            mount.join("server.key"),
            generated.signing_key.serialize_pem(),
        )
        .expect("write key");
        for (file, body) in files {
            std::fs::write(mount.join(file), body).expect("write broker file");
        }
        // The broker runs as its own user inside the container.
        for path in [dir.path(), mount.as_path()] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("open the tls directory");
        }

        let _ = Command::new("docker").args(["rm", "-f", name]).output();
        let container = Self {
            name: name.to_string(),
            ca_file,
        };
        let started = Command::new("docker")
            .args(["run", "-d", "--name", name])
            .args(["-p", &format!("127.0.0.1:{port}:{port}")])
            .args(["-v", &format!("{}:/tls:ro", mount.display())])
            .args(image_and_command)
            .output()
            .expect("run docker");
        assert!(
            started.status.success(),
            "could not start {name}: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        let deadline = Instant::now() + Duration::from_secs(120);
        while !container.logs().contains(ready) {
            assert!(
                Instant::now() < deadline,
                "{name} never logged {ready:?}: {}",
                container.logs()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        container
    }

    fn logs(&self) -> String {
        let output = Command::new("docker")
            .args(["logs", &self.name])
            .output()
            .expect("read container logs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// The `tls={...}` URI parameter that trusts this broker.
    fn trusting(&self) -> String {
        format!(r#"tls={{"ca_file":"{}"}}"#, self.ca_file.display())
    }
}

#[cfg(unix)]
impl Drop for TlsContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.name])
            .output();
    }
}

/// Whether a draining copy ended successfully within `limit`. A copy that is
/// still retrying its connection by then is stopped and counts as not delivered.
#[cfg(unix)]
fn copy_succeeds_within(from: &str, to: &str, limit: Duration) -> bool {
    let mut child = cli()
        .args(["copy", "--from", from, "--to", to, "--drain"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start CLI copy");
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll CLI copy") {
            return status.success();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An `mqtts://` URL has to mean TLS with a verified broker: a plain client and a
/// client without the broker's CA must both be unable to publish.
#[cfg(all(unix, any(feature = "full", feature = "mqtt")))]
#[test]
#[ignore = "requires docker"]
fn mqtt_tls_delivers_only_from_clients_that_trust_the_broker() {
    backend!("mqtt");
    let dir = TestDir::new();
    let broker = TlsContainer::start(
        &dir,
        "mqb-cli-mqtt-tls",
        8883,
        &[(
            "mosquitto.conf",
            "listener 8883\nallow_anonymous true\ncertfile /tls/server.pem\nkeyfile /tls/server.key\n",
        )],
        &[
            "eclipse-mosquitto:2.0.22",
            "mosquitto",
            "-c",
            "/tls/mosquitto.conf",
        ],
        "running",
    );

    let topic = unique("cli_mqtt_tls");
    let out = dir.path().join("out.jsonl");
    let query = format!("topic={topic}&qos=1");
    let trusted = format!("mqtts://localhost:8883?{query}&{}", broker.trusting());
    let source = format!("{trusted}&client_id={}", unique("sub"));
    let client = |base: &str| format!("{base}&client_id={}", unique("pub"));

    let _reader = BackgroundCopy(
        cli()
            .args(["copy", "--from", &source, "--to", &raw_uri(&out)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start background MQTT reader"),
    );

    let seeded = numbered_rows(40);
    let input = raw_uri(seed_rows(&dir, "input.jsonl", &seeded));
    let limit = Duration::from_secs(15);
    assert!(
        !copy_succeeds_within(
            &input,
            &client(&format!("mqtt://localhost:8883?{query}")),
            limit
        ),
        "a plain mqtt:// client published to a TLS listener"
    );
    assert!(
        !copy_succeeds_within(
            &input,
            &client(&format!("mqtts://localhost:8883?{query}")),
            limit
        ),
        "a client published to a broker whose certificate it cannot verify"
    );

    // As in the plain test: republish until the subscription is established.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        copy_ok("mqtt tls: publish", &input, &client(&trusted), &[]);
        let settle = Instant::now() + Duration::from_millis(500);
        while Instant::now() < settle && distinct(&read_rows(&out)).len() < seeded.len() {
            std::thread::sleep(Duration::from_millis(25));
        }
        if distinct(&read_rows(&out)).len() >= seeded.len() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the MQTT subscriber never received the rows over TLS: {}",
            broker.logs()
        );
    }
    assert_rows_eq(
        &distinct(&read_rows(&out)),
        &sorted(&seeded),
        "mqtt tls: what the subscriber received",
    );
}
