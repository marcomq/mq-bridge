//! The delivery contract (`mq_bridge::test_utils::contract`) against real brokers. Library test
//! (not `mqb`), `#[ignore]`d: each `test_<broker>_contract` starts
//! `tests/integration/docker-compose/<broker>.yml` and runs every check, printing each.
//!
//! `MQB_TEST_BACKEND=nats cargo test --test integration_test --features full,test-utils \
//!  -- --ignored --nocapture integration::contract::test_nats_contract`

#![allow(dead_code, unused_imports)]

use mq_bridge::test_utils::contract::Contract;
use mq_bridge::test_utils::run_test_with_docker;
use serde_json::json;

/// Runs the contract, on a fresh `compose` stack unless that is empty.
async fn check(compose: &str, target: Contract) {
    if !mq_bridge::test_utils::should_run(target.name()) {
        return;
    }
    let run = || async move {
        let report = target.run().await;
        println!("{report}");
        report.assert_passed();
    };
    if compose.is_empty() {
        run().await;
    } else {
        run_test_with_docker(compose, run).await;
    }
}

fn compose(file: &str) -> &'static str {
    Box::leak(format!("tests/integration/docker-compose/{file}.yml").into_boxed_str())
}

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_nats_contract() {
    let nats = json!({ "url": "nats://localhost:4222", "subject": "contract.$ID", "stream": "contract_$ID", "stream_max_bytes": 67108864 });
    let contract = Contract::new("nats", json!({ "nats": nats }), json!({ "nats": nats }));
    check(compose("nats"), contract).await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_contract() {
    let amqp = json!({ "url": "amqp://guest:guest@localhost:5672/%2f", "queue": "contract_$ID" });
    let contract = Contract::new("amqp", json!({ "amqp": amqp }), json!({ "amqp": amqp }));
    check(compose("amqp"), contract).await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_kafka_contract() {
    // Four partitions: only messages sharing a key keep their order.
    let input = json!({ "kafka": { "url": "localhost:9092", "topic": "contract_$ID", "group_id": "contract_$ID" } });
    let output = json!({ "kafka": { "url": "localhost:9092", "topic": "contract_$ID", "partition_key": "contract_key" } });
    check(compose("kafka"), Contract::new("kafka", input, output)).await;
}

#[cfg(feature = "redis-streams")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_streams_contract() {
    // The 60 s default would make each redelivery step wait a minute.
    let input = json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract_$ID", "group": "contract", "redelivery_timeout_ms": 1000 } });
    let output =
        json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract_$ID" } });
    check(
        compose("redis"),
        Contract::new("redis_streams", input, output),
    )
    .await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_contract() {
    let mongodb = json!({ "url": "mongodb://localhost:27017", "database": "contract", "collection": "contract_$ID" });
    let mut input = mongodb.clone();
    input["consume"] = json!("consumer");
    let contract = Contract::new(
        "mongodb",
        json!({ "mongodb": input }),
        json!({ "mongodb": mongodb }),
    )
    .expire_locks(|id| async move {
        let client = mongodb::Client::with_uri_str("mongodb://localhost:27017")
            .await
            .expect("mongodb client");
        client
            .database("contract")
            .collection::<mongodb::bson::Document>(&format!("contract_{id}"))
            .update_many(
                mongodb::bson::doc! { "locked_until": { "$ne": null } },
                mongodb::bson::doc! { "$set": { "locked_until": 0_i64 } },
            )
            .await
            .expect("expire claims");
    });
    check(compose("mongodb"), contract).await;
}

#[cfg(feature = "mqtt")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mqtt_contract() {
    let mqtt = |client_id: &str| {
        json!({ "mqtt": {
            "url": "mqtt://localhost:1883", "topic": "contract/$ID", "qos": 1,
            "client_id": client_id, "clean_session": false, "protocol": "v5",
        } })
    };
    let contract = Contract::new("mqtt", mqtt("contract-in-$ID"), mqtt("contract-out-$ID"));
    check(compose("mqtt"), contract).await;
}

/// A queue table on MySQL or MariaDB; the row has no id column the reader would take one from.
#[cfg(feature = "sqlx")]
fn mysql_contract(name: &'static str, url: &'static str) -> Contract {
    let input = json!({ "sqlx": { "url": url, "table": "contract_$ID", "delete_after_read": true, "polling_interval_ms": 50 } });
    let output =
        json!({ "sqlx": { "url": url, "table": "contract_$ID", "auto_create_table": true } });
    Contract::new(name, input, output)
        .unstable_id()
        .id_not_kept()
        .no_metadata()
        // The claimed rows come back in no specified order.
        .unordered()
        .expire_locks(move |id| async move {
            let pool = sqlx::MySqlPool::connect(url).await.expect("mysql pool");
            sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE contract_{id} SET locked_until = NOW() - INTERVAL 1 SECOND WHERE locked_until IS NOT NULL")))
                .execute(&pool)
                .await
                .expect("expire locks");
        })
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mysql_contract() {
    const URL: &str = "mysql://testuser:testpass@localhost:3306/testdb";
    check(compose("mysql"), mysql_contract("mysql", URL)).await;
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mariadb_contract() {
    const URL: &str = "mysql://testuser:testpass@localhost:3307/testdb";
    check(compose("mariadb"), mysql_contract("mariadb", URL)).await;
}

#[cfg(feature = "aws")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_aws_contract() {
    let sqs = json!({ "aws": {
        "queue_url": "http://localhost:4566/000000000000/contract_$ID",
        "region": "us-east-1", "endpoint_url": "http://localhost:4566",
        "access_key": "test", "secret_key": "test", "wait_time_seconds": 1,
    } });
    // A standard queue is unordered, and SQS assigns its own MessageId.
    let contract = Contract::new("aws", sqs.clone(), sqs)
        .unordered()
        .id_not_kept()
        .setup(|id| async move {
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .endpoint_url("http://localhost:4566")
                .credentials_provider(aws_sdk_sqs::config::Credentials::new(
                    "test", "test", None, None, "test",
                ))
                .load()
                .await;
            // Short enough that the crash step does not wait out the 30 s default.
            aws_sdk_sqs::Client::new(&config)
                .create_queue()
                .queue_name(format!("contract_{id}"))
                .attributes(
                    aws_sdk_sqs::types::QueueAttributeName::VisibilityTimeout,
                    "2",
                )
                .send()
                .await
                .expect("create queue");
        });
    check(compose("aws"), contract).await;
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_postgres_contract() {
    const URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
    let input = json!({ "sqlx": { "url": URL, "table": "contract_$ID", "delete_after_read": true, "polling_interval_ms": 50 } });
    let output =
        json!({ "sqlx": { "url": URL, "table": "contract_$ID", "auto_create_table": true } });
    // The table row has no id column the reader would take one from, and `RETURNING` hands
    // the claimed rows back in no specified order.
    let contract = Contract::new("postgres", input, output)
        .unstable_id()
        .id_not_kept()
        .no_metadata()
        .unordered()
        .expire_locks(|id| async move {
            let pool = sqlx::PgPool::connect(URL).await.expect("postgres pool");
            sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE contract_{id} SET locked_until = NOW() - interval '1 second' WHERE locked_until IS NOT NULL")))
                .execute(&pool)
                .await
                .expect("expire locks");
        });
    check(compose("postgres"), contract).await;
}

/// A plugin endpoint. `MQB_TEST_PLUGIN` lists the libraries like `PATH`, `MQB_TEST_CONTRACT` a JSON object with
/// `name`, `compose` (file under `docker-compose/`, or empty), `input`, `output` and optionally
/// `stable_id`, `hop_keeps_id`, `keeps_metadata`, `ordered` set to `false`, and `json: true`.
#[cfg(feature = "plugin")]
#[tokio::test]
#[ignore = "requires MQB_TEST_PLUGIN and MQB_TEST_CONTRACT"]
async fn test_plugin_contract() {
    let (Ok(library), Ok(spec)) = (
        std::env::var("MQB_TEST_PLUGIN"),
        std::env::var("MQB_TEST_CONTRACT"),
    ) else {
        println!("MQB_TEST_PLUGIN or MQB_TEST_CONTRACT unset, skipped");
        return;
    };
    for path in std::env::split_paths(&library) {
        let plugins = mq_bridge::plugin::load_endpoint_plugins(&path).expect("load plugin");
        let names: Vec<_> = plugins.iter().map(|plugin| &plugin.name).collect();
        println!("loaded {}: {names:?}", path.display());
    }
    let spec: serde_json::Value = serde_json::from_str(&spec).expect("MQB_TEST_CONTRACT json");
    let name = spec["name"].as_str().expect("name");
    let compose = match spec["compose"].as_str().unwrap_or_default() {
        "" => "",
        file => self::compose(file),
    };
    let declares_not = |key: &str| spec[key].as_bool() == Some(false);
    let mut contract = Contract::new(name, spec["input"].clone(), spec["output"].clone());
    if declares_not("stable_id") {
        contract = contract.unstable_id();
    }
    if declares_not("hop_keeps_id") {
        contract = contract.id_not_kept();
    }
    if declares_not("keeps_metadata") {
        contract = contract.no_metadata();
    }
    if declares_not("ordered") {
        contract = contract.unordered();
    }
    if spec["json"].as_bool() == Some(true) {
        contract = contract.json_payloads();
    }
    check(compose, contract).await;
}

/// A spool directory: no broker, so nothing to start.
#[cfg(feature = "dir-spool")]
#[tokio::test]
#[ignore = "delivery contract, run with the broker contracts"]
async fn test_dir_spool_contract() {
    let dir = tempfile::tempdir().expect("spool dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let spool = json!({ "dir_spool": { "path": format!("{path}/$ID"), "poll_interval_ms": 20 } });
    check("", Contract::new("dir_spool", spool.clone(), spool)).await;
}

/// A queue table in one SQLite file per check.
#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "delivery contract, run with the broker contracts"]
async fn test_sqlite_contract() {
    let dir = tempfile::tempdir().expect("database dir");
    let path = dir.path().to_str().expect("utf-8 path").to_string();
    let url = format!("sqlite://{path}/$ID.db?mode=rwc");
    let input = json!({ "sqlx": { "url": url, "table": "contract", "delete_after_read": true, "polling_interval_ms": 50 } });
    let output = json!({ "sqlx": { "url": url, "table": "contract", "auto_create_table": true } });
    let contract = Contract::new("sqlite", input, output)
        .unstable_id()
        .id_not_kept()
        .no_metadata()
        .expire_locks(move |id| {
            let url = format!("sqlite://{path}/{id}.db");
            async move {
                let pool = sqlx::SqlitePool::connect(&url).await.expect("sqlite pool");
                sqlx::query("UPDATE contract SET locked_until = datetime('now', '-1 second') WHERE locked_until IS NOT NULL")
                    .execute(&pool)
                    .await
                    .expect("expire locks");
            }
        });
    check("", contract).await;
}

/// A sled tree in queue mode, one database directory per check.
#[cfg(feature = "sled")]
#[tokio::test]
#[ignore = "delivery contract, run with the broker contracts"]
async fn test_sled_contract() {
    let dir = tempfile::tempdir().expect("database dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let sled = json!({ "sled": {
        "path": format!("{path}/$ID"), "tree": "contract",
        "read_from_start": true, "delete_after_read": true,
    } });
    check("", Contract::new("sled", sled.clone(), sled)).await;
}

/// A file read in queue mode: acknowledged lines are removed from it.
#[cfg(feature = "file")]
#[tokio::test]
#[ignore = "delivery contract, run with the broker contracts"]
async fn test_file_contract() {
    let dir = tempfile::tempdir().expect("file dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let file = json!({ "file": { "path": format!("{path}/$ID.jsonl"), "mode": "consume", "delete": true } });
    check("", Contract::new("file", file.clone(), file)).await;
}

/// ClickHouse as a sink: its cursor source is non-destructive, so rows are read back with SQL.
#[cfg(feature = "clickhouse")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_clickhouse_contract() {
    use super::clickhouse::ch_exec;
    use base64::Engine;

    let output = json!({ "clickhouse": {
        "url": "http://localhost:8123", "username": "testuser", "password": "testpass",
        "table": "contract_$ID",
    } });
    let contract = Contract::sink_only("clickhouse", output, |id| async move {
        let rows = ch_exec(&format!(
            "SELECT payload FROM contract_{id} FORMAT JSONEachRow"
        ))
        .await;
        rows.lines()
            .map(|row| {
                let row: serde_json::Value = serde_json::from_str(row)?;
                let encoded = row["payload"].as_str().unwrap_or_default();
                Ok(base64::engine::general_purpose::STANDARD.decode(encoded)?)
            })
            .collect()
    })
    .json_payloads()
    .setup(|id| async move {
        ch_exec(&format!(
            "CREATE TABLE contract_{id} (payload String) ENGINE = MergeTree ORDER BY tuple()"
        ))
        .await;
    });
    check(compose("clickhouse"), contract).await;
}

/// S3 as a sink: every object under the check's prefix is read back.
#[cfg(feature = "object-store")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_object_store_contract() {
    use ::object_store::{path::Path, ObjectStore, ObjectStoreExt};
    use futures::TryStreamExt;

    use super::object_store::{ensure_bucket, set_s3_env, BUCKET};

    set_s3_env();
    let output =
        json!({ "object_store": { "url": format!("s3://{BUCKET}/$ID"), "format": "normal" } });
    let contract = Contract::sink_only("object_store", output, |id| async move {
        let store = ::object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(BUCKET)
            .build()?;
        let objects: Vec<_> = store.list(Some(&Path::from(id))).try_collect().await?;
        let mut payloads = Vec::new();
        for object in objects {
            let body = store.get(&object.location).await?.bytes().await?;
            for line in body.split(|byte| *byte == b'\n').filter(|l| !l.is_empty()) {
                let message: mq_bridge::CanonicalMessage = serde_json::from_slice(line)?;
                payloads.push(message.payload.to_vec());
            }
        }
        Ok(payloads)
    })
    .setup(|_| ensure_bucket());
    check(compose("object_store"), contract).await;
}

/// One queue shared by all checks: the channel may only open the queues defined in `mq_init.mqsc`.
#[cfg(feature = "ibm-mq")]
#[tokio::test]
#[ignore = "requires docker compose and the IBM MQ client"]
async fn test_ibm_mq_contract() {
    if !super::ibm_mq::client_available() {
        println!("ibm_mq: no MQ client library installed, skipped");
        return;
    }
    let ibm_mq = json!({ "ibmmq": {
        "url": "localhost(1414)", "queue": "DEV.QUEUE.1", "queue_manager": "QM1",
        "channel": "DEV.APP.SVRCONN", "username": "app", "password": "adminpass",
    } });
    let contract = Contract::new("ibm_mq", ibm_mq.clone(), ibm_mq)
        .id_not_kept()
        .no_metadata()
        .skip(
            mq_bridge::test_utils::contract::Check::MixedAcks,
            "a batch is one MQ transaction: a nack backs out its acked messages too",
        );
    check(compose("ibm_mq"), contract).await;
}

/// The sqlx queue table read through logical replication instead of by polling. One shared
/// table and slot: a slot per check would exhaust `max_replication_slots`.
#[cfg(feature = "postgres-cdc")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_postgres_cdc_contract() {
    const URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
    let input = json!({ "postgres_cdc": {
        "url": URL, "publication": "contract", "slot_name": "contract",
        "create_publication": true, "publication_tables": ["contract"], "status_interval_ms": 200 } });
    let output = json!({ "sqlx": { "url": URL, "table": "contract" } });
    let contract = Contract::new("postgres_cdc", input, output)
        .id_not_kept()
        .no_metadata()
        .setup(|_| async {
            let pool = sqlx::PgPool::connect(URL).await.expect("postgres pool");
            sqlx::query("CREATE TABLE contract (id BIGSERIAL PRIMARY KEY, payload BYTEA NOT NULL, locked_until TIMESTAMPTZ, created_at TIMESTAMPTZ DEFAULT NOW())")
                .execute(&pool)
                .await
                .expect("create table");
        });
    check(compose("postgres_cdc"), contract).await;
}
