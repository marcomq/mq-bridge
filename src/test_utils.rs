#![allow(dead_code)] // This module contains helpers used by various integration tests.
use crate::traits::{BoxFuture, MessageDisposition, MessagePublisher, Received};
use crate::traits::{ConsumerError, MessageConsumer, PublisherError, ReceivedBatch, SentBatch};
use crate::{CanonicalMessage, Route};
use async_channel::{bounded, Receiver, Sender};
use once_cell::sync::Lazy;
use serde_json::json;
use std::any::Any;
use std::collections::HashSet;
use std::fmt::Display;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

/// Minimal view of the test payload used to extract the message id without
/// parsing the whole JSON object into a `serde_json::Value`.
#[derive(serde::Deserialize)]
struct MessageNumHeader {
    message_num: u64,
}

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::endpoints::memory::MemoryChannel;

pub const PERF_TEST_BATCH_MESSAGE_COUNT: usize = 100_000;
pub const PERF_TEST_SINGLE_MESSAGE_COUNT: usize = 10_000;
pub const PERF_TEST_MESSAGE_COUNT: usize = PERF_TEST_BATCH_MESSAGE_COUNT;
pub const PERF_TEST_CONCURRENCY: usize = 100;
const MAX_PARALLEL_COMMITS: usize = 4096;
const PERF_SEND_MAX_RETRIES: usize = 5;
pub const PERF_CLEANUP_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Binds port 0 so the OS picks a free port, then releases it for the caller to bind. The gap
/// between release and rebind is racy under parallel tests; nothing here reserves the port.
pub fn get_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

/// A struct to hold the performance results for a single test run.
#[derive(Debug, Clone, Default)]
pub struct PerformanceResult {
    pub test_name: String,
    pub write_performance: f64,
    pub read_performance: f64,
    pub single_write_performance: f64,
    pub single_read_performance: f64,
}

/// A global, thread-safe collector for performance results.
static PERFORMANCE_RESULTS: Lazy<Mutex<Vec<PerformanceResult>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

/// Global lock to serialize tests that use Docker containers.
static DOCKER_TEST_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

/// Per-feature elapsed time tracker to enforce time budgets across sub-benchmarks.
static BENCH_FEATURE_ELAPSED: Lazy<AsyncMutex<std::collections::HashMap<String, Duration>>> =
    Lazy::new(|| AsyncMutex::new(std::collections::HashMap::new()));

/// Sub-benchmarks that have already timed out and should be skipped on later Criterion samples.
static BENCH_TIMED_OUT_SUBBENCHES: Lazy<AsyncMutex<HashSet<String>>> =
    Lazy::new(|| AsyncMutex::new(HashSet::new()));

/// Features whose total time budget has been exceeded and should skip remaining sub-benchmarks.
static BENCH_ABORTED_FEATURES: Lazy<AsyncMutex<HashSet<String>>> =
    Lazy::new(|| AsyncMutex::new(HashSet::new()));

fn bench_subbench_key(feature: &str, subbench: &str) -> String {
    format!("{feature}::{subbench}")
}

async fn with_subbench_timeout_configured<F>(
    feature: &str,
    subbench: &str,
    per_sub_timeout: Duration,
    max_feature_total: Duration,
    fut: F,
) -> Duration
where
    F: std::future::Future<Output = Duration> + Send,
{
    let measured = match tokio::time::timeout(per_sub_timeout, fut).await {
        Ok(d) => d,
        Err(_) => {
            eprintln!("BENCH-TIMEOUT feature={} subbench={}", feature, subbench);
            BENCH_TIMED_OUT_SUBBENCHES
                .lock()
                .await
                .insert(bench_subbench_key(feature, subbench));
            per_sub_timeout
        }
    };

    // Update accumulated time for the feature.
    let total_elapsed = {
        let mut map = BENCH_FEATURE_ELAPSED.lock().await;
        let entry = map.entry(feature.to_string()).or_insert(Duration::ZERO);
        *entry += measured;
        *entry
    };

    if total_elapsed > max_feature_total {
        let mut aborted = BENCH_ABORTED_FEATURES.lock().await;
        if aborted.insert(feature.to_string()) {
            eprintln!(
                "BENCH-WARNING feature={} exceeded total time budget: {}s",
                feature,
                total_elapsed.as_secs()
            );
        }
    }

    measured
}

/// Returns true when the current Criterion sample should be skipped because the
/// sub-benchmark already timed out or the feature budget has been exhausted.
pub async fn should_skip_subbench(feature: &str, subbench: &str) -> bool {
    if BENCH_ABORTED_FEATURES.lock().await.contains(feature) {
        return true;
    }

    BENCH_TIMED_OUT_SUBBENCHES
        .lock()
        .await
        .contains(&bench_subbench_key(feature, subbench))
}

/// Run a sub-benchmark future with a per-subbench timeout (60s) and accumulate
/// elapsed time per feature. If the per-feature total exceeds 180s, a warning
/// is printed. Returns the measured duration (or the timeout duration on timeout).
pub async fn with_subbench_timeout<F>(feature: &str, subbench: &str, fut: F) -> std::time::Duration
where
    F: std::future::Future<Output = std::time::Duration> + Send,
{
    with_subbench_timeout_configured(
        feature,
        subbench,
        Duration::from_secs(60),
        Duration::from_secs(180),
        fut,
    )
    .await
}

pub fn should_run(test_name: &str) -> bool {
    let filter = std::env::var("MQB_TEST_BACKEND")
        .unwrap_or_default()
        .to_lowercase();
    filter.is_empty() || test_name.to_lowercase().contains(&filter)
}

/// Adds a performance result to the global collector.
pub fn add_performance_result(result: PerformanceResult) {
    println!(
        "Performance Result for {}: Write Batch: {}, Read Batch: {}, Write Single: {}, Read Single: {}",
        result.test_name,
        format_pretty(result.write_performance),
        format_pretty(result.read_performance),
        format_pretty(result.single_write_performance),
        format_pretty(result.single_read_performance)
    );
    PERFORMANCE_RESULTS.lock().unwrap().push(result);
}

/// A mock struct whose Drop implementation will print the summary table.
pub struct PerformanceSummaryPrinter;

pub struct DockerController {
    compose_file: String,
}

impl DockerController {
    pub fn new(compose_file: &str) -> Self {
        Self {
            compose_file: compose_file.to_string(),
        }
    }

    pub fn stop_service(&self, service: &str) {
        println!(
            "Stopping docker-compose service {} from {}...",
            service, self.compose_file
        );
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("stop")
            .arg(service)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to stop docker compose service");

        assert!(status.success(), "docker compose stop failed");
        self.await_stopped(service);
    }

    /// Blocks until the daemon stops reporting the service as running.
    ///
    /// `compose stop` can return while the container is still listed running, and
    /// a `compose up` that reads that state reports `Running`, skips the start and
    /// then fails its own `--wait` on the container that has meanwhile exited.
    fn await_stopped(&self, service: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.is_running(service) {
            if Instant::now() >= deadline {
                println!("docker still reports {service} as running 30s after stop");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether compose lists a running container for the service. A failed query
    /// answers `false`: `start_service` reports a container that never comes back.
    fn is_running(&self, service: &str) -> bool {
        Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose_file)
            .args(["ps", "-q", "--status", "running"])
            .arg(service)
            .output()
            .map(|out| out.status.success() && !out.stdout.trim_ascii().is_empty())
            .unwrap_or(false)
    }

    /// Starts the service and waits until it is running/healthy again. Plain
    /// `docker compose start` returns before the broker accepts connections,
    /// which made reconnect tests race the container startup.
    pub fn start_service(&self, service: &str) {
        // Retried once: a stale `Running` makes compose skip the start and fail
        // immediately, and by the second attempt the container reads as exited.
        for attempt in 1..=2 {
            println!(
                "Starting docker-compose service {} from {}...",
                service, self.compose_file
            );
            let status = Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&self.compose_file)
                .arg("up")
                .arg("-d")
                .arg("--wait")
                .arg("--wait-timeout")
                .arg("120")
                .arg(service)
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .expect("Failed to start docker compose service");

            if status.success() {
                return;
            }
            self.dump_diagnostics(service);
            assert!(attempt < 2, "docker compose start failed");
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Prints `ps` and the service logs; used when a restart fails or a test
    /// times out waiting for the service.
    pub fn dump_diagnostics(&self, service: &str) {
        for args in [
            vec!["ps", "-a"],
            vec!["logs", "--no-color", "--tail", "100", service],
        ] {
            let _ = Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&self.compose_file)
                .args(&args)
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();
        }
    }
}

pub struct DockerCompose {
    compose_file: String,
}

impl DockerCompose {
    pub fn new(compose_file: &str) -> Self {
        Self {
            compose_file: compose_file.to_string(),
        }
    }

    pub fn up(&self) {
        println!(
            "Starting docker-compose services from {}...",
            self.compose_file
        );
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("up")
            .arg("-d")
            .arg("--wait")
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to start docker compose");

        if !status.success() {
            let _ = Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&self.compose_file)
                .arg("ps")
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();
            let _ = Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&self.compose_file)
                .arg("logs")
                .arg("--no-color")
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();
        }

        assert!(status.success(), "docker compose up --wait failed");
        println!("Services from {} should be up.", self.compose_file);
    }

    pub fn down(&self) {
        println!(
            "Stopping docker-compose services from {}...",
            self.compose_file
        );
        Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("down")
            .arg("-v")
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to stop docker compose");
        println!("Services from {} stopped.", self.compose_file);
    }

    pub fn controller(&self) -> DockerController {
        DockerController::new(&self.compose_file)
    }
}

impl Drop for DockerCompose {
    fn drop(&mut self) {
        self.down();
    }
}

pub fn generate_test_messages(num_messages: usize) -> Vec<CanonicalMessage> {
    let mut messages = Vec::with_capacity(num_messages);
    for i in 0..num_messages {
        let payload = format!(r#"{{"message_num":{},"test_id":"integration"}}"#, i);
        let msg = CanonicalMessage::new(payload.into_bytes(), Some(fast_uuid_v7::gen_id()));
        messages.push(msg);
    }
    messages
}

/// A test harness to simplify integration testing of bridge pipelines.
struct TestHarness {
    in_channel: MemoryChannel,
    out_channel: MemoryChannel,
    messages_to_send: Vec<CanonicalMessage>,
}

impl TestHarness {
    /// Creates a new TestHarness for a given broker and configuration.
    fn new(in_route: Route, out_route: Route, num_messages: usize) -> Self {
        let messages_to_send = generate_test_messages(num_messages);

        // The input to the system is the input of the `memory_to_*` route.
        let in_channel = in_route.input.channel().unwrap();

        // The final output from the system is the output of the `*_to_memory` route.
        let out_channel = out_route.output.channel().unwrap();

        Self {
            in_channel,
            out_channel,
            messages_to_send,
        }
    }

    /// Sends all generated test messages to the input channel.
    async fn send_messages(&self) {
        self.in_channel
            .fill_messages(self.messages_to_send.clone())
            .await
            .unwrap();
    }
}

pub async fn run_pipeline_test(broker_name: &str, config_yaml: &str) {
    run_pipeline_test_internal(
        broker_name,
        broker_name,
        config_yaml,
        5,
        false,
        None,
        0,
        false,
    )
    .await;
}

pub async fn run_performance_pipeline_test(
    broker_name: &str,
    config_yaml: &str,
    num_messages: usize,
) {
    run_performance_pipeline_test_named(broker_name, broker_name, config_yaml, num_messages).await;
}

/// Like [`run_performance_pipeline_test`], but uses `display_name` for the summary
/// label while still resolving routes by `broker_name`. Lets backends that share a
/// route key (e.g. the `sqlx` postgres/mysql/mariadb pipelines, or grpc client/server
/// modes) report distinct rows in the performance summary.
pub async fn run_performance_pipeline_test_named(
    broker_name: &str,
    display_name: &str,
    config_yaml: &str,
    num_messages: usize,
) {
    run_pipeline_test_internal(
        broker_name,
        display_name,
        config_yaml,
        num_messages,
        true,
        None,
        0,
        false,
    )
    .await;
}

/// Like [`run_performance_pipeline_test_named`], but for pipelines whose read side is a genuine
/// at-least-once source (e.g. a MongoDB change stream, which redelivers on any resume). The count
/// assertion then requires every message to arrive at least once (no loss) and tolerates duplicates
/// rather than demanding exact-once totals — reflecting the CDC contract instead of masking a bug.
pub async fn run_performance_pipeline_test_at_least_once_named(
    broker_name: &str,
    display_name: &str,
    config_yaml: &str,
    num_messages: usize,
) {
    run_pipeline_test_internal(
        broker_name,
        display_name,
        config_yaml,
        num_messages,
        true,
        None,
        0,
        true,
    )
    .await;
}

/// TEMP diagnostic: measures the consume+commit route in isolation. Produces `num_messages`
/// into the broker first (untimed), stops the producer, then deploys the consume route and
/// times the pure drain (first-received -> last-received). Comparing this against the coupled
/// pipeline number tells us whether the consume side is the real bottleneck.
#[cfg(feature = "perf-diagnostics")]
pub async fn run_consume_only_bench(broker_name: &str, config_yaml: &str, num_messages: usize) {
    let yaml_val: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(config_yaml).expect("Failed to parse YAML config");
    let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
    let routes: std::collections::HashMap<String, Route> =
        serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

    let in_route_name = format!("memory_to_{}", broker_name.to_lowercase());
    let out_route_name = format!("{}_to_memory", broker_name.to_lowercase());
    let in_route = routes.get(&in_route_name).unwrap().clone();
    let out_route = routes.get(&out_route_name).unwrap().clone();

    let in_channel = in_route.input.channel().unwrap();
    let out_channel = out_route.output.channel().unwrap();

    // --- Produce phase (untimed) ---
    in_route
        .deploy(&in_route_name)
        .await
        .expect("deploy in_route");
    in_channel
        .fill_messages(generate_test_messages(num_messages))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await; // let it drain to the broker
    Route::stop(&in_route_name).await; // drop producer -> flush
    tokio::time::sleep(Duration::from_secs(1)).await;

    // --- Consume phase (timed: first-received -> last-received) ---
    out_route
        .deploy(&out_route_name)
        .await
        .expect("deploy out_route");

    let mut received = 0usize;
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(180);
    while received < num_messages && Instant::now() < deadline {
        let batch = out_channel.drain_messages();
        if batch.is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        } else {
            if first.is_none() {
                first = Some(Instant::now());
            }
            received += batch.len();
            last = Instant::now();
        }
    }
    Route::stop(&out_route_name).await;

    let first = first.expect("consume-only: no messages received");
    let secs = last.duration_since(first).as_secs_f64().max(1e-9);
    println!(
        "\n=== CONSUME-ONLY [{}]: {} msgs drained in {:.3}s => {:.0} msg/s ===\n",
        broker_name,
        received,
        secs,
        received as f64 / secs
    );
}

#[cfg(feature = "perf-diagnostics")]
pub async fn run_produce_only_bench(broker_name: &str, config_yaml: &str, num_messages: usize) {
    let yaml_val: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(config_yaml).expect("Failed to parse YAML config");
    let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
    let routes: std::collections::HashMap<String, Route> =
        serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

    let in_route_name = format!("memory_to_{}", broker_name.to_lowercase());
    let in_route = routes.get(&in_route_name).unwrap().clone();
    let in_channel = in_route.input.channel().unwrap();

    // Fill helper: many small batches so in_channel.len() tracks pipeline progress.
    const CHUNK: usize = 128;
    async fn fill_chunks(ch: &MemoryChannel, n: usize, chunk: usize) {
        for c in generate_test_messages(n).chunks(chunk) {
            ch.fill_messages(c.to_vec()).await.unwrap();
        }
    }

    in_route
        .deploy(&in_route_name)
        .await
        .expect("deploy in_route");

    // --- Warmup (untimed): pay producer connect/metadata cost ---
    let warmup = 500.min(num_messages);
    fill_chunks(&in_channel, warmup, CHUNK).await;
    let wdl = Instant::now() + Duration::from_secs(60);
    while !in_channel.is_empty() && Instant::now() < wdl {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- Produce phase (timed: fill -> in_channel drained to broker) ---
    fill_chunks(&in_channel, num_messages, CHUNK).await;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(180);
    while !in_channel.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let secs = start.elapsed().as_secs_f64().max(1e-9);
    Route::stop(&in_route_name).await;

    // Note: up to ~concurrency*batch_size messages may still be in-flight when len()==0,
    // a <1% tail on 100k — fine for a bottleneck diagnostic.
    println!(
        "\n=== PRODUCE-ONLY [{}]: {} msgs enqueued+drained in {:.3}s => {:.0} msg/s ===\n",
        broker_name,
        num_messages,
        secs,
        num_messages as f64 / secs
    );
}

pub async fn run_chaos_pipeline_test(
    broker_name: &str,
    config_yaml: &str,
    docker_controller: DockerController,
    service_name: &str,
) {
    let service_name = service_name.to_string();
    let injector = Box::new(move || {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            docker_controller.stop_service(&service_name);
            tokio::time::sleep(Duration::from_secs(2)).await;
            docker_controller.start_service(&service_name);
        }) as BoxFuture<'static, ()>
    });

    let num_messages = if cfg!(debug_assertions) {
        PERF_TEST_MESSAGE_COUNT / 4
    } else {
        PERF_TEST_MESSAGE_COUNT / 2
    };

    // Chaos restarts the broker mid-stream. Most brokers redeliver in-flight messages on
    // reconnect, so they must reach zero loss. MQTT is the spec-sanctioned exception: QoS 1/2
    // redelivery is only guaranteed across a surviving session, so Mosquitto can drop a few
    // in-flight messages the consumer can never recover after a restart. The tests enforce the
    // configured `allowed_loss` bound rather than exact delivery; 5 is an empirical tolerance
    // that stays below systemic loss yet still catches regressions.
    let allowed_loss = if broker_name.eq_ignore_ascii_case("mqtt") {
        5
    } else {
        0
    };

    run_pipeline_test_internal(
        broker_name,
        broker_name,
        config_yaml,
        num_messages,
        false,
        Some(injector),
        allowed_loss,
        false,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline_test_internal(
    broker_name: &str,
    display_name: &str,
    config_yaml: &str,
    num_messages: usize,
    is_performance_test: bool,
    chaos_injector: Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>>,
    allowed_loss: usize,
    // Perf pipelines whose read side is a genuine at-least-once source (e.g. a MongoDB change
    // stream, which by design redelivers on any resume). Delivering *more* than `num_messages` is
    // then correct, not a bug, so the count assertion checks unique coverage (no loss) instead of
    // exact equality. Only meaningful when `is_performance_test` is true.
    at_least_once: bool,
) {
    let yaml_val: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(config_yaml).expect("Failed to parse YAML config");
    let routes_val = yaml_val.get("routes").expect("YAML must have 'routes' key");
    let routes: std::collections::HashMap<String, Route> =
        serde_yaml_ng::from_value(routes_val.clone()).expect("Failed to parse routes");

    let in_route_name = format!("memory_to_{}", broker_name.to_lowercase());
    let out_route_name = format!("{}_to_memory", broker_name.to_lowercase());

    let in_route = routes
        .get(&in_route_name)
        .unwrap_or_else(|| panic!("Route {} not found", in_route_name))
        .clone();
    let out_route = routes
        .get(&out_route_name)
        .unwrap_or_else(|| panic!("Route {} not found", out_route_name))
        .clone();

    let harness = TestHarness::new(in_route.clone(), out_route.clone(), num_messages);

    in_route
        .deploy(&in_route_name)
        .await
        .expect("Failed to deploy in_route");
    out_route
        .deploy(&out_route_name)
        .await
        .expect("Failed to deploy out_route");

    // Warm up before timing: push a probe batch through the whole path so the one-time connect /
    // consumer-group join / topic creation / partition assignment completes, otherwise the clock
    // starts cold and understates sustained throughput. Matches the direct perf tests and criterion.
    // Perf tests only — chaos/functional runs assert exact delivery counts.
    if is_performance_test {
        let warmup_count = 500.min(num_messages);
        harness
            .in_channel
            .fill_messages(generate_test_messages(warmup_count))
            .await
            .unwrap();

        let warmup_deadline = Instant::now() + Duration::from_secs(60);
        let mut warmed = 0usize;
        while warmed < warmup_count && Instant::now() < warmup_deadline {
            warmed += harness.out_channel.drain_messages().len();
            if warmed < warmup_count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // All probes received => pipeline is live; drain repeatedly until channel is empty
        // to ensure no late-arriving warmup messages pollute the test count.
        let mut empty_iterations = 0;
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < drain_deadline {
            if harness.out_channel.drain_messages().is_empty() {
                empty_iterations += 1;
                if empty_iterations >= 5 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            } else {
                empty_iterations = 0;
            }
        }
    }

    let start_time = Instant::now();

    harness.send_messages().await;

    let is_chaos_test = chaos_injector.is_some();
    if let Some(injector) = chaos_injector {
        tokio::spawn(injector());
    }

    // Wait for all messages to be processed by checking the metrics.
    let timeout = if is_performance_test || is_chaos_test {
        Duration::from_secs(210)
    } else {
        Duration::from_secs(30)
    };
    let mut received = Vec::with_capacity(num_messages);
    let mut unique_received_ids = HashSet::new();

    let wait_start = Instant::now();
    let mut last_log_time = Instant::now();
    // Stall detection: once delivery is within the allowed-loss tolerance, don't wait out
    // the full timeout if no new unique messages have arrived for a while (chaos runs may
    // legitimately drop a few messages that will never be redelivered).
    let mut last_progress_count = 0usize;
    let mut last_progress_time = Instant::now();
    while wait_start.elapsed() < timeout {
        let batch = harness.out_channel.drain_messages();
        if !batch.is_empty() {
            if !is_performance_test || at_least_once {
                for msg in &batch {
                    if let Ok(hdr) = serde_json::from_slice::<MessageNumHeader>(&msg.payload) {
                        unique_received_ids.insert(hdr.message_num);
                    }
                }
            }
            received.extend(batch);
        }

        if is_performance_test {
            // At-least-once perf pipelines finish when every message has arrived at least once
            // (duplicates keep total above num_messages); exact-once ones finish on total count.
            let done = if at_least_once {
                unique_received_ids.len() >= num_messages
            } else {
                received.len() >= num_messages
            };
            if done {
                break;
            }
        } else if unique_received_ids.len() >= num_messages {
            break;
        } else if allowed_loss > 0 {
            let current = unique_received_ids.len();
            if current > last_progress_count {
                last_progress_count = current;
                last_progress_time = Instant::now();
            } else if current >= num_messages.saturating_sub(allowed_loss)
                && last_progress_time.elapsed() > Duration::from_secs(15)
            {
                println!(
                    "[{}] delivery plateaued at {}/{} unique (within {} allowed loss), stopping wait",
                    broker_name, current, num_messages, allowed_loss
                );
                break;
            }
        }

        if last_log_time.elapsed() > Duration::from_secs(5) {
            if is_performance_test {
                println!(
                    "Progress: {}/{} messages received",
                    received.len(),
                    num_messages
                );
            } else {
                println!(
                    "Progress: {}/{} messages received (Unique: {})",
                    received.len(),
                    num_messages,
                    unique_received_ids.len()
                );
            }
            last_log_time = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Route::stop(&in_route_name).await;
    Route::stop(&out_route_name).await;

    // Drain any remaining messages that arrived during shutdown
    let batch = harness.out_channel.drain_messages();
    if !batch.is_empty() {
        if !is_performance_test || at_least_once {
            for msg in &batch {
                if let Ok(hdr) = serde_json::from_slice::<MessageNumHeader>(&msg.payload) {
                    unique_received_ids.insert(hdr.message_num);
                }
            }
        }
        received.extend(batch);
    }
    let duration = start_time.elapsed();

    if is_performance_test {
        let messages_per_second = received.len() as f64 / duration.as_secs_f64();
        println!("\n--- {} Performance Test Results ---", display_name);
        println!(
            "Processed {} messages in {:.3} seconds.",
            received.len(),
            duration.as_secs_f64()
        );
        println!("Rate: {:.2} messages/second", messages_per_second);
        println!("--------------------------------\n");

        add_performance_result(PerformanceResult {
            test_name: format!("{} Pipeline", display_name),
            write_performance: messages_per_second,
            read_performance: messages_per_second,
            single_write_performance: 0.0,
            single_read_performance: 0.0,
        });

        if at_least_once {
            // At-least-once source (e.g. MongoDB change stream): every message must arrive at
            // least once — no loss — while duplicates from stream resumes are expected, not a bug.
            let unique = unique_received_ids.len();
            assert_eq!(
                unique,
                num_messages,
                "TEST FAILED for [{}]: expected all {} unique messages, but only {} distinct arrived (total received {}). Missing messages indicate real data loss.",
                display_name,
                num_messages,
                unique,
                received.len()
            );
            let duplicates = received.len().saturating_sub(unique);
            if duplicates > 0 {
                println!(
                    "[{}] at-least-once delivery: {} unique with {} duplicate redeliveries (expected for a resumable change stream)",
                    display_name, unique, duplicates
                );
            }
        } else {
            assert_eq!(
                received.len(),
                num_messages,
                "TEST FAILED for [{}]: Expected {} messages, but found {}.",
                display_name,
                num_messages,
                received.len()
            );
        }
    } else {
        let unique = unique_received_ids.len();
        let min_required = num_messages.saturating_sub(allowed_loss);
        assert!(
            unique >= min_required,
            "TEST FAILED for [{}]: Expected at least {} unique messages (>= {} - {} allowed loss), but found {}. Total received: {}",
            broker_name,
            min_required,
            num_messages,
            allowed_loss,
            unique,
            received.len()
        );
        if unique < num_messages {
            println!(
                "[{}] tolerated message loss: {} of {} unique received ({} missing, {} allowed)",
                broker_name,
                unique,
                num_messages,
                num_messages - unique,
                allowed_loss
            );
        }
    }

    println!("Successfully verified {} route!", display_name);
}

static LOG_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

pub fn setup_logging() {
    // Using a std::sync::Once ensures this is only run once per test binary.
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        // Install the rustls CryptoProvider selected by feature flag, if none is installed yet.
        // This keeps library code provider-agnostic while keeping tests self-contained.
        #[cfg(feature = "rustls-aws-lc")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
        let _ = rustls::crypto::ring::default_provider().install_default();

        let file_appender = tracing_appender::rolling::never("logs", "integration_test.log");
        let (non_blocking_writer, guard) = tracing_appender::non_blocking(file_appender);

        *LOG_GUARD.lock().unwrap() = Some(guard);

        // Default to `info` for tests, but allow overriding with the RUST_LOG environment variable.
        // For example: `RUST_LOG=info cargo test...` or `RUST_LOG=mq_bridge=trace cargo test...`
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking_writer)
            .with_ansi(false);

        let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stdout_layer)
            .init();
    });
}

impl Drop for PerformanceSummaryPrinter {
    fn drop(&mut self) {
        let results = PERFORMANCE_RESULTS.lock().unwrap();
        if results.is_empty() {
            return;
        }

        println!("\n\n--- Consolidated Performance Test Results (msgs/sec) ---");
        println!(
            "\n\n--- Batch = {} msgs, Single = {} msgs ---",
            format_pretty(PERF_TEST_BATCH_MESSAGE_COUNT),
            format_pretty(PERF_TEST_SINGLE_MESSAGE_COUNT)
        );
        println!(
            "{:<25} | {:>15} | {:>15} | {:>15} | {:>15}",
            "Test Name", "Write (Batch)", "Read (Batch)", "Write (Single)", "Read (Single)"
        );
        println!(
            "{:-<25}-|-{:->15}-|-{:->15}-|-{:->15}-|-{:->15}",
            "", "", "", "", ""
        );
        // Group each broker's direct and "<broker> Pipeline" rows next to each other
        // so the direct-vs-pipeline gap (route-layer overhead) is easy to read off.
        let mut sorted: Vec<&PerformanceResult> = results.iter().collect();
        sorted.sort_by(|a, b| {
            let base = |n: &str| n.trim_end_matches(" Pipeline").to_string();
            base(&a.test_name)
                .cmp(&base(&b.test_name))
                .then(a.test_name.cmp(&b.test_name))
        });
        for result in sorted {
            println!(
                "{:<25} | {:>15} | {:>15} | {:>15} | {:>15}",
                result.test_name,
                format_pretty(result.write_performance),
                format_pretty(result.read_performance),
                format_pretty(result.single_write_performance),
                format_pretty(result.single_read_performance)
            );
        }
        println!("-------------------------------------------------------------------------------------------------\n");
    }
}
/// A test harness that manages the lifecycle of Docker containers for a single test.
/// It ensures that `docker-compose up` is run before the test and `docker-compose down`
/// is run after, even if the test panics.
pub async fn run_test_with_docker<F, Fut>(compose_file: &str, test_fn: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _guard = DOCKER_TEST_LOCK.lock().await;
    let docker = DockerCompose::new(compose_file);
    docker.down();
    // Give some time for docker to be ready
    docker.up();
    test_fn().await;
}

/// A test harness that manages the lifecycle of Docker containers for a single test,
/// providing a controller to manipulate services during the test.
pub async fn run_test_with_docker_controller<F, Fut>(compose_file: &str, test_fn: F)
where
    F: FnOnce(DockerController) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _guard = DOCKER_TEST_LOCK.lock().await;
    let docker = DockerCompose::new(compose_file);
    docker.down();
    // Give some time for docker to be ready
    docker.up();
    test_fn(docker.controller()).await;
}

/// A generic test runner for direct performance tests.
///
/// This function abstracts the common pattern of:
/// 1. Creating a publisher and a consumer.
/// 2. Running batch write/read performance tests.
/// 3. Running single write/read performance tests.
///
/// It takes async closures to create the specific publisher and consumer,
/// making it reusable across different backend implementations.
pub async fn run_direct_perf_test<P, C, FutP, FutC, Pub, Con>(
    test_name: &str,
    create_publisher: P,
    create_consumer: C,
) -> PerformanceResult
where
    Pub: MessagePublisher + 'static,
    Con: MessageConsumer + 'static,
    P: Fn() -> FutP,
    C: Fn() -> FutC,
    FutP: std::future::Future<Output = Arc<Pub>>,
    FutC: std::future::Future<Output = Arc<tokio::sync::Mutex<Con>>>,
{
    let publisher = create_publisher().await;
    let consumer = create_consumer().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let single_write_perf = measure_single_write_performance(
        &format!("{} (Single)", test_name),
        publisher.clone(),
        PERF_TEST_SINGLE_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY,
    )
    .await
    .as_secs_f64();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let single_read_perf = measure_single_read_performance(
        &format!("{} (Single)", test_name),
        consumer.clone(),
        PERF_TEST_SINGLE_MESSAGE_COUNT,
    )
    .await
    .as_secs_f64();
    tokio::time::sleep(Duration::from_secs(2)).await;

    tokio::time::sleep(Duration::from_millis(200)).await; // Allow consumer setup

    let write_perf = measure_write_performance(
        &format!("{} (Batch)", test_name),
        publisher.clone(),
        PERF_TEST_BATCH_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY,
    )
    .await
    .as_secs_f64();
    tokio::time::sleep(Duration::from_secs(2)).await;
    // Add a delay to ensure messages are queryable, especially for Kafka.
    let read_perf = measure_read_performance(
        &format!("{} (Batch)", test_name),
        consumer.clone(),
        PERF_TEST_BATCH_MESSAGE_COUNT,
    )
    .await
    .as_secs_f64();

    drop(consumer);
    drop(publisher);
    tokio::time::sleep(Duration::from_millis(200)).await; // Allow consumer setup

    PerformanceResult {
        test_name: format!("{} Direct", test_name),
        write_performance: PERF_TEST_BATCH_MESSAGE_COUNT as f64 / write_perf,
        read_performance: PERF_TEST_BATCH_MESSAGE_COUNT as f64 / read_perf,
        single_write_performance: PERF_TEST_SINGLE_MESSAGE_COUNT as f64 / single_write_perf,
        single_read_performance: PERF_TEST_SINGLE_MESSAGE_COUNT as f64 / single_read_perf,
    }
}

static STATIC_PAYLOAD: Lazy<Vec<u8>> =
    Lazy::new(|| serde_json::to_vec(&json!({ "perf_test": true, "static": true })).unwrap());

pub fn generate_message(id: u128) -> CanonicalMessage {
    CanonicalMessage::new(STATIC_PAYLOAD.clone(), Some(id))
}

/// Verifies that multiple subscribers receive the same message (Broadcast/Pub-Sub logic).
pub async fn verify_subscriber_logic(
    publisher: Arc<dyn MessagePublisher>,
    sub1: Arc<AsyncMutex<dyn MessageConsumer>>,
    sub2: Arc<AsyncMutex<dyn MessageConsumer>>,
) {
    let payload = format!("broadcast-{}", fast_uuid_v7::gen_id());
    publisher.send(payload.as_str().into()).await.unwrap();

    // Backoff before retry
    let res1 = tokio::time::timeout(Duration::from_secs(15), async {
        let mut guard = sub1.lock().await;
        guard.receive().await
    })
    .await
    .expect("sub1 timeout")
    .unwrap();

    let res2 = tokio::time::timeout(Duration::from_secs(15), async {
        let mut guard = sub2.lock().await;
        guard.receive().await
    })
    .await
    .expect("sub2 timeout")
    .unwrap();

    assert_eq!(res1.message.get_payload_str(), payload);
    assert_eq!(res2.message.get_payload_str(), payload);
}

/// Measure the performance of writing messages to a publisher.
///
/// This test creates a publisher and consumer with a bounded channel.
/// It then spawns a number of tasks to write messages to the publisher
/// concurrently. Each task will write a batch of messages to
/// the publisher, retrying if any messages fail. The test times how long
/// it takes to write all the messages to the publisher.
///
/// The number of messages to write, the concurrency level and the batch
/// size are all configurable. The test will retry sending a batch up
/// to `PERF_SEND_MAX_RETRIES` times if any messages fail.
///
/// The test will return how long it took to write all the messages to the
/// publisher. If the count of messages written is not equal to the
/// expected count, an error will be logged.
///
/// `num_messages`: The number of messages to write to the publisher.
///
/// `concurrency`: The number of tasks to spawn concurrently to write
/// messages to the publisher.
///
/// The batch size is fixed at 128 messages per batch.
///
pub async fn measure_write_performance(
    _name: &str,
    publisher: Arc<dyn MessagePublisher>,
    num_messages: usize,
    concurrency: usize,
) -> Duration {
    // write performance test (Batch) for {}", _name);
    let batch_size = 128; // Define a reasonable batch size
    let (tx, rx): (
        Sender<Vec<CanonicalMessage>>,
        Receiver<Vec<CanonicalMessage>>,
    ) = bounded(concurrency * 4);

    let final_count = Arc::new(AtomicUsize::new(0));

    // Spawn multiple generators to ensure we don't bottleneck on message creation.
    let generator_count = (concurrency / 10).clamp(1, 8);
    for i in 0..generator_count {
        let tx = tx.clone();
        let count = num_messages / generator_count
            + if i < num_messages % generator_count {
                1
            } else {
                0
            };
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(batch_size);
            for _ in 0..count {
                batch.push(generate_message(fast_uuid_v7::gen_id()));
                if batch.len() >= batch_size {
                    if tx.send(batch).await.is_err() {
                        eprintln!("Error sending to channel");
                        return;
                    }
                    batch = Vec::with_capacity(batch_size);
                }
            }
            if !batch.is_empty() {
                let _ = tx.send(batch).await;
            }
        });
    }
    drop(tx); // Close the original sender so the channel closes when all generators are done.

    let start_time = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();

    for _ in 0..concurrency {
        let rx_clone = rx.clone();
        let publisher_clone = publisher.clone();
        let final_count_clone = Arc::clone(&final_count);

        tasks.spawn(async move {
            while let Ok(batch) = rx_clone.recv().await {
                // Retry sending the batch if some messages fail.
                let mut messages_to_send = batch;
                let mut current_batch_size = messages_to_send.len();
                let mut retry_count = 0;
                loop {
                    match publisher_clone
                        .send_batch(std::mem::take(&mut messages_to_send))
                        .await
                    {
                        Ok(SentBatch::Ack) => {
                            final_count_clone.fetch_add(
                                current_batch_size,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            break; // All sent successfully
                        }
                        Ok(SentBatch::Partial {
                            responses: _,
                            failed,
                        }) => {
                            let success_count = current_batch_size - failed.len();
                            if success_count > 0 {
                                final_count_clone
                                    .fetch_add(success_count, std::sync::atomic::Ordering::Relaxed);
                            }

                            if failed.is_empty() {
                                break; // All sent successfully
                            } else {
                                let (retryable, non_retryable): (Vec<_>, Vec<_>) = failed
                                    .into_iter()
                                    .partition(|(_, e)| matches!(e, PublisherError::Retryable(_)));

                                if !non_retryable.is_empty() {
                                    final_count_clone.fetch_add(
                                        non_retryable.len(),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }

                                if retryable.is_empty() {
                                    break;
                                }
                                retry_count += 1;
                                if retry_count >= PERF_SEND_MAX_RETRIES {
                                    eprintln!(
                                        "Max retries reached, giving up on {} messages",
                                        retryable.len()
                                    );
                                    break;
                                }
                                eprintln!("Retrying: {}", retryable.len());
                                messages_to_send =
                                    retryable.into_iter().map(|(msg, _)| msg).collect();
                                current_batch_size = messages_to_send.len();
                            }
                        }
                        Err(e) => {
                            retry_count += 1;
                            if retry_count >= PERF_SEND_MAX_RETRIES {
                                eprintln!(
                                    "Max retries reached, giving up on batch: {}",
                                    format_error_chain(&e)
                                );
                                return;
                            }
                        }
                    };
                    tokio::time::sleep(Duration::from_millis(10)).await; // Backoff before retry
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}
    publisher.flush().await.unwrap();

    let count = final_count.load(std::sync::atomic::Ordering::Relaxed);
    if count != num_messages {
        eprintln!(
            "measure_write_performance: Expected {} messages, but got {}",
            num_messages, count
        );
    }
    debug_assert_eq!(count, num_messages);
    start_time.elapsed()
}

/// A mock consumer that does nothing, useful for testing publishers in isolation.
#[derive(Clone)]
pub struct MockConsumer;

#[async_trait::async_trait]
impl MessageConsumer for MockConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        // This consumer will block forever, which is fine for tests that only need a publisher.
        // It prevents the route from exiting immediately.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        unreachable!();
    }
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        // This consumer will block forever, which is fine for tests that only need a publisher.
        // It prevents the route from exiting immediately.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        unreachable!();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Formats a number with underscores as thousand separators.
/// Handles both integers and floating-point numbers.
pub fn format_pretty<N: Display>(num: N) -> String {
    let s = num.to_string();
    let mut parts = s.splitn(2, '.');
    let integer_part = parts.next().unwrap_or("");
    let fractional_part = parts.next();

    let mut formatted_integer = String::with_capacity(integer_part.len() + integer_part.len() / 3);
    for (count, ch) in integer_part.chars().rev().enumerate() {
        if count > 0 && count % 3 == 0 {
            formatted_integer.push('_');
        }
        formatted_integer.push(ch);
    }

    let formatted_integer = formatted_integer.chars().rev().collect::<String>();

    match fractional_part {
        Some(frac) => {
            let truncated_frac = if frac.len() > 2 { &frac[..2] } else { frac };
            format!("{}.{}", formatted_integer, truncated_frac)
        }
        None => formatted_integer,
    }
}

pub async fn measure_read_performance(
    _name: &str,
    consumer: Arc<tokio::sync::Mutex<dyn MessageConsumer>>,
    num_messages: usize,
) -> Duration {
    measure_read_performance_with_timeout(_name, consumer, num_messages, Duration::from_secs(20))
        .await
}

pub async fn measure_read_performance_with_timeout(
    _name: &str,
    consumer: Arc<tokio::sync::Mutex<dyn MessageConsumer>>,
    num_messages: usize,
    receive_timeout: Duration,
) -> Duration {
    // println!("Starting read performance test (Batch) for {}", _name);
    let start_time = Instant::now();
    let mut final_count = 0;
    let batch_size = 128; // A reasonable batch size for single-threaded reading.
    let commit_semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_COMMITS));

    let consumer_clone = consumer.clone();

    loop {
        if final_count >= num_messages {
            break;
        }

        let missing = std::cmp::min(batch_size, num_messages - final_count);

        let mut consumer_guard = consumer_clone.lock().await;
        let receive_future = consumer_guard.receive_batch(missing);

        match tokio::time::timeout(receive_timeout, receive_future).await {
            Ok(Ok(batch)) if !batch.messages.is_empty() => {
                final_count += batch.messages.len();
                let commit = batch.commit;
                let permit = commit_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Semaphore closed");
                tokio::spawn(async move {
                    let _ = commit(vec![MessageDisposition::Ack; batch.messages.len()]).await;
                    drop(permit);
                });
            }
            Ok(Err(e)) => {
                eprintln!("Error receiving message: {}. Stopping read.", e);
                break;
            }
            Err(_) => {
                eprintln!("Timeout waiting for messages. Stopping read.");
                break;
            }
            _ => {
                // Empty batch, assume we are done.
                break;
            }
        }
    }

    if final_count != num_messages {
        eprintln!(
            "measure_read_performance: Expected {} messages, but got {}",
            num_messages, final_count
        );
    }
    debug_assert_eq!(final_count, num_messages);
    start_time.elapsed()
}

pub async fn measure_single_write_performance(
    _name: &str,
    publisher: Arc<dyn MessagePublisher>,
    num_messages: usize,
    concurrency: usize,
) -> Duration {
    // println!("Starting single write performance test for {}", _name);
    let (tx, rx): (Sender<CanonicalMessage>, Receiver<CanonicalMessage>) = bounded(concurrency * 2);

    let final_count = Arc::new(AtomicUsize::new(0));

    let generator_count = (concurrency / 10).clamp(1, 8);
    for i in 0..generator_count {
        let tx = tx.clone();
        let count = num_messages / generator_count
            + if i < num_messages % generator_count {
                1
            } else {
                0
            };
        tokio::spawn(async move {
            for _ in 0..count {
                if tx
                    .send(generate_message(fast_uuid_v7::gen_id()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    drop(tx);

    let start_time = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();

    for _ in 0..concurrency {
        let rx_clone = rx.clone();
        let publisher_clone = publisher.clone();
        let final_count_clone = Arc::clone(&final_count);

        tasks.spawn(async move {
            while let Ok(message) = rx_clone.recv().await {
                let mut retry_count = 0;
                loop {
                    match publisher_clone.send(message.clone()).await {
                        Ok(_) => {
                            final_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        Err(e) => {
                            retry_count += 1;
                            if retry_count >= PERF_SEND_MAX_RETRIES {
                                eprintln!(
                                    "Max retries reached, giving up on message: {}",
                                    format_error_chain(&e)
                                );
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            // Backoff
                        }
                    }
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}
    publisher.flush().await.unwrap();

    let count = final_count.load(std::sync::atomic::Ordering::Relaxed);
    if count != num_messages {
        eprintln!(
            "measure_single_write_performance: Expected {} messages, but got {}",
            num_messages, count
        );
    }
    debug_assert_eq!(count, num_messages);
    start_time.elapsed()
}

pub async fn measure_single_read_performance(
    _name: &str,
    consumer: Arc<tokio::sync::Mutex<dyn MessageConsumer>>,
    num_messages: usize,
) -> Duration {
    // println!("Starting single read performance test for {}", _name);
    let start_time = Instant::now();
    let mut final_count = 0;
    let commit_semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_COMMITS));
    loop {
        if final_count == num_messages {
            break;
        }
        let mut consumer_guard = consumer.lock().await;
        let receive_future = consumer_guard.receive();
        match tokio::time::timeout(Duration::from_secs(20), receive_future).await {
            Ok(Ok(Received {
                message: _msg,
                commit,
            })) => {
                final_count += 1;
                let permit = commit_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Semaphore closed");
                tokio::spawn(async move {
                    let _ = commit(MessageDisposition::Ack).await;
                    drop(permit);
                });
            }
            Err(_) => {
                eprintln!("Timeout waiting for single message. Stopping read.");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("Failed to receive message: {}. Stopping read.", e);
                break;
            }
        }
    }

    if final_count != num_messages {
        eprintln!(
            "measure_single_read_performance: Expected {} messages, but got {}",
            num_messages, final_count
        );
    }
    debug_assert_eq!(final_count, num_messages);
    start_time.elapsed()
}

pub fn should_run_benchmark(backend_name: &str) -> bool {
    // If the MQB_TEST_BACKEND env var is set, prefer it as a filter source.
    if let Ok(env_filters) = std::env::var("MQB_TEST_BACKEND") {
        let env_filters: Vec<&str> = env_filters
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !env_filters.is_empty() {
            return env_filters
                .iter()
                .any(|f| backend_name.contains(f) || f.contains(backend_name));
        }
    }

    let mut filters = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // Only skip values for flags that are known to take values
        if arg == "--output-format"
            || arg == "--baseline"
            || arg == "--save-baseline"
            || arg == "--load-baseline"
            || arg == "--profile-time"
            || arg == "--sample-size"
            || arg == "--measurement-time"
            || arg == "--warm-up-time"
            || arg == "--color"
            || arg == "-j"
            || arg == "--jobs"
            || arg == "--encoding"
        {
            args.next();
            continue;
        }
        if arg.starts_with("--") {
            continue;
        }
        if !arg.starts_with('-') {
            filters.push(arg);
        }
    }
    if filters.is_empty() {
        return true;
    }
    filters
        .iter()
        .any(|arg| backend_name.contains(arg) || arg.contains(backend_name))
}

pub fn print_benchmark_results(
    results: &std::collections::HashMap<String, PerformanceResult>,
    msg_count: usize,
) {
    if !results.is_empty() {
        println!("\n\n--- Consolidated Performance Test Results (msgs/sec) ---");
        println!(
            "\n\n--- Batch = {} msgs, Single = {} msgs ---",
            format_pretty(msg_count),
            format_pretty(msg_count)
        );
        println!(
            "{:<25} | {:>15} | {:>15} | {:>15} | {:>15}",
            "Test Name", "Write (Batch)", "Read (Batch)", "Write (Single)", "Read (Single)"
        );
        println!(
            "{:-<25}-|-{:->15}-|-{:->15}-|-{:->15}-|-{:->15}",
            "", "", "", "", ""
        );
        let mut sorted_results: Vec<_> = results.iter().collect();
        sorted_results.sort_by_key(|(name, _)| *name);
        for (name, stats) in sorted_results {
            println!(
                "{:<25} | {:>15} | {:>15} | {:>15} | {:>15}",
                format!("{} Direct", name),
                format_pretty(stats.write_performance),
                format_pretty(stats.read_performance),
                format_pretty(stats.single_write_performance),
                format_pretty(stats.single_read_performance)
            );
        }
        println!("---------------------------------------------------------------------------------------\n");
    }
    print_incomplete_benchmarks();
}

/// Name the sub-benchmarks that produced no result, next to the table itself.
///
/// The table is the artifact people read and paste elsewhere, and a backend that *blocks*
/// rather than runs slowly leaves no row in it — indistinguishable from one that was never
/// selected. A slow backend always reports a row, so a missing one means it hung.
pub fn print_incomplete_benchmarks() {
    let timed_out = BENCH_TIMED_OUT_SUBBENCHES.blocking_lock();
    let aborted = BENCH_ABORTED_FEATURES.blocking_lock();
    if timed_out.is_empty() && aborted.is_empty() {
        return;
    }
    println!("--- INCOMPLETE: the following produced NO results and are missing above ---");
    let mut subbenches: Vec<&String> = timed_out.iter().collect();
    subbenches.sort();
    for key in subbenches {
        println!("  TIMED OUT  {key}");
    }
    let mut features: Vec<&String> = aborted.iter().collect();
    features.sort();
    for feature in features {
        println!("  ABORTED    {feature} (time budget spent; remaining sub-benchmarks skipped)");
    }
    println!("A timeout means the endpoint blocked, not that it was slow.");
    println!(
        "---------------------------------------------------------------------------------------\n"
    );
}

/// Panic when a sub-benchmark timed out, so a deadlocked endpoint cannot exit 0.
///
/// Only timeouts fail the run. An exhausted feature budget is not enough on its own: slow but
/// successful sub-benchmarks can spend it while still reporting their rows, and failing on that
/// would turn a loaded machine into a red build.
pub fn fail_on_incomplete_benchmarks() {
    let timed_out = BENCH_TIMED_OUT_SUBBENCHES.blocking_lock();
    if timed_out.is_empty() {
        return;
    }
    let mut names: Vec<&str> = timed_out.iter().map(String::as_str).collect();
    names.sort_unstable();
    panic!(
        "benchmark incomplete: {} sub-benchmark(s) blocked and produced no results: {}",
        names.len(),
        names.join(", ")
    );
}

#[macro_export]
macro_rules! run_benchmarks {
    ($name:literal, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr, $sleep_duration:expr) => {
        $group.bench_function(concat!($name, "_single_write"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
                let sub = concat!($name, "_single_write");
                if $crate::test_utils::should_skip_subbench($name, sub).await {
                    return std::time::Duration::from_nanos(1);
                }
                let inner = async move {
                    let mut total = std::time::Duration::ZERO;
                    // Create consumer first to support brokerless protocols like ZeroMQ
                    let consumer = backend::create_consumer().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let publisher = backend::create_publisher().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    for _ in 0..iters {
                        let duration = $crate::test_utils::measure_single_write_performance(
                            concat!($name, "_single_write"),
                            std::sync::Arc::clone(&publisher),
                            $msg_count,
                            $concurrency,
                        )
                        .await;
                        total += duration;
                        tokio::time::sleep($sleep_duration).await;
                        $crate::test_utils::measure_read_performance_with_timeout(
                            "cleanup",
                            std::sync::Arc::clone(&consumer),
                            $msg_count,
                            $crate::test_utils::PERF_CLEANUP_READ_TIMEOUT,
                        )
                        .await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    let msgs_per_sec = (iters as f64 * $msg_count as f64) / total.as_secs_f64();
                    {
                        let mut results = $results.lock().await;
                        let stats = results.entry($name.to_string()).or_default();
                        stats.single_write_performance = msgs_per_sec;
                    }
                    println!(
                        "\n{} single_write: {} iters, total time {:?}, {:.2} msgs/sec",
                        $name, iters, total, msgs_per_sec
                    );
                    total
                };
                $crate::test_utils::with_subbench_timeout($name, sub, inner).await
            })
        });

        $group.bench_function(concat!($name, "_single_read"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
                let sub = concat!($name, "_single_read");
                if $crate::test_utils::should_skip_subbench($name, sub).await {
                    return std::time::Duration::from_nanos(1);
                }
                let inner = async move {
                    let mut total = std::time::Duration::ZERO;
                    let consumer = backend::create_consumer().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let publisher = backend::create_publisher().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    for _ in 0..iters {
                        $crate::test_utils::measure_write_performance(
                            "setup_fill",
                            std::sync::Arc::clone(&publisher),
                            $msg_count,
                            $concurrency,
                        )
                        .await;
                        tokio::time::sleep($sleep_duration).await;

                        let duration = $crate::test_utils::measure_single_read_performance(
                            concat!($name, "_single_read"),
                            std::sync::Arc::clone(&consumer),
                            $msg_count,
                        )
                        .await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        total += duration;
                    }
                    let msgs_per_sec = (iters as f64 * $msg_count as f64) / total.as_secs_f64();
                    {
                        let mut results = $results.lock().await;
                        let stats = results.entry($name.to_string()).or_default();
                        stats.single_read_performance = msgs_per_sec;
                    }
                    println!(
                        "\n{} single_read: {} iters, total time {:?}, {:.2} msgs/sec",
                        $name, iters, total, msgs_per_sec
                    );
                    total
                };
                $crate::test_utils::with_subbench_timeout($name, sub, inner).await
            })
        });

        $group.bench_function(concat!($name, "_batch_write"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
                let sub = concat!($name, "_batch_write");
                if $crate::test_utils::should_skip_subbench($name, sub).await {
                    return std::time::Duration::from_nanos(1);
                }
                let inner = async move {
                    let mut total = std::time::Duration::ZERO;
                    let consumer = backend::create_consumer().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let publisher = backend::create_publisher().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    for _ in 0..iters {
                        let duration = $crate::test_utils::measure_write_performance(
                            concat!($name, "_batch_write"),
                            std::sync::Arc::clone(&publisher),
                            $msg_count,
                            $concurrency,
                        )
                        .await;
                        tokio::time::sleep($sleep_duration).await;
                        total += duration;

                        $crate::test_utils::measure_read_performance_with_timeout(
                            "cleanup",
                            std::sync::Arc::clone(&consumer),
                            $msg_count,
                            $crate::test_utils::PERF_CLEANUP_READ_TIMEOUT,
                        )
                        .await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    let msgs_per_sec = (iters as f64 * $msg_count as f64) / total.as_secs_f64();
                    {
                        let mut results = $results.lock().await;
                        let stats = results.entry($name.to_string()).or_default();
                        stats.write_performance = msgs_per_sec;
                    }
                    println!(
                        "\n{} batch_write: {} iters, total time {:?}, {:.2} msgs/sec",
                        $name, iters, total, msgs_per_sec
                    );
                    total
                };
                $crate::test_utils::with_subbench_timeout($name, sub, inner).await
            })
        });

        $group.bench_function(concat!($name, "_batch_read"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
                let sub = concat!($name, "_batch_read");
                if $crate::test_utils::should_skip_subbench($name, sub).await {
                    return std::time::Duration::from_nanos(1);
                }
                let inner = async move {
                    let mut total = std::time::Duration::ZERO;
                    let consumer = backend::create_consumer().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let publisher = backend::create_publisher().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    for _ in 0..iters {
                        $crate::test_utils::measure_write_performance(
                            "setup_fill",
                            std::sync::Arc::clone(&publisher),
                            $msg_count,
                            $concurrency,
                        )
                        .await;
                        tokio::time::sleep($sleep_duration).await;

                        let duration = $crate::test_utils::measure_read_performance(
                            concat!($name, "_batch_read"),
                            std::sync::Arc::clone(&consumer),
                            $msg_count,
                        )
                        .await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        total += duration;
                    }
                    let msgs_per_sec = (iters as f64 * $msg_count as f64) / total.as_secs_f64();
                    {
                        let mut results = $results.lock().await;
                        let stats = results.entry($name.to_string()).or_default();
                        stats.read_performance = msgs_per_sec;
                    }
                    println!(
                        "\n{} batch_read: {} iters, total time {:?}, {:.2} msgs/sec",
                        $name, iters, total, msgs_per_sec
                    );
                    total
                };
                $crate::test_utils::with_subbench_timeout($name, sub, inner).await
            })
        });
    };
}

#[macro_export]
macro_rules! bench_backend {
    // Matches a backend that requires Docker but has no specific feature gate.
    ("", $name:literal, $compose_file:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr, $sleep_duration:expr) => {
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;

            // Start the Docker environment for this backend.
            // The DockerCompose struct handles `docker-compose up` on creation and `down` on drop.
            let _docker = $crate::test_utils::DockerCompose::new($compose_file);
            _docker.down();
            _docker.up();

            $crate::run_benchmarks!(
                $name,
                $group,
                $rt,
                $results,
                $msg_count,
                $concurrency,
                $sleep_duration
            );
            _docker.down();
        }
    };
    ($feature:literal, $name:literal, $compose_file:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr, $sleep_duration:expr) => {
        #[cfg(feature = $feature)]
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;

            // Start the Docker environment for this backend.
            // The DockerCompose struct handles `docker-compose up` on creation and `down` on drop.
            let _docker = $crate::test_utils::DockerCompose::new($compose_file);
            _docker.down();
            _docker.up();

            $crate::run_benchmarks!(
                $name,
                $group,
                $rt,
                $results,
                $msg_count,
                $concurrency,
                $sleep_duration
            );
            _docker.down();
        }
    };
    ($feature:literal, $name:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr, $sleep_duration:expr) => {
        #[cfg(feature = $feature)]
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;
            // No docker setup
            $crate::run_benchmarks!(
                $name,
                $group,
                $rt,
                $results,
                $msg_count,
                $concurrency,
                $sleep_duration
            );
        }
    };
    ($name:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr, $sleep_duration:expr) => {
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;
            // No docker setup, no feature gate
            $crate::run_benchmarks!(
                $name,
                $group,
                $rt,
                $results,
                $msg_count,
                $concurrency,
                $sleep_duration
            );
        }
    };
}

/// Helper to verify that a route processes messages concurrently.
/// It deploys a route with concurrency 2, sends two messages that sleep 500ms each via the `sender` endpoint,
/// and asserts that the total time is significantly less than 1000ms.
pub async fn run_concurrency_test(
    input: crate::models::Endpoint,
    output: crate::models::Endpoint,
    sender: crate::models::Endpoint,
) {
    use crate::traits::Handled;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let work_duration = Duration::from_millis(500);
    let unique_id = fast_uuid_v7::gen_id().to_string();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Handler that simulates enough work for the concurrency timing check to be stable.
    let handler = move |_msg: crate::CanonicalMessage| {
        let c = counter_clone.clone();
        async move {
            tokio::time::sleep(work_duration).await;
            c.fetch_add(1, Ordering::SeqCst);
            Ok(Handled::Ack)
        }
    };

    let route = crate::models::Route::new(input, output)
        .with_handler(handler)
        .with_concurrency(2)
        .with_batch_size(1);

    let route_name = format!("con_test_{}", unique_id);
    route
        .deploy(&route_name)
        .await
        .expect("Failed to deploy route");

    let publisher = sender
        .create_publisher(&route_name)
        .await
        .expect("Failed to create publisher");
    let start = std::time::Instant::now();

    // Send messages concurrently. For request-reply endpoints like HTTP,
    // sequential awaits in the test driver would prevent concurrent processing in the route.
    let (res1, res2) = tokio::join!(publisher.send("msg1".into()), publisher.send("msg2".into()));
    res1.expect("Send 1 failed");
    res2.expect("Send 2 failed");

    while counter.load(Ordering::SeqCst) < 2 {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("Timeout waiting for concurrent messages");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elapsed = start.elapsed();
    crate::models::Route::stop(&route_name).await;

    assert!(
        elapsed < work_duration + Duration::from_millis(300),
        "Execution was not concurrent: took {:?}",
        elapsed
    );
    assert!(
        elapsed >= work_duration,
        "Execution too fast: {:?}",
        elapsed
    );
}

/// Entry points that let the Criterion benches in `benches/` reach crate-internal hot
/// paths. Not part of the public API — it lives here so one feature gate covers every
/// out-of-crate dev harness, and can be split back out if it grows.
#[doc(hidden)]
pub mod bench {
    use crate::models::FileFormat;
    use crate::CanonicalMessage;

    /// Decodes a CSV corpus the way the file source does: the first record establishes the
    /// column header, every later record becomes one message.
    pub fn csv_records_to_json(records: &[&[u8]]) -> Vec<CanonicalMessage> {
        let mut header = None;
        records
            .iter()
            .filter_map(|record| {
                crate::endpoints::file::parse_message(record, &FileFormat::Csv, &mut header)
            })
            .collect()
    }

    /// Decodes a CSV corpus in reader-sized batches, the way the file source does. This is
    /// the path that splits a batch across cores, so it is the one that shows the cost of
    /// the split itself.
    pub fn csv_batch_decode(records: &[&[u8]], batch_size: usize) -> usize {
        let mut header = None;
        let mut buf: Vec<u8> = Vec::new();
        let mut decoded = 0;
        for chunk in records.chunks(batch_size) {
            buf.clear();
            let mut spans = Vec::with_capacity(chunk.len());
            for (i, record) in chunk.iter().enumerate() {
                let start = buf.len();
                buf.extend_from_slice(record);
                spans.push((start, buf.len(), i as u64));
            }
            decoded += crate::endpoints::file::decode_records(
                &mut buf,
                &spans,
                &FileFormat::Csv,
                &mut header,
                false,
            )
            .len();
        }
        decoded
    }

    /// Evaluates one `filter` expression over a corpus, returning how many messages it kept.
    #[cfg(feature = "filter")]
    pub fn filter_matches(
        expression: &str,
        messages: &[CanonicalMessage],
    ) -> anyhow::Result<usize> {
        let filter = crate::middleware::filter::CompiledFilter::new(expression)?;
        let mut kept = 0;
        for message in messages {
            if filter.matches(message)? {
                kept += 1;
            }
        }
        Ok(kept)
    }

    /// Applies one `transform` configuration to a corpus, returning the total output size so
    /// nothing the middleware produced can be optimized away.
    pub fn transform_messages(
        config: &crate::models::TransformMiddleware,
        messages: &[CanonicalMessage],
    ) -> anyhow::Result<usize> {
        crate::middleware::transform::bench_apply(config, messages)
    }
}

#[cfg(test)]
mod tests {
    use super::{should_skip_subbench, with_subbench_timeout_configured};
    use std::time::Duration;

    #[tokio::test]
    async fn subbench_timeout_marks_subbench_for_skip() {
        let feature = format!("timeout-feature-{}", fast_uuid_v7::gen_id());
        let subbench = format!("timeout-subbench-{}", fast_uuid_v7::gen_id());

        let measured = with_subbench_timeout_configured(
            &feature,
            &subbench,
            Duration::from_millis(5),
            Duration::from_secs(1),
            // Never ready, so the timeout branch is the only possible outcome. A
            // sleep here would race the timeout on coarse-grained OS timers.
            std::future::pending::<Duration>(),
        )
        .await;

        assert_eq!(measured, Duration::from_millis(5));
        assert!(should_skip_subbench(&feature, &subbench).await);
    }

    #[tokio::test]
    async fn feature_budget_marks_remaining_subbenches_for_skip() {
        let feature = format!("budget-feature-{}", fast_uuid_v7::gen_id());
        let first = format!("budget-subbench-a-{}", fast_uuid_v7::gen_id());
        let second = format!("budget-subbench-b-{}", fast_uuid_v7::gen_id());

        let measured = with_subbench_timeout_configured(
            &feature,
            &first,
            Duration::from_secs(1),
            Duration::from_millis(10),
            async { Duration::from_millis(15) },
        )
        .await;

        assert_eq!(measured, Duration::from_millis(15));
        assert!(should_skip_subbench(&feature, &first).await);
        assert!(should_skip_subbench(&feature, &second).await);
    }
}
