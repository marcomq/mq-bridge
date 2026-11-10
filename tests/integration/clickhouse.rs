#![allow(dead_code)]
#![cfg(feature = "clickhouse")]

use mq_bridge::endpoints::clickhouse::{ClickHouseCursorReader, ClickHousePublisher};
use mq_bridge::models::ClickHouseConfig;
use mq_bridge::test_utils::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{
    MessageConsumer, MessageDisposition, MessagePublisher, PublisherError, Sent,
};
use mq_bridge::CanonicalMessage;

const DOCKER_COMPOSE_FILE: &str = "tests/integration/docker-compose/clickhouse.yml";
const CH_URL: &str = "http://localhost:8123";
const CH_USER: &str = "testuser";
const CH_PASS: &str = "testpass";

fn base_config() -> ClickHouseConfig {
    ClickHouseConfig {
        url: CH_URL.into(),
        username: Some(CH_USER.into()),
        password: Some(CH_PASS.into()),
        ..Default::default()
    }
}

/// Run raw SQL against the ClickHouse HTTP interface (no bindings needed for setup/asserts).
async fn ch_exec(sql: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(CH_URL)
        .header("X-ClickHouse-User", CH_USER)
        .header("X-ClickHouse-Key", CH_PASS)
        .body(sql.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "ClickHouse exec failed ({status}): {text}\nSQL: {sql}"
    );
    text
}

/// End-to-end: sink a batch, read it back non-destructively via the cursor source, and verify a
/// typed column-mapping insert. Exercises the two ways payloads map to rows (whole-object + mapped).
pub async fn test_clickhouse_roundtrip() {
    setup_logging();
    run_test_with_docker(DOCKER_COMPOSE_FILE, || async {
        // Fresh table with a monotonic id for cursor paging.
        ch_exec("DROP TABLE IF EXISTS ch_events").await;
        ch_exec("CREATE TABLE ch_events (id UInt64, name String) ENGINE = MergeTree ORDER BY id")
            .await;

        // --- Sink: default JSONEachRow insert (payload is a table-shaped JSON object) ---
        let pub_cfg = ClickHouseConfig {
            table: "ch_events".into(),
            ..base_config()
        };
        let publisher = ClickHousePublisher::new(&pub_cfg).await.unwrap();
        let n: u64 = 50;
        let batch: Vec<CanonicalMessage> = (1..=n)
            .map(|i| {
                CanonicalMessage::new(format!(r#"{{"id":{i},"name":"msg-{i}"}}"#).into_bytes(), None)
            })
            .collect();
        publisher.send_batch(batch).await.unwrap();

        let count = ch_exec("SELECT count() FROM ch_events").await;
        assert_eq!(count.trim(), n.to_string(), "all rows inserted");

        // --- Source: non-destructive, resumable cursor read over `id` ---
        let src_cfg = ClickHouseConfig {
            table: "ch_events".into(),
            cursor_column: Some("id".into()),
            ..base_config()
        };
        let mut reader = ClickHouseCursorReader::new(&src_cfg).await.unwrap();

        let mut total = 0usize;
        let mut last_seen: u64 = 0;
        loop {
            let b = reader.receive_batch(20).await.unwrap();
            if b.messages.is_empty() {
                break;
            }
            for m in &b.messages {
                let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap();
                let id = v["id"].as_u64().unwrap();
                last_seen += 1;
                assert_eq!(id, last_seen, "rows arrive in ascending id order");
                assert_eq!(v["name"], format!("msg-{id}"));
            }
            total += b.messages.len();
            let acks = vec![MessageDisposition::Ack; b.messages.len()];
            (b.commit)(acks).await.unwrap();
        }
        assert_eq!(total, n as usize, "cursor read returns every row exactly once");

        // Non-destructive: the source table is untouched.
        let count2 = ch_exec("SELECT count() FROM ch_events").await;
        assert_eq!(count2.trim(), n.to_string(), "cursor read is non-destructive");

        // --- Column-mapping sink into a typed table ---
        ch_exec("DROP TABLE IF EXISTS ch_orders").await;
        ch_exec(
            "CREATE TABLE ch_orders (sku String, qty UInt32, cust String) ENGINE = MergeTree ORDER BY sku",
        )
        .await;
        let mut cols = std::collections::BTreeMap::new();
        cols.insert("sku".to_string(), "${payload:sku}".to_string());
        cols.insert("qty".to_string(), "${payload:qty}".to_string());
        cols.insert("cust".to_string(), "${metadata:cust}".to_string());
        let map_cfg = ClickHouseConfig {
            table: "ch_orders".into(),
            columns: Some(cols),
            ..base_config()
        };
        let map_pub = ClickHousePublisher::new(&map_cfg).await.unwrap();
        let mut msg = CanonicalMessage::new(br#"{"sku":"widget","qty":7}"#.to_vec(), None);
        msg.set_metadata("cust", "c-1");
        map_pub.send(msg).await.unwrap();

        let got = ch_exec("SELECT sku, qty, cust FROM ch_orders FORMAT JSONEachRow").await;
        assert!(
            got.contains("\"sku\":\"widget\"")
                && got.contains("\"qty\":7")
                && got.contains("\"cust\":\"c-1\""),
            "mapped row mismatch: {got}"
        );

        // --- lookup_query: read-by-key for `lookup` ---
        let sel_cfg = ClickHouseConfig {
            table: "ch_events".into(),
            lookup_query: Some(
                "SELECT id, name FROM ch_events WHERE id = ${payload:id} AND name = ${payload:name} LIMIT 1"
                    .into(),
            ),
            ..base_config()
        };
        let sel_pub = ClickHousePublisher::new(&sel_cfg).await.unwrap();
        let hit = sel_pub
            .send(CanonicalMessage::new(br#"{"id":7,"name":"msg-7"}"#.to_vec(), None))
            .await
            .unwrap();
        let Sent::Response(hit) = hit else {
            panic!("lookup_query must answer with a response")
        };
        let row: serde_json::Value = serde_json::from_slice(&hit.payload).unwrap();
        assert_eq!(row, serde_json::json!({"id": 7, "name": "msg-7"}));
        assert_eq!(hit.get_metadata("clickhouse.found").unwrap(), "true");

        let miss = sel_pub
            .send(CanonicalMessage::new(br#"{"id":999,"name":"it's\n\\x"}"#.to_vec(), None))
            .await
            .unwrap();
        let Sent::Response(miss) = miss else {
            panic!("lookup_query must answer with a response")
        };
        assert!(miss.payload.is_empty());
        assert_eq!(miss.get_metadata("clickhouse.found").unwrap(), "false");

        let in_cfg = ClickHouseConfig {
            lookup_query: Some("SELECT id, name FROM ch_events WHERE id IN (${payload:id})".into()),
            ..sel_cfg.clone()
        };
        let in_pub = ClickHousePublisher::new(&in_cfg).await.unwrap();
        let ask = |id: i64| CanonicalMessage::new(format!(r#"{{"id":{id}}}"#).into_bytes(), None);
        let answers = in_pub
            .lookup_batch(&[ask(7), ask(999), ask(3), ask(7)])
            .await
            .expect("an IN query batches")
            .unwrap();
        let names: Vec<_> = answers
            .iter()
            .map(|a| a.as_ref().map(|r| r["name"].clone()))
            .collect();
        assert_eq!(
            names,
            vec![Some("msg-7".into()), None, Some("msg-3".into()), Some("msg-7".into())]
        );

        let bad_cfg = ClickHouseConfig {
            lookup_query: Some("SELECT nope FROM ch_events WHERE id = ${payload:id}".into()),
            ..sel_cfg
        };
        let bad = ClickHousePublisher::new(&bad_cfg)
            .await
            .unwrap()
            .send(CanonicalMessage::new(br#"{"id":1}"#.to_vec(), None))
            .await;
        assert!(
            matches!(bad, Err(PublisherError::NonRetryable(_))),
            "a query error must not be retried: {bad:?}"
        );

        println!("[ClickHouse] round-trip + cursor + column-mapping + lookup OK");
    })
    .await;
}

/// Publisher and cursor-reader report health, then unhealthy when the server stops.
pub async fn test_clickhouse_status() {
    use mq_bridge::test_utils::run_test_with_docker_controller;
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(DOCKER_COMPOSE_FILE, |controller| async move {
        ch_exec("DROP TABLE IF EXISTS ch_status").await;
        ch_exec("CREATE TABLE ch_status (id UInt64) ENGINE = MergeTree ORDER BY id").await;

        let cfg = ClickHouseConfig {
            table: "ch_status".into(),
            cursor_column: Some("id".into()),
            ..base_config()
        };
        let publisher = ClickHousePublisher::new(&cfg).await.unwrap();
        let consumer = ClickHouseCursorReader::new(&cfg).await.unwrap();

        sleep(Duration::from_secs(1)).await;
        assert!(
            publisher.status().await.healthy,
            "publisher healthy initially"
        );
        assert!(
            consumer.status().await.healthy,
            "consumer healthy initially"
        );

        controller.stop_service("clickhouse");
        let start = std::time::Instant::now();
        loop {
            if !publisher.status().await.healthy && !consumer.status().await.healthy {
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("[ClickHouse] Timeout waiting for disconnect.");
            }
            sleep(Duration::from_secs(1)).await;
        }
        println!("[ClickHouse] Status test successful.");
    })
    .await;
}
