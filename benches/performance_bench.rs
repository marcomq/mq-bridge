use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

// Include the integration module from the tests directory so we can reuse the test logic.
#[path = "../tests/integration/mod.rs"]
mod integration;

use integration::common::{
    format_pretty, measure_read_performance, measure_single_read_performance,
    measure_single_write_performance, measure_write_performance, DockerCompose, PerformanceResult,
    PERF_TEST_CONCURRENCY,
};

const PERF_TEST_MESSAGE_COUNT: usize = 1000;
const DEFAULT_SLEEP: Duration = Duration::from_millis(50);

static BENCH_RESULTS: Lazy<Mutex<HashMap<String, PerformanceResult>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn should_run(backend_name: &str) -> bool {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|s| !s.starts_with('-'))
        .collect();
    if args.is_empty() {
        return true;
    }
    args.iter()
        .any(|arg| backend_name.contains(arg) || arg.contains(backend_name))
}

// --- Helper Modules for Backend Setup ---

#[cfg(feature = "nats")]
mod nats_helper {
    use super::*;
    use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
    use mq_bridge::models::NatsConfig;

    fn get_config() -> NatsConfig {
        NatsConfig {
            url: "nats://localhost:4222".to_string(),
            delayed_ack: false,
            ..Default::default()
        }
    }
    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let stream_name = "perf_nats_direct";
        let subject = "perf_nats_direct.subject";
        Arc::new(
            NatsPublisher::new(&get_config(), stream_name, subject)
                .await
                .unwrap(),
        )
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let stream_name = "perf_nats_direct";
        let subject = "perf_nats_direct.subject";
        Arc::new(Mutex::new(
            NatsConsumer::new(&get_config(), stream_name, subject)
                .await
                .unwrap(),
        ))
    }
}

#[cfg(feature = "mongodb")]
mod mongodb_helper {
    use super::*;
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::models::MongoDbConfig;

    fn get_config() -> MongoDbConfig {
        MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: "mq_bridge_test_db".to_string(),
            ..Default::default()
        }
    }
    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let collection_name = "perf_mongodb_direct";
        let config = get_config();

        // Drop collection before test to ensure clean state
        let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
        client
            .database(&config.database)
            .collection::<mongodb::bson::Document>(collection_name)
            .drop()
            .await
            .ok();

        Arc::new(
            MongoDbPublisher::new(&config, collection_name)
                .await
                .unwrap(),
        )
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let collection_name = "perf_mongodb_direct";
        Arc::new(Mutex::new(
            MongoDbConsumer::new(&get_config(), collection_name)
                .await
                .unwrap(),
        ))
    }
}

#[cfg(feature = "amqp")]
mod amqp_helper {
    use super::*;
    use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
    use mq_bridge::models::AmqpConfig;
    fn get_config() -> AmqpConfig {
        AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            delayed_ack: false,
            ..Default::default()
        }
    }

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let queue = "perf_test_amqp_direct";
        Arc::new(AmqpPublisher::new(&get_config(), queue).await.unwrap())
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let queue = "perf_test_amqp_direct";
        Arc::new(Mutex::new(
            AmqpConsumer::new(&get_config(), queue).await.unwrap(),
        ))
    }
}

#[cfg(feature = "kafka")]
mod kafka_helper {
    use super::*;
    use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
    use mq_bridge::models::KafkaConfig;

    fn get_config() -> KafkaConfig {
        KafkaConfig {
            brokers: "localhost:9092".to_string(),
            group_id: Some("perf_test_group_kafka".to_string()),
            producer_options: Some(vec![
                ("queue.buffering.max.ms".to_string(), "50".to_string()), // Linger for 50ms to batch messages
                ("acks".to_string(), "1".to_string()), // Wait for leader ack, a good balance
                ("compression.type".to_string(), "snappy".to_string()), // Use snappy compression
            ]),
            ..Default::default()
        }
    }
    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let topic = "perf_kafka_direct";
        Arc::new(KafkaPublisher::new(&get_config(), topic).await.unwrap())
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let topic = "perf_kafka_direct";
        Arc::new(Mutex::new(
            KafkaConsumer::new(&get_config(), topic).await.unwrap(),
        ))
    }
}

#[cfg(feature = "mqtt")]
mod mqtt_helper {
    use super::*;
    use mq_bridge::endpoints::mqtt::{MqttConsumer, MqttPublisher};
    use mq_bridge::models::MqttConfig;
    use uuid::Uuid;
    fn get_config() -> MqttConfig {
        MqttConfig {
            url: "tcp://localhost:1883".to_string(),
            queue_capacity: Some(PERF_TEST_MESSAGE_COUNT * 4), // For batch and single
            qos: Some(1),
            clean_session: false,
            keep_alive_seconds: Some(60),
            ..Default::default()
        }
    }

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let topic = "perf_mqtt_direct";
        let publisher_id = format!("pub-{}", Uuid::new_v4().as_simple());
        Arc::new(
            MqttPublisher::new(&get_config(), topic, &publisher_id)
                .await
                .unwrap(),
        )
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let topic = "perf_mqtt_direct";
        let consumer_id = format!("sub-{}", Uuid::new_v4().as_simple());
        Arc::new(Mutex::new(
            MqttConsumer::new(&get_config(), topic, &consumer_id)
                .await
                .unwrap(),
        ))
    }
}

fn performance_benchmarks(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("performance");
    // Since these are integration tests involving network/disk, we reduce sample size
    // and increase measurement time to accommodate their duration.
    group.sample_size(10);
    group.throughput(Throughput::Elements(PERF_TEST_MESSAGE_COUNT as u64));
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(1));

    macro_rules! bench_backend {
        ($feature:literal, $name:literal, $compose_file:literal, $helper:path) => {
            #[cfg(feature = $feature)]
            if should_run($name)
            {
                use $helper as backend;

                // Start the Docker environment for this backend.
                // The DockerCompose struct handles `docker-compose up` on creation and `down` on drop.
                let _docker = DockerCompose::new($compose_file);
                _docker.down();
                _docker.up();

                group.bench_function(concat!($name, "_single_write"), |b| {
                    b.to_async(&rt).iter_custom(|iters| async move {
                        let mut total = Duration::ZERO;
                        let publisher = backend::create_publisher().await;
                        let consumer = backend::create_consumer().await;
                        for _ in 0..iters {
                            // Note: create_publisher must be `pub` in the integration module
                            let duration = measure_single_write_performance(
                                concat!($name, "_single_write"),
                                Arc::clone(&publisher),
                                PERF_TEST_MESSAGE_COUNT,
                                PERF_TEST_CONCURRENCY,
                            )
                            .await;
                            total += duration;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                            // Cleanup: Read the messages we just wrote so the queue is empty for the next iteration
                            measure_single_read_performance("cleanup", Arc::clone(&consumer), PERF_TEST_MESSAGE_COUNT).await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                        }
                        let msgs_per_sec = (iters as f64 * PERF_TEST_MESSAGE_COUNT as f64) / total.as_secs_f64();
                        {
                            let mut results = BENCH_RESULTS.lock().await;
                            let stats = results.entry($name.to_string()).or_default();
                            stats.single_write_performance = msgs_per_sec;
                        }
                        println!("\n{} single_write: {} iters, total time {:?}, {:.2} msgs/sec", $name, iters, total, msgs_per_sec);
                        total
                    })
                });

                group.bench_function(concat!($name, "_single_read"), |b| {
                    b.to_async(&rt).iter_custom(|iters| async move {
                        let mut total = Duration::ZERO;
                        let publisher = backend::create_publisher().await;
                        let consumer = backend::create_consumer().await;
                        for _ in 0..iters {

                            // Fill the queue first (setup, not measured)
                            measure_single_write_performance(
                                "setup_fill",
                                Arc::clone(&publisher),
                                PERF_TEST_MESSAGE_COUNT,
                                PERF_TEST_CONCURRENCY,
                            )
                            .await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;

                            let duration = measure_single_read_performance(
                                concat!($name, "_single_read"),
                                Arc::clone(&consumer),
                                PERF_TEST_MESSAGE_COUNT,
                            )
                            .await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                            total += duration;
                        }
                        let msgs_per_sec = (iters as f64 * PERF_TEST_MESSAGE_COUNT as f64) / total.as_secs_f64();
                        {
                            let mut results = BENCH_RESULTS.lock().await;
                            let stats = results.entry($name.to_string()).or_default();
                            stats.single_read_performance = msgs_per_sec;
                        }
                        println!("\n{} single_read: {} iters, total time {:?}, {:.2} msgs/sec", $name, iters, total, msgs_per_sec);
                        total
                    })
                });

                group.bench_function(concat!($name, "_batch_write"), |b| {
                    b.to_async(&rt).iter_custom(|iters| async move {
                        let mut total = Duration::ZERO;
                        let publisher = backend::create_publisher().await;
                        let consumer = backend::create_consumer().await;
                        for _ in 0..iters {
                            let duration = measure_write_performance(
                                concat!($name, "_batch_write"),
                                Arc::clone(&publisher),
                                PERF_TEST_MESSAGE_COUNT,
                                PERF_TEST_CONCURRENCY,
                            )
                            .await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                            total += duration;

                            // Cleanup: Read the messages we just wrote
                            measure_read_performance("cleanup", Arc::clone(&consumer), PERF_TEST_MESSAGE_COUNT).await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                        }
                        let msgs_per_sec = (iters as f64 * PERF_TEST_MESSAGE_COUNT as f64) / total.as_secs_f64();
                        {
                            let mut results = BENCH_RESULTS.lock().await;
                            let stats = results.entry($name.to_string()).or_default();
                            stats.write_performance = msgs_per_sec;
                        }
                        println!("\n{} batch_write: {} iters, total time {:?}, {:.2} msgs/sec", $name, iters, total, msgs_per_sec);
                        total
                    })
                });

                group.bench_function(concat!($name, "_batch_read"), |b| {
                    b.to_async(&rt).iter_custom(|iters| async move {
                        let mut total = Duration::ZERO;
                        let publisher = backend::create_publisher().await;
                        let consumer = backend::create_consumer().await;
                        for _ in 0..iters {

                            // Fill the queue first (setup, not measured)
                            measure_write_performance(
                                "setup_fill",
                                Arc::clone(&publisher),
                                PERF_TEST_MESSAGE_COUNT,
                                PERF_TEST_CONCURRENCY,
                            )
                            .await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;

                            let duration = measure_read_performance(
                                concat!($name, "_batch_read"),
                                Arc::clone(&consumer),
                                PERF_TEST_MESSAGE_COUNT,
                            )
                            .await;
                            tokio::time::sleep(DEFAULT_SLEEP).await;
                            total += duration;
                        }
                        let msgs_per_sec = (iters as f64 * PERF_TEST_MESSAGE_COUNT as f64) / total.as_secs_f64();
                        {
                            let mut results = BENCH_RESULTS.lock().await;
                            let stats = results.entry($name.to_string()).or_default();
                            stats.read_performance = msgs_per_sec;
                        }
                        println!("\n{} batch_read: {} iters, total time {:?}, {:.2} msgs/sec", $name, iters, total, msgs_per_sec);
                        total
                    })
                });
                _docker.down();
            }
        };
    }

    bench_backend!(
        "kafka",
        "kafka",
        "tests/integration/docker-compose/kafka.yml",
        kafka_helper
    );
    bench_backend!(
        "amqp",
        "amqp",
        "tests/integration/docker-compose/amqp.yml",
        amqp_helper
    );
    bench_backend!(
        "nats",
        "nats",
        "tests/integration/docker-compose/nats.yml",
        nats_helper
    );
    bench_backend!(
        "mongodb",
        "mongodb",
        "tests/integration/docker-compose/mongodb.yml",
        mongodb_helper
    );
    bench_backend!(
        "mqtt",
        "mqtt",
        "tests/integration/docker-compose/mqtt.yml",
        mqtt_helper
    );

    // Print consolidated results
    let results = BENCH_RESULTS.blocking_lock();
    if !results.is_empty() {
        println!("\n\n--- Consolidated Performance Test Results (msgs/sec) ---");
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
    group.finish();
}

criterion_group!(benches, performance_benchmarks);
criterion_main!(benches);
