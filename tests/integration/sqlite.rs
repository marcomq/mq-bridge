#![allow(dead_code)]
#![cfg(feature = "sqlx")]

use mq_bridge::endpoints::sqlx::{SqlxConsumer, SqlxPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test,
    setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::sync::Arc;

const TABLE_NAME: &str = "messages";

fn db_file_path(id: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    // Using unique file names per test prevents lock contention and race conditions
    // when tests are run in parallel.
    p.push(format!("mq_bridge_test_sqlite_{}.db", id));
    p
}

fn db_url(id: &str) -> String {
    format!("sqlite://{}?mode=rwc", db_file_path(id).display())
}

async fn setup_db(id: &str) {
    let db_path = db_file_path(id);
    if let Err(error) = std::fs::remove_file(&db_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %db_path.display(), error = %error, "failed to remove sqlite db file");
        }
    }
    if let Some(parent) = db_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), error = %error, "failed to create sqlite db directory");
        }
    }
    if let Err(error) = std::fs::File::create(&db_path) {
        tracing::warn!(path = %db_path.display(), error = %error, "failed to create sqlite db file");
    }

    // Manually enable WAL mode to ensure high performance during tests.
    // journal_mode=WAL is persistent in the database file header.
    let url = db_url(id);
    match <sqlx::SqliteConnection as sqlx::Connection>::connect(&url).await {
        Ok(mut conn) => {
            for pragma in ["PRAGMA journal_mode=WAL;", "PRAGMA busy_timeout=5000;"] {
                if let Err(error) = sqlx::query(pragma).execute(&mut conn).await {
                    tracing::warn!(url = %url, error = %error, pragma, "failed to set sqlite pragma");
                }
            }
        }
        Err(error) => {
            tracing::warn!(url = %url, error = %error, "failed to connect to sqlite db");
        }
    }

    let config = mq_bridge::models::SqlxConfig {
        url: db_url(id),
        table: TABLE_NAME.to_string(),
        auto_create_table: true,
        ..Default::default()
    };
    let _publisher = SqlxPublisher::new(&config).await.unwrap();
}

const CONFIG_YAML: &str = r#"
routes:
    memory_to_sqlite:
        concurrency: 4
        batch_size: 1024
        input:
            memory: { topic: "sqlx-sqlite-in", enable_nack: true }
        output:
            middlewares:
                - retry:
                    max_attempts: 10
                    initial_interval_ms: 100
                    max_interval_ms: 1000
            sqlx:
                url: "{db_url}"
                table: "messages"

    sqlite_to_memory:
        concurrency: 4
        batch_size: 1024
        input:
            sqlx:
                url: "{db_url}"
                table: "messages"
                delete_after_read: true
                polling_interval_ms: 20
        output:
            memory: { topic: "sqlx-sqlite-out", capacity: {out_capacity} }
"#;

pub async fn test_sqlite_pipeline() {
    setup_logging();
    setup_db("pipeline").await;
    let config_yaml = CONFIG_YAML
        .replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        )
        .replace("{db_url}", &db_url("pipeline"));
    run_pipeline_test("sqlite", &config_yaml).await;
}

pub async fn test_sqlite_performance_pipeline() {
    setup_logging();
    setup_db("perf_pipeline").await;
    let config_yaml = CONFIG_YAML
        .replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        )
        .replace("{db_url}", &db_url("perf_pipeline"));
    run_performance_pipeline_test("sqlite", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
}

pub async fn test_sqlite_performance_direct() {
    setup_logging();
    setup_db("direct").await;
    let config = mq_bridge::models::SqlxConfig {
        url: db_url("direct"),
        table: TABLE_NAME.to_string(),
        delete_after_read: true,
        polling_interval_ms: Some(1),
        auto_create_table: true,
        ..Default::default()
    };

    let result = run_direct_perf_test(
        "SQLx (SQLite)",
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
}

pub async fn test_sqlite_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};

    setup_logging();
    setup_db("status").await;
    let config = mq_bridge::models::SqlxConfig {
        url: db_url("status"),
        table: TABLE_NAME.to_string(),
        ..Default::default()
    };

    let publisher = SqlxPublisher::new(&config).await.unwrap();
    let consumer = SqlxConsumer::new(&config).await.unwrap();

    let pub_status = publisher.status().await;
    let con_status = consumer.status().await;
    assert!(pub_status.healthy);
    assert!(con_status.healthy);
}
