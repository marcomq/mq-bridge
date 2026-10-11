#[cfg(feature = "amqp")]
pub mod amqp;
#[cfg(feature = "aws")]
pub mod aws;
#[cfg(feature = "clickhouse")]
pub mod clickhouse;
pub mod contract;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http")]
pub mod http_tls;
#[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
pub mod ibm_mq;
#[cfg(any(feature = "ibm-mq-static", feature = "ibm-mq"))]
pub mod ibm_mq_tls;
#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "sqlx")]
pub mod mariadb;
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mongodb")]
pub mod mongodb_raw;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "sqlx")]
pub mod mysql;
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "object-store")]
pub mod object_store;
#[cfg(feature = "sqlx")]
pub mod postgres;
#[cfg(all(feature = "postgres-cdc", feature = "test-utils"))]
pub mod postgres_cdc;
#[cfg(feature = "redis-streams")]
pub mod redis_streams;
#[cfg(feature = "sqlx")]
pub mod sqlite;
pub mod tls_helpers;
#[cfg(feature = "websocket")]
pub mod websocket;
#[cfg(feature = "zeromq")]
pub mod zeromq;

pub mod file;
pub mod ipc;
pub mod logic_test;
pub mod memory;
// performance_static was just for internal optimiztion - not a real test

#[cfg(feature = "grpc")]
pub mod grpc_tls;
pub mod route;

/// Assert that running `route` once fails with a `ConsumerError::Permanent`.
///
/// This targets the classification itself, which is what decides the route's fate:
/// `route.rs` breaks out of the reconnect loop only for `Permanent`, so anything else spins
/// on the reconnect interval forever. `run_until_err` is used rather than `run` because a
/// permanent failure *during startup* reaches the caller of `run` as the same generic
/// "failed to start" error as a timeout — the very ambiguity that hid these diagnoses.
#[allow(dead_code)]
pub async fn assert_permanent_consumer_error(
    route: mq_bridge::Route,
    route_name: &str,
    label: &str,
) {
    use mq_bridge::traits::ConsumerError;

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        route.run_until_err(route_name, None, None),
    )
    .await
    .unwrap_or_else(|_| panic!("'{label}': route neither completed nor failed"))
    .expect_err("must fail");

    assert!(
        matches!(
            err.downcast_ref::<ConsumerError>(),
            Some(ConsumerError::Permanent(_))
        ),
        "'{label}': must be a permanent error so the route stops; got: {err:#}"
    );
}
