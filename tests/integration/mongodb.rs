#![allow(dead_code)]
use std::sync::Arc;

use crate::integration::common::PERF_TEST_MESSAGE_COUNT;

use super::common::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    run_test_with_docker, setup_logging,
};
use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
const CONFIG_YAML: &str = r#"
routes:
  memory_to_mongodb:
    input:
      memory: { topic: "test-in-mongodb" }
    output:
      mongodb: { url: "mongodb://localhost:27017", database: "mq_bridge_test", collection: "test_collection" }

  mongodb_to_memory:
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

pub async fn test_mongodb_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let collection_name = "perf_mongodb_direct";
        let config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: "mq_bridge_test_db".to_string(),
            ..Default::default()
        };

        // Drop collection before test
        let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
        client
            .database(&config.database)
            .collection::<mongodb::bson::Document>(collection_name)
            .drop()
            .await
            .ok();

        let result = run_direct_perf_test(
            "MongoDB",
            || async {
                Arc::new(
                    MongoDbPublisher::new(&config, collection_name)
                        .await
                        .unwrap(),
                )
            },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    MongoDbConsumer::new(&config, collection_name)
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
