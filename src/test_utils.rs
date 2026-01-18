#![allow(dead_code)] // This module contains helpers used by various integration tests.
use crate::traits::{BoxFuture, MessagePublisher, Received};
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
    }

    pub fn start_service(&self, service: &str) {
        println!(
            "Starting docker-compose service {} from {}...",
            service, self.compose_file
        );
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose_file)
            .arg("start")
            .arg(service)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("Failed to start docker compose service");

        assert!(status.success(), "docker compose start failed");
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
        let msg = CanonicalMessage::new(payload.into_bytes(), None);
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
    run_pipeline_test_internal(broker_name, config_yaml, 5, false, None).await;
}

pub async fn run_performance_pipeline_test(
    broker_name: &str,
    config_yaml: &str,
    num_messages: usize,
) {
    run_pipeline_test_internal(broker_name, config_yaml, num_messages, true, None).await;
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
            tokio::time::sleep(Duration::from_millis(500)).await;
            docker_controller.stop_service(&service_name);
            tokio::time::sleep(Duration::from_secs(5)).await;
            docker_controller.start_service(&service_name);
        }) as BoxFuture<'static, ()>
    });

    run_pipeline_test_internal(broker_name, config_yaml, 10000, false, Some(injector)).await;
}

async fn run_pipeline_test_internal(
    broker_name: &str,
    config_yaml: &str,
    num_messages: usize,
    is_performance_test: bool,
    chaos_injector: Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>>,
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
    while wait_start.elapsed() < timeout {
        let batch = harness.out_channel.drain_messages();
        if !batch.is_empty() {
            if !is_performance_test {
                for msg in &batch {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                        if let Some(num) = val.get("message_num").and_then(|v| v.as_u64()) {
                            unique_received_ids.insert(num);
                        }
                    }
                }
            }
            received.extend(batch);
        }

        if is_performance_test {
            if received.len() >= num_messages {
                break;
            }
        } else if unique_received_ids.len() >= num_messages {
            break;
        }

        if last_log_time.elapsed() > Duration::from_secs(5) {
            println!(
                "Progress: {}/{} messages received (Unique: {})",
                received.len(),
                num_messages,
                unique_received_ids.len()
            );
            last_log_time = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Route::stop(&in_route_name).await;
    Route::stop(&out_route_name).await;

    // Drain any remaining messages that arrived during shutdown
    let batch = harness.out_channel.drain_messages();
    if !batch.is_empty() {
        if !is_performance_test {
            for msg in &batch {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                    if let Some(num) = val.get("message_num").and_then(|v| v.as_u64()) {
                        unique_received_ids.insert(num);
                    }
                }
            }
        }
        received.extend(batch);
    }
    let duration = start_time.elapsed();

    if is_performance_test {
        let messages_per_second = received.len() as f64 / duration.as_secs_f64();
        println!("\n--- {} Performance Test Results ---", broker_name);
        println!(
            "Processed {} messages in {:.3} seconds.",
            received.len(),
            duration.as_secs_f64()
        );
        println!("Rate: {:.2} messages/second", messages_per_second);
        println!("--------------------------------\n");

        add_performance_result(PerformanceResult {
            test_name: format!("{} Pipeline", broker_name),
            write_performance: messages_per_second,
            read_performance: messages_per_second,
            single_write_performance: 0.0,
            single_read_performance: 0.0,
        });

        assert_eq!(
            received.len(),
            num_messages,
            "TEST FAILED for [{}]: Expected {} messages, but found {}.",
            broker_name,
            num_messages,
            received.len()
        );
    } else {
        assert_eq!(
            unique_received_ids.len(),
            num_messages,
            "TEST FAILED for [{}]: Expected {} unique messages, but found {}. Total received: {}",
            broker_name,
            num_messages,
            unique_received_ids.len(),
            received.len()
        );
    }

    println!("Successfully verified {} route!", broker_name);
}

static LOG_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

pub fn setup_logging() {
    // Using a std::sync::Once ensures this is only run once per test binary.
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
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
        for result in results.iter() {
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

pub fn generate_message() -> CanonicalMessage {
    CanonicalMessage::new(STATIC_PAYLOAD.clone(), None)
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
/// to `MAX_RETRIES` times if any messages fail.
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
    let (tx, rx): (Sender<CanonicalMessage>, Receiver<CanonicalMessage>) =
        bounded(batch_size * concurrency * 2);

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
            for _ in 0..count {
                if tx.send(generate_message()).await.is_err() {
                    break;
                }
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
                let mut batch_size = messages_to_send.len();
                let mut retry_count = 0;
                const MAX_RETRIES: usize = 5;
                loop {
                    match publisher_clone
                        .send_batch(std::mem::take(&mut messages_to_send))
                        .await
                    {
                        Ok(SentBatch::Ack) => {
                            final_count_clone
                                .fetch_add(batch_size, std::sync::atomic::Ordering::Relaxed);
                            break; // All sent successfully
                        }
                        Ok(SentBatch::Partial {
                            responses: _,
                            failed,
                        }) => {
                            if failed.is_empty() {
                                final_count_clone
                                    .fetch_add(batch_size, std::sync::atomic::Ordering::Relaxed);
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
                                eprintln!("Retrying: {}", retryable.len());
                                messages_to_send =
                                    retryable.into_iter().map(|(msg, _)| msg).collect();
                                batch_size = messages_to_send.len();
                                retry_count = 0; // Reset on partial success
                            }
                        }
                        Err(e) => {
                            eprintln!("Error sending bulk messages: {}", e);
                            retry_count += 1;
                            if retry_count >= MAX_RETRIES {
                                eprintln!("Max retries reached, giving up on batch");
                                break;
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

        match tokio::time::timeout(Duration::from_secs(10), receive_future).await {
            Ok(Ok(batch)) if !batch.messages.is_empty() => {
                final_count += batch.messages.len();
                let commit = batch.commit;
                let permit = commit_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Semaphore closed");
                tokio::spawn(async move {
                    let _ = commit(None).await;
                    drop(permit);
                });
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
                if tx.send(generate_message()).await.is_err() {
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
                loop {
                    match publisher_clone.send(message.clone()).await {
                        Ok(_) => {
                            final_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        Err(e) => {
                            eprintln!("Error sending message: {}. Retrying...", e);
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
        if let Ok(Ok(Received {
            message: _msg,
            commit,
        })) = tokio::time::timeout(Duration::from_secs(10), receive_future).await
        {
            final_count += 1;
            let permit = commit_semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Semaphore closed");
            tokio::spawn(async move {
                let _ = commit(None).await;
                drop(permit);
            });
        } else {
            eprintln!("Failed to receive message or timed out. Stopping read.");
            break;
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
    let mut filters = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // Only skip values for flags that are known to take values
        if arg == "--output-format"
            || arg == "--baseline"
            || arg == "--save-baseline"
            || arg == "--load-baseline"
            || arg == "--profile-time"
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
}

#[macro_export]
macro_rules! run_benchmarks {
    ($name:literal, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr) => {
        $group.bench_function(concat!($name, "_single_write"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    $crate::test_utils::measure_read_performance(
                        "cleanup",
                        std::sync::Arc::clone(&consumer),
                        $msg_count,
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
            })
        });

        $group.bench_function(concat!($name, "_single_read"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;

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
            })
        });

        $group.bench_function(concat!($name, "_batch_write"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    total += duration;

                    $crate::test_utils::measure_read_performance(
                        "cleanup",
                        std::sync::Arc::clone(&consumer),
                        $msg_count,
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
            })
        });

        $group.bench_function(concat!($name, "_batch_read"), |b| {
            b.to_async($rt).iter_custom(|iters| async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;

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
            })
        });
    };
}

#[macro_export]
macro_rules! bench_backend {
    ("", $name:literal, $compose_file:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr) => {
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;

            // Start the Docker environment for this backend.
            // The DockerCompose struct handles `docker-compose up` on creation and `down` on drop.
            let _docker = $crate::test_utils::DockerCompose::new($compose_file);
            _docker.down();
            _docker.up();

            $crate::run_benchmarks!($name, $group, $rt, $results, $msg_count, $concurrency);
            _docker.down();
        }
    };
    ($feature:literal, $name:literal, $compose_file:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr) => {
        #[cfg(feature = $feature)]
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;

            // Start the Docker environment for this backend.
            // The DockerCompose struct handles `docker-compose up` on creation and `down` on drop.
            let _docker = $crate::test_utils::DockerCompose::new($compose_file);
            _docker.down();
            _docker.up();

            $crate::run_benchmarks!($name, $group, $rt, $results, $msg_count, $concurrency);
            _docker.down();
        }
    };
    ($feature:literal, $name:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr) => {
        #[cfg(feature = $feature)]
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;
            // No docker setup
            $crate::run_benchmarks!($name, $group, $rt, $results, $msg_count, $concurrency);
        }
    };
    ($name:literal, $helper:path, $group:expr, $rt:expr, $results:expr, $msg_count:expr, $concurrency:expr) => {
        if $crate::test_utils::should_run_benchmark($name) {
            use $helper as backend;
            // No docker setup, no feature gate
            $crate::run_benchmarks!($name, $group, $rt, $results, $msg_count, $concurrency);
        }
    };
}
