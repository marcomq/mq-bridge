// Headless `--config` runs: when every route drains (`exit_on_empty`), the process
// exits by itself; otherwise it keeps running until a signal.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mq-bridge-app-headless-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file_route(name: &str, from: &Path, to: &Path, drains: bool) -> String {
    format!(
        "  {name}:\n    exit_on_empty: {drains}\n    input:\n      file:\n        path: '{}'\n        format: raw\n    output:\n      file:\n        path: '{}'\n        format: raw\n",
        from.display(),
        to.display()
    )
}

fn headless(dir: &TestDir, routes: &[String]) -> Child {
    let config = dir.file("config.yaml");
    std::fs::write(&config, format!("routes:\n{}", routes.concat())).expect("write config");
    Command::new(env!("CARGO_BIN_EXE_mq-bridge-app"))
        .args(["--no-ui", "--no-metrics", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start headless CLI")
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

fn seed(dir: &TestDir, name: &str) -> PathBuf {
    let path = dir.file(name);
    std::fs::write(&path, "a\nb\nc\n").expect("seed source");
    path
}

fn rows(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |body| body.lines().count())
}

#[test]
fn a_config_whose_routes_all_drain_exits_when_they_finish() {
    let dir = TestDir::new();
    let (first, second) = (dir.file("first.out"), dir.file("second.out"));
    let mut child = headless(
        &dir,
        &[
            file_route("first", &seed(&dir, "first.in"), &first, true),
            file_route("second", &seed(&dir, "second.in"), &second, true),
        ],
    );

    let status = exit_within(&mut child, Duration::from_secs(60));
    if status.is_none() {
        let _ = child.kill();
    }
    let status = status.expect("the CLI kept running after every route drained");
    assert!(status.success(), "drained run exited with {status}");
    assert_eq!((rows(&first), rows(&second)), (3, 3));
}

#[test]
fn a_config_with_a_continuous_route_keeps_running() {
    let dir = TestDir::new();
    let (drained, continuous) = (dir.file("drained.out"), dir.file("continuous.out"));
    let mut child = headless(
        &dir,
        &[
            file_route("drained", &seed(&dir, "drained.in"), &drained, true),
            file_route(
                "continuous",
                &seed(&dir, "continuous.in"),
                &continuous,
                false,
            ),
        ],
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    while rows(&drained) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = exit_within(&mut child, Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(rows(&drained), 3, "the draining route never finished");
    assert!(
        status.is_none(),
        "exited with {status:?} while a route still runs"
    );
}
