use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

// Include the integration module from the tests directory so we can reuse the test logic.
#[path = "../tests/integration/mod.rs"]
mod integration; // Still needed for backend modules like kafka, nats etc.

use mq_bridge::bench_backend;
use mq_bridge::test_utils::{print_benchmark_results, PerformanceResult, PERF_TEST_CONCURRENCY};

const PERF_TEST_MESSAGE_COUNT: usize = 1000;

static BENCH_RESULTS: Lazy<Mutex<HashMap<String, PerformanceResult>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// --- Helper Modules for Backend Setup ---

#[cfg(feature = "nats")]
pub mod nats_helper {
    use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
    use mq_bridge::models::NatsConfig;
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

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
pub mod mongodb_helper {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::models::MongoDbConfig;
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

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
        Arc::new(
            MongoDbPublisher::new(&config, collection_name)
                .await
                .unwrap(),
        )
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
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

        Arc::new(Mutex::new(
            MongoDbConsumer::new(&config, collection_name)
                .await
                .unwrap(),
        ))
    }
}

#[cfg(feature = "amqp")]
pub mod amqp_helper {
    use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
    use mq_bridge::models::AmqpConfig;
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

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
pub mod kafka_helper {
    use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
    use mq_bridge::models::KafkaConfig;
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn get_config() -> KafkaConfig {
        KafkaConfig {
            url: "localhost:9092".to_string(),
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
pub mod mqtt_helper {
    use super::PERF_TEST_MESSAGE_COUNT;
    use mq_bridge::endpoints::mqtt::{MqttConsumer, MqttPublisher};
    use mq_bridge::models::MqttConfig;
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    fn get_config() -> MqttConfig {
        MqttConfig {
            url: "tcp://localhost:1883".to_string(),
            queue_capacity: Some(PERF_TEST_MESSAGE_COUNT * 4), // For batch and single
            max_inflight: Some(1000),
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

#[cfg(feature = "aws")]
pub mod aws_helper {
    use aws_sdk_sns::config::Credentials;
    use mq_bridge::endpoints::aws::{AwsConsumer, AwsPublisher};
    use mq_bridge::models::{AwsConfig, AwsEndpoint};
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn ensure_queue_exists() -> String {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url("http://localhost:4566")
            .credentials_provider(Credentials::new("test", "test", None, None, "static"))
            .load()
            .await;
        let client = aws_sdk_sqs::Client::new(&config);
        let resp = client
            .create_queue()
            .queue_name("perf-test-queue")
            .send()
            .await
            .expect("Failed to create SQS queue");
        let queue_url = resp.queue_url.expect("SQS queue URL was None");
        client
            .purge_queue()
            .queue_url(&queue_url)
            .send()
            .await
            .expect("Failed to purge SQS queue");
        queue_url
    }

    fn get_endpoint(queue_url: Option<String>) -> AwsEndpoint {
        AwsEndpoint {
            queue_url: Some(queue_url.unwrap_or_else(|| {
                "http://localhost:4566/000000000000/perf-test-queue".to_string()
            })),
            topic_arn: None,
            config: AwsConfig {
                region: Some("us-east-1".to_string()),
                endpoint_url: Some("http://localhost:4566".to_string()),
                access_key: Some("test".to_string()),
                secret_key: Some("test".to_string()),
                ..Default::default()
            },
        }
    }

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let url = ensure_queue_exists().await;
        Arc::new(AwsPublisher::new(&get_endpoint(Some(url))).await.unwrap())
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        Arc::new(Mutex::new(
            AwsConsumer::new(&get_endpoint(None)).await.unwrap(),
        ))
    }
}

#[cfg(feature = "zeromq")]
pub mod zeromq_helper {
    use super::PERF_TEST_MESSAGE_COUNT;
    use mq_bridge::endpoints::zeromq::{ZeroMqConsumer, ZeroMqPublisher};
    use mq_bridge::models::{ZeroMqConfig, ZeroMqEndpoint, ZeroMqSocketType};
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use once_cell::sync::Lazy;
    use rand::Rng;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    static PORT: Lazy<AtomicU16> = Lazy::new(|| {
        let mut rng = rand::rng();
        AtomicU16::new(rng.random_range(10000..60000))
    });

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let port = PORT.load(Ordering::SeqCst);
        let config = ZeroMqEndpoint {
            topic: None,
            config: ZeroMqConfig {
                url: format!("ipc:///tmp/mq-bridge-{}.sock", port),
                socket_type: Some(ZeroMqSocketType::Push),
                bind: false,
                internal_buffer_size: Some(PERF_TEST_MESSAGE_COUNT + 1),
            },
        };
        Arc::new(ZeroMqPublisher::new(&config).await.unwrap())
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let port = PORT.fetch_add(1, Ordering::SeqCst) + 1;
        let path = format!("/tmp/mq-bridge-{}.sock", port);
        let _ = std::fs::remove_file(&path);
        let config = ZeroMqEndpoint {
            topic: None,
            config: ZeroMqConfig {
                url: format!("ipc://{}", path),
                socket_type: Some(ZeroMqSocketType::Pull),
                bind: true,
                internal_buffer_size: Some(PERF_TEST_MESSAGE_COUNT + 1),
            },
        };
        Arc::new(Mutex::new(ZeroMqConsumer::new(&config).await.unwrap()))
    }
}

#[cfg(feature = "ibm-mq")]
pub mod ibm_mq_helper {
    use mq_bridge::{
        models::{IbmMqConfig, IbmMqEndpoint},
        traits::{MessageConsumer, MessagePublisher},
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    pub fn get_config() -> IbmMqConfig {
        IbmMqConfig {
            user: Some("app".to_string()),
            password: Some("admin".to_string()),
            queue_manager: "QM1".to_string(),
            connection_name: "localhost(1414)".to_string(),
            channel: "DEV.APP.SVRCONN".to_string(),
            ..Default::default()
        }
    }

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        let endpoint_config = IbmMqEndpoint {
            queue: Some("DEV.QUEUE.1".to_string()),
            topic: None,
            config: get_config(),
        };

        let publisher =
            mq_bridge::endpoints::ibm_mq::create_ibm_mq_publisher("bench_pub", &endpoint_config)
                .await
                .expect("Failed to create publisher");
        Arc::new(publisher)
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        let endpoint_config = IbmMqEndpoint {
            queue: Some("DEV.QUEUE.1".to_string()),
            topic: None,
            config: get_config(),
        };

        let consumer =
            mq_bridge::endpoints::ibm_mq::create_ibm_mq_consumer("bench_sub", &endpoint_config)
                .await
                .expect("Failed to create consumer");
        Arc::new(Mutex::new(consumer))
    }
}

pub mod memory_helper {
    use super::PERF_TEST_MESSAGE_COUNT;
    use mq_bridge::endpoints::memory::{MemoryConsumer, MemoryPublisher};
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    pub async fn create_publisher() -> Arc<dyn MessagePublisher> {
        Arc::new(MemoryPublisher::new_local(
            "perf_memory_bench",
            PERF_TEST_MESSAGE_COUNT * 2,
        ))
    }

    pub async fn create_consumer() -> Arc<Mutex<dyn MessageConsumer>> {
        Arc::new(Mutex::new(MemoryConsumer::new_local(
            "perf_memory_bench",
            PERF_TEST_MESSAGE_COUNT * 2,
        )))
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

    bench_backend!(
        "aws",
        "aws",
        "tests/integration/docker-compose/aws.yml",
        aws_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "kafka",
        "kafka",
        "tests/integration/docker-compose/kafka.yml",
        kafka_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "amqp",
        "amqp",
        "tests/integration/docker-compose/amqp.yml",
        amqp_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "nats",
        "nats",
        "tests/integration/docker-compose/nats.yml",
        nats_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "mongodb",
        "mongodb",
        "tests/integration/docker-compose/mongodb.yml",
        mongodb_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "mqtt",
        "mqtt",
        "tests/integration/docker-compose/mqtt.yml",
        mqtt_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );

    bench_backend!(
        "zeromq",
        "zeromq",
        zeromq_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "ibm-mq",
        "ibm-mq",
        "tests/integration/docker-compose/ibm_mq.yml",
        ibm_mq_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );
    bench_backend!(
        "memory",
        memory_helper,
        group,
        &rt,
        &BENCH_RESULTS,
        PERF_TEST_MESSAGE_COUNT,
        PERF_TEST_CONCURRENCY
    );

    // Print consolidated results
    let results = BENCH_RESULTS.blocking_lock();
    print_benchmark_results(&results, PERF_TEST_MESSAGE_COUNT);
    group.finish();
}

criterion_group!(benches, performance_benchmarks);
criterion_main!(benches);
