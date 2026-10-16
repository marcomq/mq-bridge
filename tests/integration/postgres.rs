#![allow(dead_code)]
#![cfg(feature = "sqlx")]

use mq_bridge::endpoints::sqlx::{SqlxConsumer, SqlxPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_direct_perf_test, run_performance_pipeline_test_named,
    run_test_with_docker, run_test_with_docker_controller, setup_logging, PERF_TEST_MESSAGE_COUNT,
};
use std::sync::Arc;

const DOCKER_COMPOSE_FILE: &str = "tests/integration/docker-compose/postgres.yml";
const DATABASE_URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
const TABLE_NAME: &str = "messages";

async fn setup_db() {
    let config = mq_bridge::models::SqlxConfig {
        url: DATABASE_URL.to_string(),
        table: TABLE_NAME.to_string(),
        auto_create_table: true,
        ..Default::default()
    };
    // This will trigger table creation
    let _publisher = SqlxPublisher::new(&config).await.unwrap();
}

const CONFIG_YAML: &str = r#"
routes:
  memory_to_sqlx:
    concurrency: 4
    batch_size: 1024
    input:
      memory: { topic: "sqlx-test-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      sqlx:
        url: "postgres://testuser:testpass@localhost:5432/testdb"
        table: "messages"
        min_connections: 2

  sqlx_to_memory:
    concurrency: 4
    batch_size: 1024
    input:
      sqlx:
        url: "postgres://testuser:testpass@localhost:5432/testdb"
        table: "messages"
        delete_after_read: true
        polling_interval_ms: 20
        min_connections: 2
    output:
      memory: { topic: "sqlx-test-out", capacity: {out_capacity} }
"#;

pub async fn test_postgres_performance_pipeline() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        setup_db().await;
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test_named(
            "sqlx",
            "postgres",
            &config_yaml,
            PERF_TEST_MESSAGE_COUNT,
        )
        .await;
    })
    .await;
}

pub async fn test_postgres_performance_direct() {
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
            "SQLx (Postgres)",
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

pub async fn test_postgres_multicolumn() {
    use mq_bridge::traits::MessagePublisher;
    use mq_bridge::CanonicalMessage;
    use sqlx::{AnyPool, Row};

    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(DATABASE_URL).await.unwrap();
        sqlx::query("DROP TABLE IF EXISTS orders")
            .execute(&pool)
            .await
            .unwrap();
        // Typed columns: string-binding everything would fail on `qty INTEGER` / `price DOUBLE PRECISION`.
        sqlx::query(
            "CREATE TABLE orders (sku TEXT, qty INTEGER, price DOUBLE PRECISION, cust TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let config = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: "orders".to_string(),
            insert_query: Some(
                "INSERT INTO orders (sku, qty, price, cust) VALUES (${payload:sku}, ${payload:qty}, ${payload:price}, ${metadata:cust})"
                    .to_string(),
            ),
            ..Default::default()
        };
        let publisher = SqlxPublisher::new(&config).await.unwrap();

        let mut msg =
            CanonicalMessage::new(br#"{"sku":"abc","qty":7,"price":1.5}"#.to_vec(), None);
        msg.metadata.insert("cust".to_string(), "c1".to_string());
        publisher.send(msg).await.unwrap();

        // Batch of 2 with typed columns and per-row metadata.
        let mut batch = Vec::new();
        for i in 0..2 {
            let mut m = CanonicalMessage::new(
                format!(r#"{{"sku":"s{i}","qty":{},"price":{}}}"#, i * 10, i as f64 + 0.5)
                    .into_bytes(),
                None,
            );
            m.metadata.insert("cust".to_string(), format!("b{i}"));
            batch.push(m);
        }
        publisher.send_batch(batch).await.unwrap();

        let row = sqlx::query("SELECT sku, qty, price, cust FROM orders WHERE sku = 'abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let sku: String = row.get("sku");
        let qty: i32 = row.get("qty");
        let price: f64 = row.get("price");
        let cust: String = row.get("cust");
        assert_eq!(sku, "abc");
        assert_eq!(qty, 7);
        assert_eq!(price, 1.5);
        assert_eq!(cust, "c1");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3, "1 single + 2 batch rows expected");

        println!("[Postgres] Multi-column typed insert test successful.");
    })
    .await;
}

/// A row the table rejects fails alone, whichever way the batch is written.
pub async fn test_postgres_rejected_row_fails_alone() {
    use mq_bridge::outcomes::SentBatch;
    use mq_bridge::traits::MessagePublisher;
    use mq_bridge::CanonicalMessage;
    use sqlx::AnyPool;

    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(DATABASE_URL).await.unwrap();
        let insert = "INSERT INTO rejects (k, n) VALUES (${payload:k}, ${payload:n})";
        let base = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: "rejects".to_string(),
            ..Default::default()
        };
        let variants = [
            (
                "insert",
                mq_bridge::models::SqlxConfig {
                    insert_query: Some(insert.to_string()),
                    ..base.clone()
                },
            ),
            (
                "copy",
                mq_bridge::models::SqlxConfig {
                    insert_query: Some(insert.to_string()),
                    bulk_copy: true,
                    ..base.clone()
                },
            ),
            (
                "auto",
                mq_bridge::models::SqlxConfig {
                    columns: Some(mq_bridge::models::SqlColumns::Auto),
                    ..base.clone()
                },
            ),
            (
                "auto copy",
                mq_bridge::models::SqlxConfig {
                    columns: Some(mq_bridge::models::SqlColumns::Auto),
                    bulk_copy: true,
                    ..base.clone()
                },
            ),
        ];
        for (name, config) in variants {
            for sql in [
                "DROP TABLE IF EXISTS rejects",
                "CREATE TABLE rejects (k TEXT PRIMARY KEY, n INTEGER CHECK (n < 100))",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            let publisher = SqlxPublisher::new(&config).await.unwrap();
            let message = |body: &str| CanonicalMessage::new(body.as_bytes().to_vec(), None);
            let rejected = message(r#"{"k":"b","n":500}"#);
            let rejected_id = rejected.message_id;
            let sent = publisher
                .send_batch(vec![
                    message(r#"{"k":"a","n":1}"#),
                    rejected,
                    message(r#"{"k":"c","n":3}"#),
                ])
                .await
                .unwrap_or_else(|e| panic!("{name}: the whole batch failed: {e}"));
            let SentBatch::Partial { failed, .. } = sent else {
                panic!("{name}: expected one failed message");
            };
            assert_eq!(failed.len(), 1, "{name}");
            assert_eq!(failed[0].0.message_id, rejected_id, "{name}");
            let keys: Vec<String> = sqlx::query_scalar("SELECT k FROM rejects ORDER BY k")
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(keys, ["a", "c"], "{name}");
        }
    })
    .await;
}

/// `columns: auto` sends one statement text with bind types that change from record to record.
pub async fn test_postgres_auto_columns_changing_bind_types() {
    use mq_bridge::traits::MessagePublisher;
    use mq_bridge::CanonicalMessage;
    use sqlx::AnyPool;

    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(DATABASE_URL).await.unwrap();
        sqlx::query("DROP TABLE IF EXISTS prices")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE prices (id INTEGER PRIMARY KEY, price NUMERIC)")
            .execute(&pool)
            .await
            .unwrap();

        let config = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: "prices".to_string(),
            columns: Some(mq_bridge::models::SqlColumns::Auto),
            max_connections: Some(1),
            ..Default::default()
        };
        let publisher = SqlxPublisher::new(&config).await.unwrap();
        for record in [
            r#"{"id":1,"price":5}"#,
            r#"{"id":2,"price":5.5}"#,
            r#"{"id":3,"price":"8.25"}"#,
        ] {
            let msg = CanonicalMessage::new(record.as_bytes().to_vec(), None);
            publisher.send(msg).await.unwrap();
        }

        let prices: Vec<String> = sqlx::query_scalar("SELECT price::text FROM prices ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(prices, ["5", "5.5", "8.25"]);
    })
    .await;
}

/// Regression: a source table with a `TIMESTAMPTZ` column (plus other types the sqlx `Any`
/// driver cannot map: `NUMERIC`, `TEXT[]`) must be readable by the cursor reader instead of
/// failing every read forever. Reads 7 columns and writes the rows to a JSON file.
pub async fn test_postgres_cursor_timestamptz_to_json() {
    use mq_bridge::traits::MessageConsumer;
    use sqlx::AnyPool;

    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(DATABASE_URL).await.unwrap();
        sqlx::query("DROP TABLE IF EXISTS events")
            .execute(&pool)
            .await
            .unwrap();
        // 7 columns; created_at/amount/tags are types the `Any` driver cannot decode directly.
        sqlx::query(
            "CREATE TABLE events (\
                id BIGINT PRIMARY KEY, \
                name TEXT, \
                amount NUMERIC(10,2), \
                active BOOLEAN, \
                ratio DOUBLE PRECISION, \
                tags TEXT[], \
                created_at TIMESTAMPTZ)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Insert via literal SQL: binding NUMERIC/TIMESTAMPTZ/arrays through `Any` is itself
        // unsupported, so the values live in the statement text.
        for i in 1..=3 {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO events VALUES ({i}, 'row{i}', {i}.50, true, {i}.25, \
                 ARRAY['a','b'], TIMESTAMPTZ '2024-01-0{i} 12:00:00+00')"
            )))
            .execute(&pool)
            .await
            .unwrap();
        }

        let config = mq_bridge::models::SqlxConfig {
            url: DATABASE_URL.to_string(),
            table: "events".to_string(),
            cursor_column: Some("id".to_string()),
            cursor_id: Some("ts-json".to_string()),
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

        // The timestamptz column must be present as a (cast-to-text) string, not null.
        let created = rows[0].get("created_at").and_then(|v| v.as_str());
        assert!(
            created.is_some_and(|s| s.starts_with("2024-01-01")),
            "created_at should be an RFC-ish string, got {:?}",
            rows[0].get("created_at")
        );
        // NUMERIC arrives as a string, TEXT[] as a JSON array; native types keep their JSON types.
        assert!(
            rows[0].get("amount").unwrap().is_string(),
            "numeric -> string"
        );
        assert_eq!(
            rows[0].get("tags"),
            Some(&serde_json::json!(["a", "b"])),
            "array -> JSON array"
        );
        assert!(rows[0].get("id").unwrap().is_i64(), "bigint stays numeric");
        assert!(
            rows[0].get("active").unwrap().is_boolean(),
            "bool stays bool"
        );

        // Write the read rows out to a JSON file, mirroring `copy --from postgres ... --to file`.
        let out_path = std::env::temp_dir().join(format!("mqb_events_{}.json", std::process::id()));
        std::fs::write(&out_path, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
        let reread: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
        assert_eq!(reread.len(), 3);
        let _ = std::fs::remove_file(&out_path);

        (batch.commit)(vec![mq_bridge::traits::MessageDisposition::Ack; 3])
            .await
            .unwrap();

        println!(
            "[Postgres] timestamptz cursor -> JSON test successful ({} rows written to file).",
            reread.len()
        );
    })
    .await;
}

pub async fn test_postgres_status() {
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

        println!("[Postgres] Checking initial status...");
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
        println!("[Postgres] Initial status check OK.");

        controller.stop_service("postgres");
        println!("[Postgres] Service 'postgres' stopped. Waiting for disconnect detection...");

        let start = std::time::Instant::now();
        loop {
            if !publisher.status().await.healthy && !consumer.status().await.healthy {
                println!("[Postgres] Disconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[Postgres] Timeout waiting for disconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }

        controller.start_service("postgres");
        println!("[Postgres] Service 'postgres' started. Waiting for reconnect...");

        let start = std::time::Instant::now();
        loop {
            if publisher.status().await.healthy && consumer.status().await.healthy {
                println!("[Postgres] Reconnect detected.");
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[Postgres] Timeout waiting for reconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }
        println!("[Postgres] Status test successful.");
    })
    .await;
}
