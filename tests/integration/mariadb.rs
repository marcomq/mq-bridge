#![allow(dead_code)]
#![cfg(feature = "sqlx")]

use mq_bridge::endpoints::sqlx::{SqlxConsumer, SqlxPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test_named,
    run_test_with_docker, run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::future::Future;
use std::sync::Arc;

const DOCKER_COMPOSE_FILE: &str = "tests/integration/docker-compose/mariadb.yml";
const DATABASE_URL: &str = "mysql://testuser:testpass@localhost:3307/testdb";
const TABLE_NAME: &str = "messages";

async fn setup_db() {
    let config = mq_bridge::models::SqlxConfig {
        url: DATABASE_URL.to_string(),
        table: TABLE_NAME.to_string(),
        auto_create_table: true,
        ..Default::default()
    };
    let _publisher = SqlxPublisher::new(&config).await.unwrap();
}

const CONFIG_YAML: &str = r#"
routes:
  memory_to_sqlx:
    concurrency: 4
    batch_size: 1024
    input:
      memory: { topic: "sqlx-mariadb-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      sqlx:
        url: "mysql://testuser:testpass@localhost:3307/testdb"
        table: "messages"

  sqlx_to_memory:
    concurrency: 4
    batch_size: 1024
    input:
      sqlx:
        url: "mysql://testuser:testpass@localhost:3307/testdb"
        table: "messages"
        delete_after_read: true
        polling_interval_ms: 20
    output:
      memory: { topic: "sqlx-mariadb-out", capacity: {out_capacity} }
"#;

pub async fn test_mariadb_performance_pipeline() {
    run_mariadb_test(|config_yaml| async move {
        run_performance_pipeline_test_named(
            "sqlx",
            "mariadb",
            &config_yaml,
            PERF_TEST_MESSAGE_COUNT,
        )
        .await;
    })
    .await;
}

async fn run_mariadb_test<F, Fut>(runner: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        setup_db().await;
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        runner(config_yaml).await;
    })
    .await;
}

pub async fn test_mariadb_cursor_unmappable_types() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        super::mysql::assert_cursor_reads_unmappable_types(DATABASE_URL, "mariadb-types").await;
    })
    .await;
}

pub async fn test_mariadb_checkpoint_table() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        super::mysql::assert_sql_checkpoint_round_trip(DATABASE_URL, "mariadb").await;
    })
    .await;
}

pub async fn test_mariadb_performance_direct() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        setup_db().await;
        let config = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: TABLE_NAME.to_string(),
            delete_after_read: true,
            polling_interval_ms: Some(1),
            auto_create_table: true,
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "SQLx (MariaDB)",
            || async {
                let pub_config = config.clone();
                Arc::new(SqlxPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let consumer_config = config.clone();
                Arc::new(tokio::sync::Mutex::new(
                    SqlxConsumer::new(&consumer_config).await.unwrap(),
                ))
            },
        )
        .await;

        add_performance_result(result);
    })
    .await;
}

pub async fn test_mariadb_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(DOCKER_COMPOSE_FILE, |controller| async move {
        setup_db().await;
        let config = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: TABLE_NAME.to_string(),
            acquire_timeout_ms: Some(1000),
            ..Default::default()
        };

        let publisher = SqlxPublisher::new(&config).await.unwrap();
        let consumer = SqlxConsumer::new(&config).await.unwrap();

        println!("[MariaDB] Checking initial status...");
        sleep(Duration::from_secs(2)).await;
        let pub_status = publisher.status().await;
        let con_status = consumer.status().await;
        assert!(
            pub_status.healthy,
            "Publisher should be healthy initially. Status: {:?}",
            pub_status
        );
        assert!(
            con_status.healthy,
            "Consumer should be healthy initially. Status: {:?}",
            con_status
        );
        println!("[MariaDB] Initial status check OK.");

        controller.stop_service("mariadb");
        println!("[MariaDB] Service 'mariadb' stopped. Waiting for disconnect detection...");

        let start = std::time::Instant::now();
        loop {
            if !publisher.status().await.healthy && !consumer.status().await.healthy {
                println!("[MariaDB] Disconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[MariaDB] Timeout waiting for disconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }

        controller.start_service("mariadb");
        println!("[MariaDB] Service 'mariadb' started. Waiting for reconnect...");

        let start = std::time::Instant::now();
        loop {
            if publisher.status().await.healthy && consumer.status().await.healthy {
                println!("[MariaDB] Reconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[MariaDB] Timeout waiting for reconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }
        println!("[MariaDB] Status test successful.");
    })
    .await;
}
