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
        status.unwrap_or_else(|| {
            panic!(
                "no exit within {timeout:?} of SIGTERM\n{}",
                self.log_tail()
            )
        })
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
            .args(["copy", "--from", &source, "--to", &raw_uri(&first), "--drain"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start copy");
        let deadline = Instant::now() + Duration::from_secs(120);
        while complete_rows(&first).len() < seeded.len() / 4 {
            assert!(Instant::now() < deadline, "first run never got a quarter in");
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
}
