#![allow(dead_code)]
use std::sync::Arc;

use mq_bridge::test_utils::PERF_TEST_MESSAGE_COUNT;

use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging,
};
use mq_bridge::traits::MessagePublisher;
const CONFIG_YAML: &str = r#"
routes:
  memory_to_mongodb:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-mongodb" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      mongodb: { url: "mongodb://localhost:27017", database: "mq_bridge_test", collection: "test_collection" }

  mongodb_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      mongodb: { url: "mongodb://localhost:27017", database: "mq_bridge_test", collection: "test_collection" }
    output:
      memory: { topic: "test-out-mongodb", capacity: {out_capacity} }
"#;

pub async fn test_mongodb_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("mongodb", &config_yaml).await;
    })
    .await;
}

pub async fn test_mongodb_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/mongodb.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("mongodb", &config_yaml, controller, "mongodb").await;
        },
    )
    .await;
}

#[tokio::test]
async fn test_mongodb_subscriber_no_duplicates() {
    use mq_bridge::models::{Endpoint, Route};
    use mq_bridge::type_handler::TypeHandler;
    use mq_bridge::Handled;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    setup_logging();
    let collection_name = "test_no_dupes_route";
    let db_name = "mq_bridge_test_dupes_route";
    let url = "mongodb://localhost:27017";

    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct TestMsg {
        id: u32,
        data: String,
    }

    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async move {
        // Clean setup
        let client = mongodb::Client::with_uri_str(url).await.unwrap();
        client
            .database(db_name)
            .collection::<mongodb::bson::Document>(collection_name)
            .drop()
            .await
            .ok();

        // 1. Setup Input Endpoint (MongoDB Subscriber)
        let input_config = mq_bridge::models::MongoDbConfig {
            url: url.to_string(),
            database: db_name.to_string(),
            collection: Some(collection_name.to_string()),
            change_stream: true, // Subscriber mode
            polling_interval_ms: Some(10),
            format: mq_bridge::models::MongoDbFormat::Json,
            ..Default::default()
        };
        let input = Endpoint::new(mq_bridge::models::EndpointType::MongoDb(input_config.clone()));

        // 2. Setup Output Endpoint (Memory to verify)
        let output = Endpoint::new_memory("out_no_dupes", 20);

        // 3. Setup TypeHandler
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        
        let type_handler = TypeHandler::new().add("test_msg", move |msg: TestMsg| {
            let counter = counter_clone.clone();
            async move {
                assert!(msg.id == 1 || msg.id == 2);
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Handled::Ack)
            }
        });

        // 4. Create Route
        let route = Route::new(input, output).with_handler(type_handler);

        // 5. Run Route (Start subscriber)
        route.deploy("test_no_dupes_route").await.unwrap();

        // 6. Publish messages
        let publisher = MongoDbPublisher::new(&input_config).await.unwrap();
        let msg1 = TestMsg { id: 1, data: "one".to_string() };
        let msg2 = TestMsg { id: 2, data: "two".to_string() };
        
        publisher.send(mq_bridge::msg!(&msg1, "test_msg")).await.unwrap();
        publisher.send(mq_bridge::msg!(&msg2, "test_msg")).await.unwrap();

        // Wait for processing
        let start = std::time::Instant::now();
        while counter.load(Ordering::SeqCst) < 2 {
            if start.elapsed() > std::time::Duration::from_secs(10) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        mq_bridge::stop_route("test_no_dupes_route").await;
        assert_eq!(counter.load(Ordering::SeqCst), 2, "Should have processed exactly 2 messages");
    }).await;
}

pub async fn test_mongodb_replica_set_pipeline() {
    setup_logging();
    run_test_with_docker(
        "tests/integration/docker-compose/mongodb-replica.yml",
        || async {
            let config_yaml = CONFIG_YAML
                .replace(
                    "mongodb://localhost:27017",
                    "mongodb://localhost:27018/?replicaSet=rs0",
                )
                .replace("memory_to_mongodb", "memory_to_mongodb_rs")
                .replace("mongodb_to_memory", "mongodb_rs_to_memory")
                .replace(
                    "{out_capacity}",
                    &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
                );
            run_pipeline_test("mongodb_rs", &config_yaml).await;
        },
    )
    .await;
}

pub async fn test_mongodb_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("mongodb", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

async fn run_mongodb_direct_perf_test_impl(
    compose_file: &str,
    url: &str,
    database: &str,
    collection_name: &str,
    test_name: &str,
) {
    setup_logging();
    let url = url.to_string();
    let database = database.to_string();
    let collection_name = collection_name.to_string();
    let test_name = test_name.to_string();

    run_test_with_docker(compose_file, || async move {
        let config = mq_bridge::models::MongoDbConfig {
            url,
            database,
            ..Default::default()
        };

        // Drop collection before test
        let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
        client
            .database(&config.database)
            .collection::<mongodb::bson::Document>(&collection_name)
            .drop()
            .await
            .ok();

        let result = run_direct_perf_test(
            &test_name,
            || async {
                let mut pub_config = config.clone();
                pub_config.collection = Some(collection_name.clone());
                Arc::new(MongoDbPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.collection = Some(collection_name.clone());
                Arc::new(tokio::sync::Mutex::new(
                    MongoDbConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_mongodb_performance_direct() {
    run_mongodb_direct_perf_test_impl(
        "tests/integration/docker-compose/mongodb.yml",
        "mongodb://localhost:27017",
        "mq_bridge_test_db",
        "perf_mongodb_direct",
        "MongoDB",
    )
    .await;
}

pub async fn test_mongodb_replica_set_performance_direct() {
    run_mongodb_direct_perf_test_impl(
        "tests/integration/docker-compose/mongodb-replica.yml",
        "mongodb://localhost:27018/?replicaSet=rs0",
        "mq_bridge_test_db_rs",
        "perf_mongodb_rs_direct",
        "MongoDB RS",
    )
    .await;
}
