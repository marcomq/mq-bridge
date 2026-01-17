#![allow(dead_code)]
use std::sync::Arc;

use crate::integration::common::PERF_TEST_MESSAGE_COUNT;

use super::common::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging,
};
use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
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
                Arc::new(
                    MongoDbPublisher::new(&config, &collection_name)
                        .await
                        .unwrap(),
                )
            },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    MongoDbConsumer::new(&config, &collection_name)
                        .await
                        .unwrap(),
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
