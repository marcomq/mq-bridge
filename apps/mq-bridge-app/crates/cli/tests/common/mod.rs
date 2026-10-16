//! Helpers shared by the docker-backed CLI test targets. Each target uses a
//! different subset, hence the blanket `dead_code` allowance.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// ---------------------------------------------------------------- CLI driving

pub fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mq-bridge-app"))
}

pub fn copy_with_options(from: &str, to: &str, options: &[&str]) -> Output {
    let mut command = cli();
    command.args(["copy", "--from", from, "--to", to, "--drain"]);
    command.args(options);
    command.output().expect("run CLI copy")
}

/// Runs a copy, failing with the case name and stderr. Every leg of a backend
/// matrix goes through here, so a failure names which leg broke.
pub fn copy_ok(case: &str, from: &str, to: &str, options: &[&str]) -> Output {
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
pub fn should_run(backend: &str) -> bool {
    let filter = std::env::var("MQB_TEST_BACKEND")
        .unwrap_or_default()
        .to_lowercase();
    filter.is_empty() || backend.to_lowercase().contains(&filter)
}

/// Skips the body when `MQB_TEST_BACKEND` excludes this backend. A skip is a
/// pass: only one service runs at a time, so the rest must not fail to connect.
#[macro_export]
macro_rules! backend {
    ($name:expr) => {
        if !$crate::common::should_run($name) {
            eprintln!("skipping {}: excluded by MQB_TEST_BACKEND", $name);
            return;
        }
    };
}

// ------------------------------------------------------------------- fixtures

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mq-bridge-app-cli-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
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
pub fn unique(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &id[..12])
}

/// `count` distinct JSON rows, so a dropped or duplicated row shows up in the
/// assertion instead of hiding behind identical payloads.
pub fn numbered_rows(count: usize) -> Vec<String> {
    (0..count).map(row).collect()
}

pub fn row(index: usize) -> String {
    format!(
        r#"{{"id":{index},"name":"row-{index}","amount":{}}}"#,
        index * 10
    )
}

pub fn seed_rows(dir: &TestDir, name: &str, rows: &[String]) -> PathBuf {
    let path = dir.path().join(name);
    let mut body = rows.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(&path, body).expect("seed row source");
    path
}

pub fn raw_uri(path: impl AsRef<Path>) -> String {
    format!("file://{}?format=raw", path.as_ref().display())
}

pub fn read_rows(path: impl AsRef<Path>) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|body| body.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Rows in a canonical order. A route writes with several workers, so which row
/// lands first is not part of the contract; that each lands once is.
pub fn sorted(rows: &[String]) -> Vec<String> {
    let mut sorted = rows.to_vec();
    sorted.sort();
    sorted
}

/// Reports the first difference and the count rather than dumping every row.
pub fn assert_rows_eq<T: PartialEq + std::fmt::Debug>(actual: &[T], expected: &[T], case: &str) {
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
