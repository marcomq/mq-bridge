// Aggregate Docker-backed integration suites. Keep these ignored so default
// `cargo test` stays local and fast.
//
// Run from the project root:
// cargo test --test integration_test --features full,test-utils --release -- --ignored --nocapture --test-threads=1
//
// Limit to a backend with MQB_TEST_BACKEND, for example:
// MQB_TEST_BACKEND=kafka cargo test --test integration_test --features full,test-utils --release -- --ignored --nocapture --test-threads=1

#![allow(unused_imports, dead_code)]

#[path = "integration/mod.rs"]
mod integration;

#[allow(dead_code)]
pub fn should_run(test_name: &str) -> bool {
    mq_bridge::test_utils::should_run(test_name)
}

#[cfg(all(feature = "kafka", feature = "perf-diagnostics"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnostic: consume-only isolation"]
async fn test_kafka_consume_only() {
    integration::kafka::test_kafka_consume_only_bench().await;
}

#[cfg(all(feature = "kafka", feature = "perf-diagnostics"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnostic: produce-only isolation"]
async fn test_kafka_produce_only() {
    integration::kafka::test_kafka_produce_only_bench().await;
}

#[cfg(feature = "amqp")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_amqp_message_id_round_trip() {
    integration::amqp::test_amqp_message_id_round_trip().await;
}

#[cfg(all(feature = "mongodb", feature = "dedup"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_mongodb_dedup_store_competing_instances() {
    integration::mongodb::test_mongodb_dedup_store_competing_instances().await;
}

#[cfg(feature = "kafka")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_kafka_nack_replay() {
    integration::kafka::test_kafka_nack_replays_from_committed_offset().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_all_request_reply() {
    println!("--- Running All Request-Reply Tests ---");
    #[cfg(feature = "kafka")]
    if should_run("kafka") {
        integration::route::test_kafka_request_reply().await;
        integration::route::test_kafka_request_reply_multiple_sequential().await;
        integration::route::test_kafka_request_reply_lost_response().await;
    }
    #[cfg(feature = "nats")]
    if should_run("nats") {
        integration::route::test_nats_request_reply().await;
        integration::route::test_nats_core_request_reply().await;
    }
    #[cfg(feature = "mongodb")]
    if should_run("mongodb") {
        integration::route::test_mongodb_request_reply_pattern().await;
        integration::route::test_mongodb_request_reply_multiple_sequential().await;
        integration::route::test_mongodb_request_reply_lost_response().await;
    }
    #[cfg(feature = "amqp")]
    if should_run("amqp") {
        integration::route::test_amqp_request_reply().await;
    }
    #[cfg(feature = "mqtt")]
    if should_run("mqtt") {
        integration::route::test_mqtt_request_reply().await;
    }
    integration::route::test_memory_request_reply().await;
    // Additional memory request-reply tests for multi-message scenarios
    integration::route::test_memory_request_reply_multiple_sequential().await;
    integration::route::test_memory_request_reply_multiple_concurrent().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_all_subscriber_logic() {
    println!("--- Running All Subscriber and Request-Reply Logic Tests ---");

    // --- Subscriber Logic ---
    #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
    {
        if should_run("ibm-mq") && integration::ibm_mq::client_available() {
            println!("\n\n>>> Starting IBM MQ Subscriber Logic Test...");
            integration::ibm_mq::test_ibm_mq_subscriber_logic().await;
        }
    }
    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Subscriber Logic Test...");
            integration::kafka::test_kafka_subscriber_logic().await;
        }
    }
    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Subscriber Logic Test...");
            integration::mqtt::test_mqtt_subscriber_logic().await;
        }
    }
    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Subscriber Logic Test...");
            integration::nats::test_nats_subscriber_logic().await;
            println!("\n\n>>> Starting NATS Drain (exit_on_empty) Test...");
            integration::nats::test_nats_drain_exits_on_empty().await;
        }
    }
    #[cfg(feature = "redis-streams")]
    {
        if should_run("redis_streams") {
            println!("\n\n>>> Starting Redis Streams Subscriber Logic Test...");
            integration::redis_streams::test_redis_subscriber_logic().await;
            println!("\n\n>>> Starting Redis Streams Drain (exit_on_empty) Test...");
            integration::redis_streams::test_redis_drain_exits_on_empty().await;
            println!("\n\n>>> Starting Redis Streams Partial Batch Test...");
            integration::redis_streams::test_redis_partial_batch_is_delivered().await;
            println!("\n\n>>> Starting Redis Streams Reclaim (drained stream) Test...");
            integration::redis_streams::test_redis_reclaims_backlog_with_drained_stream().await;
        }
    }
    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Subscriber Logic Test...");
            integration::amqp::test_amqp_subscriber_logic().await;
        }
    }
    if should_run("file") {
        println!("\n\n>>> Starting File Subscriber Logic Test...");
        integration::file::test_file_subscriber_logic().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_chaos() {
    println!("--- Running All Chaos Tests ---");
    println!("Tests are run sequentially.");

    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Chaos Test...");
            integration::kafka::test_kafka_chaos().await;
        }
    }

    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Chaos Test...");
            integration::nats::test_nats_chaos().await;
        }
    }

    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Chaos Test...");
            integration::amqp::test_amqp_chaos().await;
        }
    }

    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Chaos Test...");
            // MQTT chaos tests are currently flaky due to issues with session persistence/QoS handling
            // in the test environment (Mosquitto + rumqttc).
            integration::mqtt::test_mqtt_chaos().await;
        }
    }

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Chaos Test...");
            integration::mongodb::test_mongodb_chaos().await;
        }
    }

    #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
    {
        if should_run("ibm-mq") && integration::ibm_mq::client_available() {
            println!("\n\n>>> Starting IBM MQ Chaos Test...");
            integration::ibm_mq::test_ibm_mq_chaos().await;
        }
    }

    // AWS chaos test is excluded by default as it requires LocalStack which can be heavy/flaky in some envs
    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") || should_run("postgres") {
            integration::postgres::test_postgres_chaos().await;
        }
        if should_run("sqlx") || should_run("mariadb") {
            integration::mariadb::test_mariadb_chaos().await;
        }
        if should_run("sqlx") || should_run("mysql") {
            integration::mysql::test_mysql_chaos().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_status() {
    println!("--- Running All Status Tests ---");
    println!("Tests are run sequentially.");

    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Status Test...");
            integration::kafka::test_kafka_status().await;
        }
    }

    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Status Test...");
            integration::nats::test_nats_status().await;
        }
    }

    #[cfg(feature = "redis-streams")]
    {
        if should_run("redis_streams") {
            println!("\n\n>>> Starting Redis Streams Status Test...");
            integration::redis_streams::test_redis_status().await;
        }
    }

    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Status Test...");
            integration::amqp::test_amqp_status().await;
        }
    }

    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Status Test...");
            integration::mqtt::test_mqtt_status().await;
        }
    }

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Status Test...");
            integration::mongodb::test_mongodb_status().await;
        }
    }

    #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
    {
        if should_run("ibm-mq") && integration::ibm_mq::client_available() {
            println!("\n\n>>> Starting IBM MQ Status Test...");
            integration::ibm_mq::test_ibm_mq_status().await;
        }
    }

    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") || should_run("postgres") {
            integration::postgres::test_postgres_status().await;
        }
        if should_run("sqlx") || should_run("mariadb") {
            integration::mariadb::test_mariadb_status().await;
        }
        if should_run("sqlx") || should_run("sqlite") {
            integration::sqlite::test_sqlite_status().await;
        }
        if should_run("sqlx") || should_run("mysql") {
            integration::mysql::test_mysql_status().await;
        }
    }

    #[cfg(feature = "aws")]
    {
        if should_run("aws") {
            println!("\n\n>>> Starting AWS Status Test...");
            integration::aws::test_aws_status().await;
        }
    }

    #[cfg(feature = "clickhouse")]
    {
        if should_run("clickhouse") {
            println!("\n\n>>> Starting ClickHouse Status Test...");
            integration::clickhouse::test_clickhouse_status().await;
        }
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_sqlx_multicolumn() {
    if should_run("sqlx") || should_run("postgres") {
        integration::postgres::test_postgres_multicolumn().await;
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_sqlx_cursor_timestamptz_to_json() {
    if should_run("sqlx") || should_run("postgres") {
        integration::postgres::test_postgres_cursor_timestamptz_to_json().await;
    }
}

#[cfg(feature = "clickhouse")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_clickhouse() {
    if should_run("clickhouse") {
        integration::clickhouse::test_clickhouse_roundtrip().await;
    }
}

#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_pipeline().await;
    }
}

#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_batches_a_backlog_of_small_transactions() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_batches_a_backlog_of_small_transactions()
            .await;
    }
}

#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_temporary_slot() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_temporary_slot().await;
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mysql)"]
async fn test_mysql_cursor_unmappable_types() {
    if should_run("sqlx") || should_run("mysql") {
        integration::mysql::test_mysql_cursor_unmappable_types().await;
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mariadb)"]
async fn test_mariadb_cursor_unmappable_types() {
    if should_run("sqlx") || should_run("mariadb") {
        integration::mariadb::test_mariadb_cursor_unmappable_types().await;
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mysql)"]
async fn test_mysql_checkpoint_table() {
    if should_run("sqlx") || should_run("mysql") {
        integration::mysql::test_mysql_checkpoint_table().await;
    }
}

#[cfg(feature = "sqlx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mariadb)"]
async fn test_mariadb_checkpoint_table() {
    if should_run("sqlx") || should_run("mariadb") {
        integration::mariadb::test_mariadb_checkpoint_table().await;
    }
}

#[cfg(feature = "nats")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (nats)"]
async fn test_nats_subject_stream_mismatch_fails_fast() {
    if should_run("nats") {
        integration::nats::test_nats_subject_stream_mismatch_fails_fast().await;
    }
}

#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_confirms_lsn_on_clean_stop() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_confirms_lsn_on_clean_stop().await;
    }
}

#[cfg(feature = "object-store")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (localstack s3)"]
async fn test_object_store_pipeline() {
    if should_run("object_store") {
        integration::object_store::test_object_store_pipeline().await;
    }
}

#[cfg(feature = "object-store")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (localstack s3)"]
async fn test_object_store_checkpoint_round_trip() {
    if should_run("object_store") {
        integration::object_store::test_object_store_checkpoint_round_trip().await;
    }
}

#[cfg(feature = "object-store")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (localstack s3)"]
async fn test_object_store_permanent_errors_fail_fast() {
    if should_run("object_store") {
        integration::object_store::test_object_store_permanent_errors_fail_fast().await;
    }
}

#[cfg(feature = "object-store")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (localstack s3)"]
async fn test_object_store_resume() {
    if should_run("object_store") {
        integration::object_store::test_object_store_resume().await;
    }
}

#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_restart() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_restart_safety().await;
    }
}

/// Isolated CDC read throughput (seed untimed, time only the replication drain).
#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_read_throughput() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_read_throughput().await;
    }
}

/// Per-change insert->capture latency (p50/p95/p99).
#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (postgres with wal_level=logical)"]
async fn test_postgres_cdc_latency() {
    if should_run("postgres_cdc") || should_run("postgres") {
        integration::postgres_cdc::test_postgres_cdc_latency().await;
    }
}

/// Isolated MongoDB change-stream read throughput (seed untimed, time only the drain).
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mongodb replica set)"]
async fn test_mongodb_cdc_read_throughput() {
    if should_run("mongodb_cdc") || should_run("mongodb") {
        integration::mongodb::test_mongodb_cdc_read_throughput().await;
    }
}

/// MongoDB change-stream per-change insert->capture latency (p50/p95/p99).
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mongodb replica set)"]
async fn test_mongodb_cdc_latency() {
    if should_run("mongodb_cdc") || should_run("mongodb") {
        integration::mongodb::test_mongodb_cdc_latency().await;
    }
}

/// Regression: an idle change stream must survive the idle resume-token refresh (no abort).
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mongodb replica set)"]
async fn test_mongodb_cdc_survives_idle_resume_refresh() {
    if should_run("mongodb_cdc") || should_run("mongodb") {
        integration::mongodb::test_mongodb_cdc_survives_idle_resume_refresh().await;
    }
}

/// Regression: capture_all must surface an empty batch after its snapshot stays quiet in drain mode.
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (mongodb replica set)"]
async fn test_mongodb_capture_all_exits_on_empty() {
    if should_run("mongodb_cdc") || should_run("mongodb") {
        integration::mongodb::test_mongodb_capture_all_exits_on_empty().await;
    }
}

/// Standalone MongoDB supports snapshot, but change-stream modes require a replica set.
#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose (standalone mongodb)"]
async fn test_mongodb_standalone_mode_boundaries() {
    if should_run("mongodb") {
        integration::mongodb::test_mongodb_standalone_mode_boundaries().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_performance_pipeline() {
    let _summary_printer = mq_bridge::test_utils::PerformanceSummaryPrinter;
    println!("--- Running All Performance Pipeline Tests ---");
    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Performance Pipeline Test...");
            integration::kafka::test_kafka_performance_pipeline().await;
        }
    }
    #[cfg(feature = "aws")]
    {
        if should_run("aws") {
            println!("\n\n>>> Starting AWS Performance Pipeline Test...");
            integration::aws::test_aws_performance_pipeline().await;
        }
    }
    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Performance Pipeline Test...");
            integration::amqp::test_amqp_performance_pipeline().await;
        }
    }
    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Performance Pipeline Test...");
            integration::mqtt::test_mqtt_performance_pipeline().await;
        }
    }
    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Performance Pipeline Test...");
            integration::nats::test_nats_performance_pipeline().await;
        }
    }
    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Performance Pipeline Test...");
            integration::mongodb::test_mongodb_performance_pipeline().await;
        }
        if should_run("mongodb_replica_set") {
            println!("\n\n>>> Starting MongoDB Replica Set Performance Pipeline Test...");
            integration::mongodb::test_mongodb_replica_set_pipeline().await;
        }
        if should_run("mongodb_cdc") {
            println!("\n\n>>> Starting MongoDB CDC (change stream) Performance Pipeline Test...");
            integration::mongodb::test_mongodb_cdc_performance_pipeline().await;
        }
    }
    #[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
    {
        if should_run("postgres_cdc") {
            println!(
                "\n\n>>> Starting Postgres CDC (logical replication) Performance Pipeline Test..."
            );
            integration::postgres_cdc::test_postgres_cdc_performance_pipeline().await;
        }
    }
    #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
    {
        if should_run("ibm-mq") && integration::ibm_mq::client_available() {
            println!("\n\n>>> Starting IBM MQ Performance Pipeline Test...");
            integration::ibm_mq::test_ibm_mq_performance_pipeline().await;
        }
    }
    #[cfg(feature = "zeromq")]
    {
        if should_run("zeromq") {
            println!("\n\n>>> Starting ZeroMQ Performance Pipeline Test...");
            integration::zeromq::test_zeromq_performance_pipeline().await;
        }
    }
    #[cfg(feature = "redis-streams")]
    {
        if should_run("redis_streams") {
            println!("\n\n>>> Starting Redis Streams Performance Pipeline Test...");
            integration::redis_streams::test_redis_performance_pipeline().await;
        }
    }
    #[cfg(feature = "grpc")]
    {
        if should_run("grpc") {
            println!("\n\n>>> Starting gRPC Performance Pipeline Test...");
            integration::grpc::test_grpc_performance_pipeline().await;
        }
    }
    #[cfg(feature = "http")]
    {
        if should_run("http") {
            println!("\n\n>>> Starting HTTP Performance Pipeline Test...");
            integration::http::test_http_performance_pipeline().await;
        }
    }
    #[cfg(feature = "websocket")]
    {
        if should_run("websocket") {
            println!("\n\n>>> Starting WebSocket Performance Pipeline Test...");
            integration::websocket::test_websocket_performance_pipeline().await;
        }
    }
    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") || should_run("postgres") {
            integration::postgres::test_postgres_performance_pipeline().await;
        }
        if should_run("sqlx") || should_run("mysql") {
            integration::mysql::test_mysql_performance_pipeline().await;
        }
        if should_run("sqlx") || should_run("mariadb") {
            integration::mariadb::test_mariadb_performance_pipeline().await;
        }
        if should_run("sqlx") || should_run("sqlite") {
            integration::sqlite::test_sqlite_performance_pipeline().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_performance_direct() {
    // This instance will print the summary table when it's dropped at the end of the test.
    let _summary_printer = mq_bridge::test_utils::PerformanceSummaryPrinter;

    println!("--- Running All Direct Performance Tests ---");
    println!("Tests are run sequentially to ensure accurate measurements.");

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb_rs") {
            println!("\n\n>>> Starting MongoDB Replica Set Direct Performance Test...");
            integration::mongodb::test_mongodb_replica_set_performance_direct().await;
        }
        if should_run("mongodb_direct") {
            println!("\n\n>>> Starting MongoDB Direct Performance Test...");
            integration::mongodb::test_mongodb_performance_direct().await;
        }
    }
    #[cfg(feature = "aws")]
    {
        if should_run("aws") {
            println!("\n\n>>> Starting AWS Direct Performance Test...");
            integration::aws::test_aws_performance_direct().await;
        }
    }
    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Direct Performance Test...");
            integration::nats::test_nats_performance_direct().await;
        }
    }
    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Direct Performance Test...");
            integration::mqtt::test_mqtt_performance_direct().await;
        }
    }
    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Direct Performance Test...");
            integration::kafka::test_kafka_performance_direct().await;
        }
    }
    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Direct Performance Test...");
            integration::amqp::test_amqp_performance_direct().await;
        }
    }
    #[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
    {
        if should_run("ibm-mq") && integration::ibm_mq::client_available() {
            println!("\n\n>>> Starting IBM MQ Direct Performance Test...");
            integration::ibm_mq::test_ibm_mq_performance_direct().await;
        }
    }
    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") || should_run("postgres") {
            integration::postgres::test_postgres_performance_direct().await;
        }
        if should_run("sqlx") || should_run("mariadb") {
            integration::mariadb::test_mariadb_performance_direct().await;
        }
        if should_run("sqlx") || should_run("sqlite") {
            integration::sqlite::test_sqlite_performance_direct().await;
        }
        if should_run("sqlx") || should_run("mysql") {
            integration::mysql::test_mysql_performance_direct().await;
        }
    }
    // The summary table will be printed here when `_summary_printer` is dropped.
}
