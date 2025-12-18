#![allow(dead_code)] // This module contains helpers used by various integration tests.
use async_channel::{bounded, Receiver, Sender};
use chrono;
use hot_queue::traits::{BatchCommitFunc, MessagePublisher};
use hot_queue::traits::{CommitFunc, MessageConsumer};
use hot_queue::{CanonicalMessage, Route};
use once_cell::sync::Lazy;
use serde_json::json;
use std::any::Any;
use std::fmt::Display;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::tempdir;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use hot_queue::endpoints::memory::MemoryChannel;

/// A struct to hold the performance results for a single test run.
#[derive(Debug, Clone)]
pub struct PerformanceResult {
    pub test_name: String,
    pub write_performance: f64,
    pub read_performance: f64,
}

/// A global, thread-safe collector for performance results.
static PERFORMANCE_RESULTS: Lazy<Mutex<Vec<PerformanceResult>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

/// Adds a performance result to the global collector.
pub fn add_performance_result(result: PerformanceResult) {
    PERFORMANCE_RESULTS.lock().unwrap().push(result);
}

/// A mock struct whose Drop implementation will print the summary table.
pub struct PerformanceSummaryPrinter;

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
        let status = Command::new("docker-compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("up")
            .arg("-d")
            .arg("--wait")
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to start docker-compose");

        assert!(status.success(), "docker-compose up --wait failed");
        println!("Services from {} should be up.", self.compose_file);
    }

    pub fn down(&self) {
        println!(
            "Stopping docker-compose services from {}...",
            self.compose_file
        );
        Command::new("docker-compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("down")
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to stop docker-compose");
        println!("Services from {} stopped.", self.compose_file);
    }
}

impl Drop for DockerCompose {
    fn drop(&mut self) {
        self.down();
    }
}

pub fn generate_test_messages(num_messages: usize) -> Vec<CanonicalMessage> {
    let mut messages = Vec::new();
    messages.reserve(num_messages);

    for i in 0..num_messages {
        let payload = json!({ "message_num": i, "test_id": "integration" });
        let msg = CanonicalMessage::from_json(payload.clone()).unwrap();
        messages.push(msg);
    }
    messages
}

/// A test harness to simplify integration testing of bridge pipelines.
struct TestHarness {
    _temp_dir: tempfile::TempDir,
    in_channel: MemoryChannel,
    out_channel: MemoryChannel,
    messages_to_send: Vec<CanonicalMessage>,
}

impl TestHarness {
    /// Creates a new TestHarness for a given broker and configuration.
    fn new(in_route: Route, out_route: Route, num_messages: usize) -> Self {
        let temp_dir = tempdir().unwrap();
        let messages_to_send = generate_test_messages(num_messages);

        // The input to the system is the input of the `memory_to_*` route.
        let in_channel = in_route.input.channel().unwrap();

        // The final output from the system is the output of the `*_to_memory` route.
        let out_channel = out_route.output.channel().unwrap();

        Self {
            _temp_dir: temp_dir,
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
    run_pipeline_test_internal(broker_name, config_yaml, 5, false).await;
}

pub async fn run_performance_pipeline_test(
    broker_name: &str,
    config_yaml: &str,
    num_messages: usize,
) {
    run_pipeline_test_internal(broker_name, config_yaml, num_messages, true).await;
}

async fn run_pipeline_test_internal(
    _broker_name: &str,
    _config_yaml: &str,
    _num_messages: usize,
    _is_performance_test: bool,
) {
    /*
    let app_config: AppConfig =
        serde_yaml_ng::from_str(config_yaml).expect("Failed to parse YAML config");
    let mut harness = TestHarness::new(app_config, num_messages);

    let bridge_handle = harness.bridge.run();
    let start_time = std::time::Instant::now();

    harness.send_messages().await;

    // Wait for all messages to be processed by checking the metrics.
    let timeout = if is_performance_test {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(30)
    };
    let wait_start = Instant::now();
    while wait_start.elapsed() < timeout {
        let received_count = harness.out_channel.len();
        if received_count >= num_messages {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if harness.shutdown_tx.send(()).is_err() {
        println!("WARN: Could not send shutdown signal, bridge may have already stopped.");
    }

    let received = harness.out_channel.drain_messages()
    let duration = start_time.elapsed();

    if is_performance_test {
        let messages_per_second = num_messages as f64 / duration.as_secs_f64();
        println!("\n--- {} Performance Test Results ---", broker_name);
        println!(
            "Processed {} messages in {:.3} seconds.",
            received.len(),
            duration.as_secs_f64()
        );
        println!("Rate: {:.2} messages/second", messages_per_second);
        println!("--------------------------------\n");
    }

    assert_eq!(
        received.len(),
        num_messages,
        "TEST FAILED for [{}]: Expected {} messages, but found {}.",
        broker_name,
        num_messages,
        received.len()
    );
    println!("Successfully verified {} route!", broker_name);

    let _ = bridge_handle.await;
    */
}

static LOG_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

pub fn setup_logging() {
    // Using a std::sync::Once ensures this is only run once per test binary.
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        let file_appender = tracing_appender::rolling::never("logs", "integration_test.log");
        let (non_blocking_writer, guard) = tracing_appender::non_blocking(file_appender);

        *LOG_GUARD.lock().unwrap() = Some(guard);

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
            "{:<25} | {:>20} | {:>20}",
            "Test Name", "Write Performance", "Read Performance"
        );
        println!("{:-<25}-|-{:->20}-|-{:->20}", "", "", "");
        for result in results.iter() {
            println!(
                "{:<25} | {:>20} | {:>20}",
                result.test_name,
                format_pretty(result.write_performance),
                format_pretty(result.read_performance)
            );
        }
        println!("-----------------------------------------------------------------------\n");
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
    let _docker = DockerCompose::new(compose_file);
    // Give some time for docker to be ready
    _docker.up();
    test_fn().await;
}

pub const PERF_TEST_MESSAGE_COUNT: usize = 20_000;
pub const PERF_TEST_CONCURRENCY: usize = 100;

pub fn generate_message() -> CanonicalMessage {
    CanonicalMessage::from_json(json!({ "perf_test": true, "ts": chrono::Utc::now().to_rfc3339() }))
        .unwrap()
}

pub async fn measure_write_performance(
    name: &str,
    publisher: Arc<dyn MessagePublisher>,
    num_messages: usize,
    concurrency: usize,
) -> f64 {
    println!("\n--- Measuring Write Performance for {} ---", name);
    let (tx, rx): (Sender<CanonicalMessage>, Receiver<CanonicalMessage>) = bounded(concurrency * 2);

    tokio::spawn(async move {
        for _ in 0..num_messages {
            // The test will hang if the receiver is dropped, so we ignore the error.
            if tx.send(generate_message()).await.is_err() {
                break;
            }
        }
        tx.close();
    });

    let start_time = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();

    for _ in 0..concurrency {
        let rx_clone = rx.clone();
        let publisher_clone = publisher.clone();
        let batch_size = (num_messages / concurrency / 10).max(100); // Define a reasonable batch size

        tasks.spawn(async move {
            loop {
                // Wait for the first message to start a batch.
                let first_message = match rx_clone.recv().await {
                    Ok(msg) => msg,
                    Err(_) => break, // Channel is closed and empty, so we're done.
                };

                let mut batch = Vec::with_capacity(batch_size);
                batch.push(first_message);

                // Greedily fill the rest of the batch without waiting.
                for _ in 1..batch_size {
                    if let Ok(msg) = rx_clone.try_recv() {
                        batch.push(msg);
                    } else {
                        break; // Channel is empty for now.
                    }
                }

                // Retry sending the batch if some messages fail.
                let mut messages_to_send = batch;
                loop {
                    match publisher_clone.send_batch(messages_to_send.clone()).await {
                        Ok((_, failed)) if failed.is_empty() => break, // All sent successfully
                        Ok((_, failed)) => messages_to_send = failed,  // Retry failed messages
                        Err(e) => eprintln!("Error sending bulk messages: {}", e),
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await; // Backoff before retry
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}
    publisher.flush().await.unwrap();

    let duration = start_time.elapsed();
    let msgs_per_sec = num_messages as f64 / duration.as_secs_f64();

    println!(
        "  Wrote {} messages in {:.2?} ({} msgs/sec)",
        format_pretty(num_messages),
        duration,
        format_pretty(msgs_per_sec)
    );
    msgs_per_sec
}

/// A mock consumer that does nothing, useful for testing publishers in isolation.
#[derive(Clone)]
pub struct MockConsumer;

#[async_trait::async_trait]
impl MessageConsumer for MockConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        // This consumer will block forever, which is fine for tests that only need a publisher.
        // It prevents the route from exiting immediately.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        unreachable!();
    }
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        // This consumer will block forever, which is fine for tests that only need a publisher.
        // It prevents the route from exiting immediately.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        unreachable!();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub async fn measure_read_performance(
    name: &str,
    consumer: Arc<tokio::sync::Mutex<dyn MessageConsumer>>,
    num_messages: usize,
) -> f64 {
    println!("\n--- Measuring Read Performance for {} ---", name);
    let start_time = Instant::now();
    let final_count = Arc::new(AtomicUsize::new(0));
    let batch_size = 100; // A reasonable batch size for single-threaded reading.

    let final_count_clone = Arc::clone(&final_count);
    let consumer_clone = consumer.clone();

    loop {
        let current_count = final_count_clone.load(std::sync::atomic::Ordering::Relaxed);
        if current_count >= num_messages {
            break;
        }

        let missing = std::cmp::min(batch_size, num_messages - current_count);

        let mut consumer_guard = consumer_clone.lock().await;
        let receive_future = consumer_guard.receive_batch(missing);

        match tokio::time::timeout(Duration::from_secs(5), receive_future).await {
            Ok(Ok((msgs, commit))) if !msgs.is_empty() => {
                final_count_clone.fetch_add(msgs.len(), std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move { commit(None).await });
            }
            Ok(Err(e)) => {
                eprintln!("Error receiving message: {}. Stopping read.", e);
                break;
            }
            _ => {
                // Timeout or empty batch, assume we are done.
                break;
            }
        }
    }

    let num_messages = final_count.load(std::sync::atomic::Ordering::Relaxed);
    let duration: Duration = start_time.elapsed();
    // Use the actual number of messages received for calculation, in case of under-delivery.
    let msgs_per_sec = if duration.as_secs_f64() > 0.0 {
        num_messages as f64 / duration.as_secs_f64()
    } else {
        0.0
    };

    println!(
        "  Read {} messages in {:.2?} ({} msgs/sec)",
        format_pretty(num_messages),
        duration,
        format_pretty(msgs_per_sec)
    );
    msgs_per_sec
}

/// Formats a number with commas as thousand separators.
/// Handles both integers and floating-point numbers.
pub fn format_pretty<N: Display>(num: N) -> String {
    let s = num.to_string();
    let mut parts = s.splitn(2, '.');
    let integer_part = parts.next().unwrap_or("");
    let fractional_part = parts.next();

    let mut formatted_integer = String::with_capacity(integer_part.len() + integer_part.len() / 3);
    let mut count = 0;
    for ch in integer_part.chars().rev() {
        if count > 0 && count % 3 == 0 {
            formatted_integer.push('_');
        }
        formatted_integer.push(ch);
        count += 1;
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
