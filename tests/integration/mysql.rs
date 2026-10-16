#![allow(dead_code)]
#![cfg(feature = "sqlx")]

use mq_bridge::endpoints::sqlx::{SqlxConsumer, SqlxPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test_named,
    run_test_with_docker, run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::future::Future;
use std::sync::Arc;

const DOCKER_COMPOSE_FILE: &str = "tests/integration/docker-compose/mysql.yml";
const DATABASE_URL: &str = "mysql://testuser:testpass@localhost:3306/testdb";
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
      memory: { topic: "sqlx-mysql-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      sqlx:
        url: "mysql://testuser:testpass@localhost:3306/testdb"
        table: "messages"

  sqlx_to_memory:
    concurrency: 4
    batch_size: 1024
    input:
      sqlx:
        url: "mysql://testuser:testpass@localhost:3306/testdb"
        table: "messages"
        delete_after_read: true
        polling_interval_ms: 20
    output:
      memory: { topic: "sqlx-mysql-out", capacity: {out_capacity} }
"#;

pub async fn test_mysql_performance_pipeline() {
    run_mysql_test(|config_yaml| async move {
        run_performance_pipeline_test_named("sqlx", "mysql", &config_yaml, PERF_TEST_MESSAGE_COUNT)
            .await;
    })
    .await;
}

/// Creates the checkpoint meta table and round-trips a cursor value through it.
///
/// Shared with the MariaDB suite. `last_value` is a reserved word on MySQL 8 (the
/// `LAST_VALUE` window function), so unquoted DDL fails there with ERROR 1064 — this is
/// the regression guard for the per-driver identifier quoting.
pub async fn assert_sql_checkpoint_round_trip(database_url: &str, source_name: &str) {
    let cursor_id = format!("cp-{}", fast_uuid_v7::gen_id());
    let backend = mq_bridge::checkpoint::parse_checkpoint_store(database_url)
        .expect("parse sqlx checkpoint_store");

    let store =
        mq_bridge::checkpoint::build_external_store(backend.clone(), source_name, &cursor_id)
            .await
            .expect("create checkpoint meta table");
    assert_eq!(store.load().await.unwrap(), None, "fresh cursor is empty");
    store.save("42").await.expect("save cursor");
    assert_eq!(store.load().await.unwrap(), Some("42".to_string()));
    store.save("99").await.expect("overwrite cursor");

    // A freshly built store for the same cursor sees the persisted value.
    let reopened = mq_bridge::checkpoint::build_external_store(backend, source_name, &cursor_id)
        .await
        .expect("rebuild checkpoint store");
    assert_eq!(reopened.load().await.unwrap(), Some("99".to_string()));
}

/// Regression: a source table with types the sqlx `Any` driver cannot map — `DECIMAL`
/// (`NewDecimal`), `TIMESTAMP`/`DATETIME`, `JSON`, `TINYINT` — must be readable by the cursor
/// reader instead of failing every read forever. The `::text` auto-cast originally shipped for
/// PostgreSQL only; this is the MySQL/MariaDB half (`CAST(col AS CHAR)`).
///
/// Shared with the MariaDB suite.
pub async fn assert_cursor_reads_unmappable_types(database_url: &str, cursor_id: &str) {
    use mq_bridge::traits::MessageConsumer;
    use sqlx::AnyPool;

    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(database_url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS typed_events")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE typed_events (\
            id BIGINT PRIMARY KEY, \
            name VARCHAR(64), \
            amount DECIMAL(10,2), \
            active TINYINT(1), \
            ratio DOUBLE, \
            payload JSON, \
            created_at TIMESTAMP)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Insert via literal SQL: binding DECIMAL/TIMESTAMP/JSON through `Any` is itself unsupported.
    for i in 1..=3 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO typed_events VALUES ({i}, 'row{i}', {i}.50, 1, {i}.25, \
             '{{\"k\": {i}}}', TIMESTAMP '2024-01-0{i} 12:00:00')"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }

    let config = mq_bridge::models::SqlxConfig {
        url: database_url.to_string(),
        table: "typed_events".to_string(),
        cursor_column: Some("id".to_string()),
        // Unique per run: a persisted cursor from a previous run would resume past all
        // 3 rows and return an empty batch.
        cursor_id: Some(format!("{cursor_id}-{}", fast_uuid_v7::gen_id())),
        ..Default::default()
    };

    let mut reader = mq_bridge::endpoints::sqlx::SqlxCursorReader::new(&config)
        .await
        .unwrap();
    let batch = reader.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 3, "all 3 rows must be read");

    let rows: Vec<serde_json::Value> = batch
        .messages
        .iter()
        .map(|m| serde_json::from_slice(&m.payload).unwrap())
        .collect();

    // The unmappable columns arrive as (cast-to-CHAR) strings, not null.
    let created = rows[0].get("created_at").and_then(|v| v.as_str());
    assert!(
        created.is_some_and(|s| s.starts_with("2024-01-01")),
        "created_at should be a timestamp string, got {:?}",
        rows[0].get("created_at")
    );
    assert_eq!(
        rows[0].get("amount").and_then(|v| v.as_str()),
        Some("1.50"),
        "decimal -> string"
    );
    assert!(
        rows[0].get("payload").unwrap().is_string(),
        "json -> string"
    );
    assert!(
        rows[0].get("active").unwrap().is_string(),
        "tinyint is not Any-mappable either -> string"
    );
    // Natively mappable types keep their JSON types.
    assert!(rows[0].get("id").unwrap().is_i64(), "bigint stays numeric");
    assert!(rows[0].get("ratio").unwrap().is_f64(), "double stays f64");
    assert!(
        rows[0].get("name").unwrap().is_string(),
        "varchar stays str"
    );

    (batch.commit)(vec![mq_bridge::traits::MessageDisposition::Ack; 3])
        .await
        .unwrap();
}

pub async fn test_mysql_cursor_unmappable_types() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        assert_cursor_reads_unmappable_types(DATABASE_URL, "mysql-types").await;
    })
    .await;
}

pub async fn test_mysql_checkpoint_table() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        assert_sql_checkpoint_round_trip(DATABASE_URL, "mysql").await;
    })
    .await;
}

async fn run_mysql_test<F, Fut>(runner: F)
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

pub async fn test_mysql_performance_direct() {
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
            "SQLx (MySQL)",
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

pub async fn test_mysql_status() {
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

        println!("[MySQL] Checking initial status...");
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
        println!("[MySQL] Initial status check OK.");

        controller.stop_service("mysql");
        println!("[MySQL] Service 'mysql' stopped. Waiting for disconnect detection...");

        let start = std::time::Instant::now();
        loop {
            if !publisher.status().await.healthy && !consumer.status().await.healthy {
                println!("[MySQL] Disconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[MySQL] Timeout waiting for disconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }

        controller.start_service("mysql");
        println!("[MySQL] Service 'mysql' started. Waiting for reconnect...");

        let start = std::time::Instant::now();
        loop {
            if publisher.status().await.healthy && consumer.status().await.healthy {
                println!("[MySQL] Reconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[MySQL] Timeout waiting for reconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }
        println!("[MySQL] Status test successful.");
    })
    .await;
}
