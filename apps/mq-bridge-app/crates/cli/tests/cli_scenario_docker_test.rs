//! Route-level scenarios against the real binary: what the engine's
//! `tests/integration` pipeline and chaos tests check in-process, here through
//! `--config` and `copy` runs, plus what only a process can show — a graceful
//! SIGTERM and a `kill -9` followed by a resume.
//!
//! The in-process `memory` legs become files: one route writes the seeded rows
//! to the backend, a second reads them back into a file the test watches.
//!
//! ```text
//! docker compose -f tests/integration/docker-compose/kafka.yml up -d --wait
//! MQB_TEST_BACKEND=kafka cargo test -p mq-bridge-app \
//!     --test cli_scenario_docker_test -- --ignored --test-threads=1 kafka
//! ```
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::*;

// ------------------------------------------------------------ process control

/// A headless `--config` run whose log stays readable for failure messages.
struct Headless {
    child: Child,
    log: PathBuf,
}

impl Headless {
    fn start(dir: &TestDir, routes: &str) -> Self {
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, format!("routes:\n{routes}")).expect("write config");
        let log = dir.path().join("mqb.log");
        let child = cli()
            .args(["--no-ui", "--no-metrics", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).expect("create log"))
            .spawn()
            .expect("start headless CLI");
        Self { child, log }
    }

    fn log_tail(&self) -> String {
        tail(&self.log)
    }

    /// One SIGTERM, then the graceful stop must finish on its own.
    fn terminate(mut self, timeout: Duration) -> ExitStatus {
        signal(&self.child, "TERM");
        let status = exit_within(&mut self.child, timeout);
        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        status
            .unwrap_or_else(|| panic!("no exit within {timeout:?} of SIGTERM\n{}", self.log_tail()))
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -{name} failed");
}

fn exit_within(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll CLI") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn tail(path: &Path) -> String {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = body.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

// ------------------------------------------------------------------- services

/// `docker compose` on the engine's integration stacks, which the CI job and
/// `run_docker_tests.sh` bring up before the tests run.
struct Compose(PathBuf);

impl Compose {
    fn new(backend: &str) -> Self {
        Self(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../tests/integration/docker-compose")
                .join(format!("{backend}.yml")),
        )
    }

    fn run(&self, args: &[&str]) {
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.0)
            .args(args)
            .status()
            .expect("run docker compose");
        assert!(status.success(), "docker compose {args:?} failed");
    }

    fn stop(&self, service: &str) {
        self.run(&["stop", service]);
    }

    /// Back up and healthy, so the route's reconnect is what the test measures.
    fn start(&self, service: &str) {
        self.run(&["up", "-d", "--wait", "--wait-timeout", "120", service]);
    }
}

// ----------------------------------------------------------------- assertions

/// Complete JSON lines only: a process killed mid-write may leave a torn last line.
fn complete_rows(path: &Path) -> Vec<String> {
    read_rows(path)
        .into_iter()
        .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        .collect()
}

fn distinct(rows: &[String]) -> BTreeSet<String> {
    rows.iter().cloned().collect()
}

/// Waits until `path` holds `expected` distinct rows, or fails with the log.
fn wait_for_rows(path: &Path, expected: usize, timeout: Duration, run: &Headless) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let rows = complete_rows(path);
        if distinct(&rows).len() >= expected {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {expected} distinct rows after {timeout:?}\n{}",
            distinct(&rows).len(),
            run.log_tail()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn assert_all_arrived(seeded: &[String], got: &[String], case: &str) {
    let got = distinct(got);
    let missing: Vec<&String> = seeded.iter().filter(|row| !got.contains(*row)).collect();
    assert!(
        missing.is_empty(),
        "{case}: {} of {} rows missing, first {:?}",
        missing.len(),
        seeded.len(),
        missing.first()
    );
}

// ====================================================================== Kafka

#[cfg(any(feature = "full", feature = "kafka"))]
mod kafka {
    use super::*;

    const BROKER: &str = "localhost:9092";

    /// The engine's `memory_to_kafka` / `kafka_to_memory` pair with file legs.
    fn routes(input: &Path, output: &Path, topic: &str) -> String {
        format!(
            r#"  file_to_kafka:
    concurrency: 2
    batch_size: 1024
    exit_on_empty: true
    input:
      file: {{ path: '{input}', format: raw }}
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      kafka:
        url: "{BROKER}"
        topic: "{topic}"
        producer_options:
          - ["queue.buffering.max.ms", "1"]
          - ["acks", "1"]
          - ["compression.type", "snappy"]
  kafka_to_file:
    concurrency: 2
    batch_size: 1024
    input:
      kafka:
        url: "{BROKER}"
        topic: "{topic}"
        group_id: "g_{topic}"
    output:
      file: {{ path: '{output}', format: raw }}
"#,
            input = input.display(),
            output = output.display(),
        )
    }

    #[test]
    #[ignore = "requires docker compose"]
    fn kafka_pipeline_delivers_every_row_and_stops_on_sigterm() {
        backend!("kafka");
        let dir = TestDir::new();
        let topic = unique("cli_scn_kafka");
        let seeded = numbered_rows(20_000);
        let input = seed_rows(&dir, "input.jsonl", &seeded);
        let output = dir.path().join("output.jsonl");

        let run = Headless::start(&dir, &routes(&input, &output, &topic));
        let got = wait_for_rows(&output, seeded.len(), Duration::from_secs(120), &run);
        assert_all_arrived(&seeded, &got, "kafka pipeline");

        let status = run.terminate(Duration::from_secs(30));
        assert!(status.success(), "SIGTERM exit: {status}");
    }

    /// The engine's chaos test: the broker goes away mid-stream and comes back.
    /// Every row must still arrive; duplicates are allowed (at-least-once).
    #[test]
    #[ignore = "requires docker compose, restarts the broker"]
    fn kafka_broker_restart_mid_stream_loses_nothing() {
        backend!("kafka");
        let dir = TestDir::new();
        let topic = unique("cli_scn_kafka_chaos");
        let seeded = numbered_rows(100_000);
        let input = seed_rows(&dir, "input.jsonl", &seeded);
        let output = dir.path().join("output.jsonl");
        let compose = Compose::new("kafka");

        let run = Headless::start(&dir, &routes(&input, &output, &topic));
        wait_for_rows(&output, 1, Duration::from_secs(60), &run);
        compose.stop("kafka");
        let at_stop = distinct(&complete_rows(&output)).len();
        std::thread::sleep(Duration::from_secs(2));
        compose.start("kafka");
        assert!(
            at_stop < seeded.len(),
            "every row had arrived before the broker stopped; the restart proved nothing"
        );

        let got = wait_for_rows(&output, seeded.len(), Duration::from_secs(300), &run);
        assert_all_arrived(&seeded, &got, "kafka chaos");
        eprintln!(
            "kafka chaos: {} rows at stop, {} duplicates",
            at_stop,
            got.len() - distinct(&got).len()
        );

        let status = run.terminate(Duration::from_secs(30));
        assert!(status.success(), "SIGTERM exit: {status}");
    }

    /// `kill -9` mid-copy, then the same group again: what the first run did not
    /// commit is read again, so the two outputs together hold every row.
    #[test]
    #[ignore = "requires docker compose"]
    fn kafka_copy_killed_mid_run_resumes_without_loss() {
        backend!("kafka");
        let dir = TestDir::new();
        let topic = unique("cli_scn_kafka_kill");
        let sink = format!("kafka://{BROKER}?topic={topic}");
        let source = format!("{sink}&group_id=g_{topic}");
        let seeded = numbered_rows(100_000);
        copy_ok(
            "kafka kill: seed",
            &raw_uri(seed_rows(&dir, "input.jsonl", &seeded)),
            &sink,
            &["--batch-size", "1024"],
        );

        let first = dir.path().join("first.jsonl");
        let mut child = cli()
            .args([
                "copy",
                "--from",
                &source,
                "--to",
                &raw_uri(&first),
                "--drain",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start copy");
        let deadline = Instant::now() + Duration::from_secs(120);
        while complete_rows(&first).len() < seeded.len() / 4 {
            assert!(
                Instant::now() < deadline,
                "first run never got a quarter in"
            );
            assert!(
                child.try_wait().expect("poll copy").is_none(),
                "the first run finished before it could be killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        signal(&child, "KILL");
        let _ = child.wait();
        let first_rows = complete_rows(&first);
        assert!(
            first_rows.len() < seeded.len(),
            "the first run copied everything before the kill; nothing to resume"
        );

        // The killed member still holds the group for `session.timeout.ms`; the
        // drain must wait it out rather than fail.
        let second = dir.path().join("second.jsonl");
        copy_ok("kafka kill: resume", &source, &raw_uri(&second), &[]);
        let second_rows = complete_rows(&second);

        let mut union = first_rows.clone();
        union.extend(second_rows.iter().cloned());
        assert_all_arrived(&seeded, &union, "kafka kill -9 resume");
        eprintln!(
            "kafka kill: first {} rows, second {} rows, {} duplicates",
            first_rows.len(),
            second_rows.len(),
            union.len() - distinct(&union).len()
        );
    }

    fn spawn_copy(from: &str, out: &Path) -> Child {
        cli()
            .args(["copy", "--from", from, "--to", &raw_uri(out), "--drain"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start copy")
    }

    fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !done() {
            assert!(Instant::now() < deadline, "{what} within {timeout:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Redpanda Connect #2299: partitions move while the first member has records
    /// prefetched. Whatever it commits must not pass a record nobody delivered, so a
    /// final drain of the group finds every row the two members missed.
    #[test]
    #[ignore = "requires docker compose"]
    fn kafka_rebalance_between_two_processes_loses_nothing() {
        backend!("kafka");
        let dir = TestDir::new();
        let topic = unique("cli_scn_kafka_rebalance");
        let sink = format!("kafka://{BROKER}?topic={topic}");
        let group = format!("{sink}&group_id=g_{topic}");
        let seeded = numbered_rows(100_000);
        copy_ok(
            "kafka rebalance: seed",
            &raw_uri(seed_rows(&dir, "input.jsonl", &seeded)),
            &sink,
            &["--batch-size", "1024"],
        );

        // Paced, so the prefetcher runs ahead of the route when the partitions move.
        let paced = format!("{group}|limiter?messages_per_second=5000");
        let (out_a, out_b) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
        let mut a = spawn_copy(&paced, &out_a);
        wait_until("member A started", Duration::from_secs(60), || {
            complete_rows(&out_a).len() >= 5_000
        });
        let mut b = spawn_copy(&paced, &out_b);
        wait_until("member B got partitions", Duration::from_secs(60), || {
            !complete_rows(&out_b).is_empty()
        });
        assert!(
            a.try_wait().expect("poll A").is_none(),
            "member A finished before B joined; no rebalance happened"
        );
        signal(&a, "TERM");
        let a_status = exit_within(&mut a, Duration::from_secs(60));
        assert!(
            a_status.is_some_and(|status| status.success()),
            "member A after SIGTERM: {a_status:?}"
        );
        let b_status = exit_within(&mut b, Duration::from_secs(180));
        if b_status.is_none() {
            let _ = b.kill();
        }

        let out_c = dir.path().join("c.jsonl");
        copy_ok(
            "kafka rebalance: final drain",
            &group,
            &raw_uri(&out_c),
            &[],
        );
        let (rows_a, rows_b, rows_c) = (
            complete_rows(&out_a),
            complete_rows(&out_b),
            complete_rows(&out_c),
        );
        let mut union = rows_a.clone();
        union.extend(rows_b.iter().cloned());
        union.extend(rows_c.iter().cloned());
        assert_all_arrived(&seeded, &union, "kafka rebalance");
        eprintln!(
            "kafka rebalance: A {} rows, B {} rows (exit {b_status:?}), final drain {} rows, \
             {} duplicates",
            rows_a.len(),
            rows_b.len(),
            rows_c.len(),
            union.len() - distinct(&union).len()
        );
    }
}

// ======================================================================= NATS

#[cfg(any(feature = "full", feature = "nats"))]
mod nats {
    use super::*;

    /// JetStream, with the subject under the stream name: a publisher creates the
    /// stream as `<stream>.>`, so a subject outside that prefix would not be kept.
    fn endpoint() -> String {
        let stream = unique("CLI_SCN_NATS");
        format!("nats://localhost:4222?subject={stream}.data&stream={stream}")
    }

    fn routes(input: &Path, output: &Path, uri: &str) -> String {
        let (subject, stream) = uri
            .split_once("subject=")
            .and_then(|(_, rest)| rest.split_once("&stream="))
            .expect("nats uri");
        format!(
            r#"  file_to_nats:
    concurrency: 2
    batch_size: 1024
    exit_on_empty: true
    input:
      file: {{ path: '{input}', format: raw }}
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      nats:
        url: "nats://localhost:4222"
        subject: "{subject}"
        stream: "{stream}"
  nats_to_file:
    concurrency: 2
    batch_size: 1024
    input:
      nats:
        url: "nats://localhost:4222"
        subject: "{subject}"
        stream: "{stream}"
    output:
      file: {{ path: '{output}', format: raw }}
"#,
            input = input.display(),
            output = output.display(),
        )
    }

    #[test]
    #[ignore = "requires docker compose"]
    fn nats_pipeline_delivers_every_row_and_stops_on_sigterm() {
        backend!("nats");
        let dir = TestDir::new();
        let seeded = numbered_rows(50_000);
        let input = seed_rows(&dir, "input.jsonl", &seeded);
        let output = dir.path().join("output.jsonl");

        let run = Headless::start(&dir, &routes(&input, &output, &endpoint()));
        let got = wait_for_rows(&output, seeded.len(), Duration::from_secs(120), &run);
        assert_all_arrived(&seeded, &got, "nats pipeline");

        let status = run.terminate(Duration::from_secs(30));
        assert!(status.success(), "SIGTERM exit: {status}");
    }

    /// The engine's chaos test: the server goes away mid-stream and comes back with
    /// its JetStream store. Every row must still arrive; duplicates are allowed.
    #[test]
    #[ignore = "requires docker compose, restarts the server"]
    fn nats_server_restart_mid_stream_loses_nothing() {
        backend!("nats");
        let dir = TestDir::new();
        let seeded = numbered_rows(100_000);
        let input = seed_rows(&dir, "input.jsonl", &seeded);
        let output = dir.path().join("output.jsonl");
        let compose = Compose::new("nats");

        let run = Headless::start(&dir, &routes(&input, &output, &endpoint()));
        wait_for_rows(&output, 1, Duration::from_secs(60), &run);
        compose.stop("nats");
        let at_stop = distinct(&complete_rows(&output)).len();
        std::thread::sleep(Duration::from_secs(2));
        compose.start("nats");
        assert!(
            at_stop < seeded.len(),
            "every row had arrived before the server stopped; the restart proved nothing"
        );

        let got = wait_for_rows(&output, seeded.len(), Duration::from_secs(300), &run);
        assert_all_arrived(&seeded, &got, "nats chaos");
        eprintln!(
            "nats chaos: {} rows at stop, {} duplicates",
            at_stop,
            got.len() - distinct(&got).len()
        );

        let status = run.terminate(Duration::from_secs(30));
        assert!(status.success(), "SIGTERM exit: {status}");
    }

    /// `kill -9` mid-copy, then the same durable consumer again: what the first run
    /// did not ack is redelivered, so the two outputs together hold every row.
    #[test]
    #[ignore = "requires docker compose"]
    fn nats_copy_killed_mid_run_resumes_without_loss() {
        backend!("nats");
        let dir = TestDir::new();
        let uri = endpoint();
        let seeded = numbered_rows(100_000);
        copy_ok(
            "nats kill: seed",
            &raw_uri(seed_rows(&dir, "input.jsonl", &seeded)),
            &uri,
            &["--batch-size", "1024"],
        );

        let first = dir.path().join("first.jsonl");
        let mut child = cli()
            .args(["copy", "--from", &uri, "--to", &raw_uri(&first), "--drain"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start copy");
        let deadline = Instant::now() + Duration::from_secs(120);
        while complete_rows(&first).len() < seeded.len() / 4 {
            assert!(
                Instant::now() < deadline,
                "first run never got a quarter in"
            );
            assert!(
                child.try_wait().expect("poll copy").is_none(),
                "the first run finished before it could be killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        signal(&child, "KILL");
        let _ = child.wait();
        let first_rows = complete_rows(&first);
        assert!(
            first_rows.len() < seeded.len(),
            "the first run copied everything before the kill; nothing to resume"
        );

        // Started right away: the drain must wait out `ack_wait` for what the killed
        // run held instead of exiting early (U13 in upstream-issue-triage.md).
        let second = dir.path().join("second.jsonl");
        copy_ok("nats kill: resume", &uri, &raw_uri(&second), &[]);
        let second_rows = complete_rows(&second);

        let mut union = first_rows.clone();
        union.extend(second_rows.iter().cloned());
        assert_all_arrived(&seeded, &union, "nats kill -9 resume");
        eprintln!(
            "nats kill: first {} rows, second {} rows, {} duplicates",
            first_rows.len(),
            second_rows.len(),
            union.len() - distinct(&union).len()
        );
    }
}

// =================================================================== Postgres

#[cfg(any(feature = "full", feature = "sqlx"))]
mod postgres {
    use super::*;
    use std::io::Write;

    const URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
    /// The logical-replication stack: CDC needs it, and the cursor tests run on it
    /// too, so one server serves the whole module.
    const STACK: &str = "postgres_cdc";

    fn psql(compose: &Compose) -> Command {
        let mut command = Command::new("docker");
        command.arg("compose").arg("-f").arg(&compose.0).args([
            "exec",
            "-T",
            "postgres",
            "psql",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            "testuser",
            "-d",
            "testdb",
        ]);
        command
    }

    fn sql(compose: &Compose, statement: &str) {
        let status = psql(compose)
            .args(["-c", statement])
            .status()
            .expect("run psql");
        assert!(status.success(), "psql failed: {statement}");
    }

    fn query(compose: &Compose, statement: &str) -> String {
        let out = psql(compose)
            .args(["-tA", "-c", statement])
            .output()
            .expect("run psql");
        assert!(out.status.success(), "psql failed: {statement}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn file_store(path: &Path) -> String {
        url::form_urlencoded::byte_serialize(format!("file://{}", path.display()).as_bytes())
            .collect()
    }

    /// The `id` of each complete row; snapshot rows and change events are both flat rows.
    fn ids(path: &Path) -> Vec<i64> {
        complete_rows(path)
            .iter()
            .filter_map(|row| serde_json::from_str::<serde_json::Value>(row).ok()?["id"].as_i64())
            .collect()
    }

    /// Redpanda Connect #4584: a crash mid-snapshot must not lose rows. Rows inserted
    /// during the snapshot arrive through it or through the slot; duplicates are allowed.
    #[test]
    #[ignore = "requires docker compose"]
    fn postgres_cdc_capture_all_killed_mid_snapshot_resumes_without_loss() {
        backend!("postgres");
        let dir = TestDir::new();
        let compose = Compose::new(STACK);
        let table = unique("cli_scn_snap");
        const SEEDED: i64 = 500_000;
        const DURING: i64 = 1_000;
        sql(
            &compose,
            &format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, name TEXT); \
                 INSERT INTO {table} SELECT g, 'row-' || g FROM generate_series(1, {SEEDED}) g; \
                 CREATE PUBLICATION {table}_pub FOR TABLE {table}"
            ),
        );
        let source = format!(
            "postgres-cdc://testuser:testpass@localhost:5432/testdb?publication={table}_pub\
             &slot_name={table}_slot&consume=capture_all&cursor_id={table}\
             &checkpoint_store={}",
            file_store(&dir.path().join("checkpoint.json"))
        );

        let first = dir.path().join("first.jsonl");
        let mut child = cli()
            .args([
                "copy",
                "--from",
                &source,
                "--to",
                &raw_uri(&first),
                "--drain",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start copy");
        let wait_for = |rows: usize, child: &mut Child| {
            let deadline = Instant::now() + Duration::from_secs(120);
            while complete_rows(&first).len() < rows {
                assert!(
                    Instant::now() < deadline,
                    "first run stalled below {rows} rows"
                );
                assert!(
                    child.try_wait().expect("poll copy").is_none(),
                    "the first run finished before it could be killed"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for(1, &mut child);
        sql(
            &compose,
            &format!(
                "INSERT INTO {table} SELECT g, 'during-' || g \
                 FROM generate_series({}, {}) g",
                SEEDED + 1,
                SEEDED + DURING
            ),
        );
        wait_for(SEEDED as usize / 4, &mut child);
        signal(&child, "KILL");
        let _ = child.wait();
        let first_ids = ids(&first);
        assert!(
            (first_ids.len() as i64) < SEEDED,
            "the snapshot finished before the kill; nothing to resume"
        );

        let second = dir.path().join("second.jsonl");
        copy_ok("cdc snapshot kill: resume", &source, &raw_uri(&second), &[]);
        let second_ids = ids(&second);
        query(
            &compose,
            &format!("SELECT pg_drop_replication_slot('{table}_slot')"),
        );

        let union: BTreeSet<i64> = first_ids.iter().chain(&second_ids).copied().collect();
        let missing: Vec<i64> = (1..=SEEDED + DURING)
            .filter(|id| !union.contains(id))
            .collect();
        assert!(
            missing.is_empty(),
            "{} rows missing, first {:?}; first run {}, second run {}",
            missing.len(),
            missing.first(),
            first_ids.len(),
            second_ids.len()
        );
        eprintln!(
            "cdc snapshot kill: first {} rows, second {} rows, {} duplicates",
            first_ids.len(),
            second_ids.len(),
            first_ids.len() + second_ids.len() - union.len()
        );
    }

    /// Debezium's idle-table WAL growth: while only an unpublished table is written, the
    /// slot must still confirm the server's position, so the server can drop old WAL.
    #[test]
    #[ignore = "requires docker compose"]
    fn postgres_cdc_slot_advances_while_published_tables_are_idle() {
        backend!("postgres");
        let dir = TestDir::new();
        let compose = Compose::new(STACK);
        let table = unique("cli_scn_idle");
        sql(
            &compose,
            &format!(
                "CREATE TABLE {table}_a (id BIGINT PRIMARY KEY); \
                 CREATE TABLE {table}_b (id BIGINT PRIMARY KEY, pad TEXT); \
                 CREATE PUBLICATION {table}_pub FOR TABLE {table}_a"
            ),
        );
        let slot = format!("{table}_slot");
        let out = dir.path().join("out.jsonl");
        let run = Headless::start(
            &dir,
            &format!(
                r#"  idle:
    input:
      postgres_cdc:
        url: "{URL}"
        publication: {table}_pub
        slot_name: {slot}
        status_interval_ms: 500
    output:
      file: {{ path: '{}', format: raw }}
"#,
                out.display()
            ),
        );
        let active = format!("SELECT active FROM pg_replication_slots WHERE slot_name = '{slot}'");
        let deadline = Instant::now() + Duration::from_secs(60);
        while query(&compose, &active) != "t" {
            assert!(
                Instant::now() < deadline,
                "slot never became active\n{}",
                run.log_tail()
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        sql(
            &compose,
            &format!(
                "INSERT INTO {table}_b SELECT g, repeat('x', 200) FROM generate_series(1, 200000) g"
            ),
        );
        let written = query(&compose, "SELECT pg_current_wal_lsn()");
        let caught_up = format!(
            "SELECT confirmed_flush_lsn >= '{written}'::pg_lsn FROM pg_replication_slots \
             WHERE slot_name = '{slot}'"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while query(&compose, &caught_up) != "t" {
            assert!(
                Instant::now() < deadline,
                "slot stuck at {} behind {written} while only {table}_b was written\n{}",
                query(
                    &compose,
                    &format!(
                        "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name = '{slot}'"
                    )
                ),
                run.log_tail()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(run.terminate(Duration::from_secs(30)).success());
        query(
            &compose,
            &format!("SELECT pg_drop_replication_slot('{slot}')"),
        );
        assert!(
            complete_rows(&out).is_empty(),
            "nothing of {table}_a changed"
        );
    }

    /// Kafka Connect's `timestamp.delay.interval.ms` case: id=1 commits after id=2 was
    /// read. A cursor that moved past 2 must not skip 1 for good.
    #[test]
    #[ignore = "requires docker compose"]
    fn postgres_cursor_keeps_a_row_whose_transaction_commits_late() {
        backend!("postgres");
        let dir = TestDir::new();
        let compose = Compose::new(STACK);
        let table = unique("cli_scn_late");
        sql(
            &compose,
            &format!("CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, name TEXT)"),
        );

        let mut slow = psql(&compose)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start slow transaction");
        let mut stdin = slow.stdin.take().expect("psql stdin");
        writeln!(stdin, "BEGIN; INSERT INTO {table}(name) VALUES ('slow');").unwrap();
        stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        // The open insert holds id=1 once the sequence has been called.
        let probe = format!("SELECT is_called FROM {table}_id_seq");
        loop {
            let out = psql(&compose)
                .args(["-tA", "-c", &probe])
                .output()
                .expect("read sequence");
            if String::from_utf8_lossy(&out.stdout).trim() == "t" {
                break;
            }
            assert!(Instant::now() < deadline, "the slow insert never took id=1");
            std::thread::sleep(Duration::from_millis(100));
        }
        sql(
            &compose,
            &format!("INSERT INTO {table}(name) VALUES ('fast')"),
        );

        let checkpoint = dir.path().join("cursor.json");
        let checkpoint_uri: String = url::form_urlencoded::byte_serialize(
            format!("file://{}", checkpoint.display()).as_bytes(),
        )
        .collect();
        let source = format!(
            "{URL}?table={table}&cursor_column=id&cursor_id=late&checkpoint_store={checkpoint_uri}"
        );
        let first = dir.path().join("first.jsonl");
        copy_ok(
            "late commit: first",
            &source,
            &raw_uri(&first),
            &["--resume"],
        );

        writeln!(stdin, "COMMIT;").unwrap();
        drop(stdin);
        assert!(slow.wait().expect("slow transaction").success());

        let second = dir.path().join("second.jsonl");
        copy_ok(
            "late commit: second",
            &source,
            &raw_uri(&second),
            &["--resume"],
        );

        let mut ids: Vec<i64> = complete_rows(&first)
            .iter()
            .chain(complete_rows(&second).iter())
            .map(|row| {
                serde_json::from_str::<serde_json::Value>(row).unwrap()["id"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![1, 2],
            "first {:?}, second {:?}",
            read_rows(&first),
            read_rows(&second)
        );
    }
}
