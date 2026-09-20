//! Postgres logical-replication CDC integration + restart-safety tests.
//!
//! Requires Docker (a Postgres started with `wal_level=logical`, see
//! `docker-compose/postgres_cdc.yml`). Run with:
//!   `cargo test --test integration_test --features "test-utils postgres-cdc" -- --include-ignored postgres_cdc`
#![cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#![allow(dead_code)]

use mq_bridge::endpoints::postgres::PostgresCdcConsumer;
use mq_bridge::endpoints::sqlx::SqlxPublisher;
use mq_bridge::models::{PostgresCdcConfig, SqlxConfig};
use mq_bridge::sqlx::{Connection, PgConnection};
use mq_bridge::test_utils::{
    add_performance_result, run_performance_pipeline_test_named, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, PerformanceResult, PERF_TEST_MESSAGE_COUNT,
};
use mq_bridge::traits::{MessageConsumer, MessageDisposition};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const COMPOSE: &str = "tests/integration/docker-compose/postgres_cdc.yml";
const URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
const PUBLICATION: &str = "mqb_cdc_pub";

fn cfg(slot: &str) -> PostgresCdcConfig {
    PostgresCdcConfig {
        url: URL.to_string(),
        publication: PUBLICATION.to_string(),
        consume: None,
        source_metadata: false,
        slot_name: slot.to_string(),
        create_slot: true,
        create_publication: false,
        publication_tables: Vec::new(),
        temporary_slot: false,
        cursor_id: None,
        checkpoint_store: None,
        status_interval_ms: 500,
        tls: Default::default(),
    }
}

/// Connect a plain SQL connection, retrying briefly (used after a restart).
async fn connect_retry() -> PgConnection {
    for _ in 0..30 {
        if let Ok(conn) = PgConnection::connect(URL).await {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    PgConnection::connect(URL)
        .await
        .expect("connect to postgres")
}

/// Drop any leftover slot/publication/table and recreate a clean schema.
/// sqlx 0.9's `&mut PgConnection` executor requires `'static` query text, so we
/// use literal statements (the table/publication names are compile-time
/// constants matching `PUBLICATION`) and bind only the dynamic slot name.
async fn reset_schema(slot: &str) {
    let mut conn = connect_retry().await;
    // A slot may linger from a previous run; drop it if present (ignore errors).
    let _ = sqlx::query(
        "SELECT pg_drop_replication_slot($1) FROM pg_replication_slots WHERE slot_name = $1",
    )
    .bind(slot)
    .execute(&mut conn)
    .await;
    let _ = sqlx::query("DROP PUBLICATION IF EXISTS mqb_cdc_pub")
        .execute(&mut conn)
        .await;
    sqlx::query("DROP TABLE IF EXISTS cdc_users")
        .execute(&mut conn)
        .await
        .expect("drop table");
    sqlx::query("CREATE TABLE cdc_users (id INT PRIMARY KEY, name TEXT)")
        .execute(&mut conn)
        .await
        .expect("create table");
    sqlx::query("CREATE PUBLICATION mqb_cdc_pub FOR TABLE cdc_users")
        .execute(&mut conn)
        .await
        .expect("create publication");
}

async fn insert_rows(range: std::ops::RangeInclusive<i32>) {
    let mut conn = connect_retry().await;
    for id in range {
        sqlx::query("INSERT INTO cdc_users (id, name) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("name-{id}"))
            .execute(&mut conn)
            .await
            .expect("insert row");
    }
}

/// Drain change events until `want` rows have been collected, acking each batch.
/// Returns (operation, id) pairs. Fails the test if it stalls.
async fn drain_ids(consumer: &mut PostgresCdcConsumer, want: usize) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    while out.len() < want {
        let batch = tokio::time::timeout(Duration::from_secs(20), consumer.receive_batch(1024))
            .await
            .expect("timed out waiting for CDC events")
            .expect("receive_batch failed");
        let n = batch.messages.len();
        for msg in &batch.messages {
            let op = msg
                .metadata
                .get("postgres.operation")
                .cloned()
                .unwrap_or_default();
            let body: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap_or_default();
            let id = body.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
            out.push((op, id));
        }
        (batch.commit)(vec![MessageDisposition::Ack; n])
            .await
            .expect("commit (ack) failed");
    }
    out
}

/// Basic pipeline: inserts are captured as `insert` change events with flat rows.
pub async fn test_postgres_cdc_pipeline() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let slot = "mqb_cdc_basic_slot";
        reset_schema(slot).await;
        // Create the consumer (and slot) BEFORE inserting so the changes are captured.
        let mut consumer = PostgresCdcConsumer::new(&cfg(slot))
            .await
            .expect("create CDC consumer");

        insert_rows(1..=100).await;
        let seen = drain_ids(&mut consumer, 100).await;

        assert!(seen.iter().all(|(op, _)| op == "insert"), "all inserts");
        let ids: BTreeSet<i64> = seen.iter().map(|(_, id)| *id).collect();
        for id in 1..=100 {
            assert!(ids.contains(&(id as i64)), "row {id} must be captured");
        }
    })
    .await;
}

/// `temporary_slot: true` must actually deliver changes, and must leave no slot
/// behind once the route stops. A Postgres *temporary* slot dies with the session
/// that created it, so the option is implemented as drop-on-stop instead.
pub async fn test_postgres_cdc_temporary_slot() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let slot = "mqb_cdc_temp_slot";
        reset_schema(slot).await;
        let mut config = cfg(slot);
        config.temporary_slot = true;

        let mut consumer = PostgresCdcConsumer::new(&config)
            .await
            .expect("create CDC consumer");
        insert_rows(1..=10).await;
        let seen = drain_ids(&mut consumer, 10).await;
        assert_eq!(seen.len(), 10, "temporary_slot route must deliver rows");

        // Graceful stop runs the teardown that drops the slot; the extra `drop` then hits
        // the fallback in `Drop`, which must be a no-op because teardown is single-shot.
        consumer.close().await.expect("clean close");
        drop(consumer);
        let mut conn = connect_retry().await;
        let mut remaining = -1i64;
        for _ in 0..40 {
            remaining = sqlx::query_scalar(
                "SELECT count(*) FROM pg_replication_slots WHERE slot_name = $1",
            )
            .bind(slot)
            .fetch_one(&mut conn)
            .await
            .expect("count slots");
            if remaining == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(
            remaining, 0,
            "temporary slot '{slot}' must be dropped on stop"
        );
    })
    .await;
}

/// Read the slot's durable `confirmed_flush_lsn` as a `pg_lsn` string.
async fn confirmed_flush_lsn(slot: &str) -> String {
    let mut conn = connect_retry().await;
    sqlx::query_scalar::<_, String>(
        "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
    )
    .bind(slot)
    .fetch_one(&mut conn)
    .await
    .expect("read confirmed_flush_lsn")
}

/// `X/Y` hex LSN -> u64, for comparing positions.
fn lsn_to_u64(lsn: &str) -> u64 {
    let (hi, lo) = lsn.split_once('/').expect("pg_lsn has the form X/Y");
    (u64::from_str_radix(hi, 16).expect("lsn high") << 32)
        | u64::from_str_radix(lo, 16).expect("lsn low")
}

/// After a clean stop, the slot's `confirmed_flush_lsn` must cover the last delivered and
/// acked change. The teardown that flushes it runs in `on_disconnect_hook` (awaited by the
/// route while the runtime is healthy); doing it from `Drop` instead always failed with
/// "control-plane connect failed: task was cancelled", so the final flush never landed and
/// the confirmed position lagged the last transaction.
pub async fn test_postgres_cdc_confirms_lsn_on_clean_stop() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let slot = "mqb_cdc_flush_slot";
        reset_schema(slot).await;
        let mut consumer = PostgresCdcConsumer::new(&cfg(slot))
            .await
            .expect("create CDC consumer");

        insert_rows(1..=20).await;
        let mut last_lsn = 0u64;
        let mut seen = 0usize;
        while seen < 20 {
            let batch = tokio::time::timeout(Duration::from_secs(20), consumer.receive_batch(1024))
                .await
                .expect("timed out waiting for CDC events")
                .expect("receive_batch failed");
            let n = batch.messages.len();
            for msg in &batch.messages {
                let lsn = msg
                    .metadata
                    .get("postgres.lsn")
                    .expect("each change carries postgres.lsn");
                last_lsn = last_lsn.max(lsn_to_u64(lsn));
            }
            seen += n;
            (batch.commit)(vec![MessageDisposition::Ack; n])
                .await
                .expect("commit (ack) failed");
        }
        assert!(last_lsn > 0, "delivered changes must carry an LSN");

        // Graceful stop: `close()` awaits the same disconnect hook the route awaits.
        consumer.close().await.expect("clean close");
        let confirmed = confirmed_flush_lsn(slot).await;
        assert!(
            lsn_to_u64(&confirmed) >= last_lsn,
            "confirmed_flush_lsn ({confirmed}) must cover the last acked change ({last_lsn:#x}) \
             after a clean stop"
        );
    })
    .await;
}

// --- Isolated CDC read benchmarks ---------------------------------------------------------------
// Unlike the coupled pipeline test (which times INSERT + WAL + read together and is therefore
// write-bound), these attach the reader first, seed the table *untimed* — the changes buffer in the
// WAL, retained by the slot — and then time only the logical-decoding drain. That isolates the CDC
// reader's throughput and per-change latency, which is where CDC differs from a bulk select.

/// Seed `n` rows in a single statement (untimed) so the read benchmark measures only the drain.
async fn seed_rows(n: usize) {
    let mut conn = connect_retry().await;
    sqlx::query(
        "INSERT INTO cdc_users (id, name) SELECT g, 'name-' || g FROM generate_series(1, $1) g",
    )
    .bind(n as i32)
    .execute(&mut conn)
    .await
    .expect("seed rows");
}

/// Isolated CDC read throughput: seed first (buffered in the WAL), then time only the drain.
pub async fn test_postgres_cdc_read_throughput() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let slot = "mqb_cdc_readbench_slot";
        reset_schema(slot).await;
        // Slot must exist BEFORE seeding so the WAL retains the changes for the reader.
        let mut consumer = PostgresCdcConsumer::new(&cfg(slot))
            .await
            .expect("create CDC consumer");

        let n = PERF_TEST_MESSAGE_COUNT;
        seed_rows(n).await;

        let start = Instant::now();
        let seen = drain_ids(&mut consumer, n).await;
        let elapsed = start.elapsed();

        assert!(seen.len() >= n, "captured {} < {n}", seen.len());
        let rps = n as f64 / elapsed.as_secs_f64();
        println!(
            "postgres_cdc read-only throughput: {rps:.0} rows/s ({n} rows in {:.2}s)",
            elapsed.as_secs_f64()
        );
        add_performance_result(PerformanceResult {
            test_name: "postgres_cdc read-only".to_string(),
            read_performance: rps,
            ..Default::default()
        });
    })
    .await;
}

/// Per-change insert->capture latency (p50/p95/p99). The reader is attached first; rows are then
/// inserted one at a time so each change is decoded on its own, and we time commit -> delivery.
pub async fn test_postgres_cdc_latency() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        let slot = "mqb_cdc_latency_slot";
        reset_schema(slot).await;
        let mut consumer = PostgresCdcConsumer::new(&cfg(slot))
            .await
            .expect("create CDC consumer");
        let n = 500usize;

        // Drain in a task, timestamping each row's arrival by id.
        let drain = tokio::spawn(async move {
            let mut arrivals: std::collections::HashMap<i64, Instant> =
                std::collections::HashMap::new();
            while arrivals.len() < n {
                let batch =
                    tokio::time::timeout(Duration::from_secs(30), consumer.receive_batch(1024))
                        .await
                        .expect("latency drain timed out")
                        .expect("receive_batch failed");
                let now = Instant::now();
                let count = batch.messages.len();
                for msg in &batch.messages {
                    let body: serde_json::Value =
                        serde_json::from_slice(&msg.payload).unwrap_or_default();
                    if let Some(id) = body.get("id").and_then(|v| v.as_i64()) {
                        arrivals.entry(id).or_insert(now);
                    }
                }
                (batch.commit)(vec![MessageDisposition::Ack; count])
                    .await
                    .expect("commit (ack) failed");
            }
            arrivals
        });

        // Insert one row at a time, recording the commit instant per id.
        let mut sent: std::collections::HashMap<i64, Instant> = std::collections::HashMap::new();
        let mut conn = connect_retry().await;
        for id in 1..=n as i64 {
            sqlx::query("INSERT INTO cdc_users (id, name) VALUES ($1, $2)")
                .bind(id as i32)
                .bind(format!("name-{id}"))
                .execute(&mut conn)
                .await
                .expect("insert row");
            sent.insert(id, Instant::now());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let arrivals = drain.await.expect("drain task panicked");
        let mut lat_ms: Vec<f64> = sent
            .iter()
            .filter_map(|(id, t0)| {
                arrivals
                    .get(id)
                    .map(|t1| t1.saturating_duration_since(*t0).as_secs_f64() * 1000.0)
            })
            .collect();
        assert!(!lat_ms.is_empty(), "no latencies collected");
        lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| lat_ms[(((lat_ms.len() as f64) * p) as usize).min(lat_ms.len() - 1)];
        println!(
            "postgres_cdc latency (n={}): p50={:.2}ms p95={:.2}ms p99={:.2}ms",
            lat_ms.len(),
            pct(0.50),
            pct(0.95),
            pct(0.99)
        );
    })
    .await;
}

// --- CDC performance pipeline -------------------------------------------------------------------
// Reports a "postgres_cdc Pipeline" row in the consolidated perf summary. The producer INSERTs rows
// via the `sqlx` publisher; the read side captures them off the WAL through a replication slot
// (`postgres_cdc`) — non-destructive, no `DELETE`-on-ack. The endpoint opens the slot when the route
// is deployed, which the harness does before the producer writes, so no changes are missed.

const PERF_TABLE: &str = "cdc_perf";

const PERF_CONFIG_YAML: &str = r#"
routes:
  memory_to_pgcdc:
    concurrency: 4
    batch_size: 1024
    input:
      memory: { topic: "pgcdc-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 20
            initial_interval_ms: 500
            max_interval_ms: 2000
      sqlx:
        url: "postgres://testuser:testpass@localhost:5432/testdb"
        table: "cdc_perf"
        min_connections: 2

  pgcdc_to_memory:
    concurrency: 1
    batch_size: 1024
    input:
      postgres_cdc:
        url: "postgres://testuser:testpass@localhost:5432/testdb"
        publication: "mqb_cdc_perf_pub"
        slot_name: "mqb_cdc_perf_slot"
    output:
      memory: { topic: "pgcdc-out", capacity: {out_capacity} }
"#;

/// Materialize the table (using the publisher's own schema), reset any leftover slot/publication,
/// then create the publication. The slot itself is created by the endpoint at deploy time.
async fn setup_perf_cdc() {
    let sqlx_cfg = SqlxConfig {
        url: URL.to_string(),
        table: PERF_TABLE.to_string(),
        auto_create_table: true,
        ..Default::default()
    };
    // Constructing the publisher creates the table.
    let _publisher = SqlxPublisher::new(&sqlx_cfg).await.expect("create table");

    let mut conn = connect_retry().await;
    sqlx::query("DELETE FROM cdc_perf")
        .execute(&mut conn)
        .await
        .ok();
    let _ = sqlx::query(
        "SELECT pg_drop_replication_slot('mqb_cdc_perf_slot') \
         FROM pg_replication_slots WHERE slot_name = 'mqb_cdc_perf_slot'",
    )
    .execute(&mut conn)
    .await;
    let _ = sqlx::query("DROP PUBLICATION IF EXISTS mqb_cdc_perf_pub")
        .execute(&mut conn)
        .await;
    sqlx::query("CREATE PUBLICATION mqb_cdc_perf_pub FOR TABLE cdc_perf")
        .execute(&mut conn)
        .await
        .expect("create publication");
}

pub async fn test_postgres_cdc_performance_pipeline() {
    setup_logging();
    run_test_with_docker(COMPOSE, || async {
        setup_perf_cdc().await;
        let config_yaml = PERF_CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test_named(
            "pgcdc",
            "postgres_cdc",
            &config_yaml,
            PERF_TEST_MESSAGE_COUNT,
        )
        .await;
    })
    .await;
}

/// Restart-safety: an in-flight, un-acked batch survives a Postgres restart.
/// After a clean restart, a fresh consumer resumes from the slot's confirmed
/// LSN and redelivers the un-acked changes — no data loss, no gap into
/// already-acknowledged changes.
pub async fn test_postgres_cdc_restart_safety() {
    setup_logging();
    run_test_with_docker_controller(COMPOSE, |controller| async move {
        let slot = "mqb_cdc_restart_slot";
        reset_schema(slot).await;
        let mut consumer = PostgresCdcConsumer::new(&cfg(slot))
            .await
            .expect("create CDC consumer");

        // Batch 1: consume and ACK — advances the slot's confirmed LSN.
        insert_rows(1..=50).await;
        let seen1 = drain_ids(&mut consumer, 50).await;
        assert_eq!(seen1.len(), 50, "batch 1 fully consumed");

        // Batch 2: read (in-flight) but DO NOT ack, then simulate a crash by
        // dropping the consumer before commit — the confirmed LSN stays put.
        insert_rows(51..=100).await;
        let _inflight = tokio::time::timeout(Duration::from_secs(20), consumer.receive_batch(1024))
            .await
            .expect("timed out reading in-flight batch")
            .expect("receive_batch failed");
        // Intentionally no commit() call.
        drop(consumer);

        // Clean restart of the database (permanent slot + confirmed LSN persist).
        controller.stop_service("postgres");
        controller.start_service("postgres");

        // Fresh consumer resumes from the slot's confirmed LSN (past batch 1).
        let mut consumer2 = {
            let mut last_err = None;
            let mut built = None;
            for _ in 0..30 {
                match PostgresCdcConsumer::new(&cfg(slot)).await {
                    Ok(c) => {
                        built = Some(c);
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            built.unwrap_or_else(|| panic!("reconnect CDC consumer: {last_err:?}"))
        };

        let seen2 = drain_ids(&mut consumer2, 50).await;
        let ids2: BTreeSet<i64> = seen2.iter().map(|(_, id)| *id).collect();

        // No loss: every un-acked change (51..=100) is redelivered.
        for id in 51..=100 {
            assert!(
                ids2.contains(&(id as i64)),
                "row {id} must be redelivered after restart (no data loss)"
            );
        }
        // No gap into already-acknowledged changes: batch 1 (1..=50) is not
        // re-read, since its LSN was confirmed before the restart.
        assert!(
            ids2.iter().all(|id| *id >= 51),
            "already-acked rows (<=50) must not be redelivered; got {ids2:?}"
        );
    })
    .await;
}
