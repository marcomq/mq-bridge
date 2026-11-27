//! The one place that maps a store URL or an input endpoint to the endpoint code behind it.
//! `checkpoint`, `deduplication`, `aggregate` and the route ask here, so none of them names
//! a database endpoint or its feature.

use crate::checkpoint::{CheckpointBackend, CheckpointStore, VersionedCheckpoint};
use crate::models::EndpointType;
use anyhow::anyhow;
use std::sync::Arc;

/// A shared store whose endpoint is not compiled into this build.
fn not_built(what: &str, backend: &CheckpointBackend) -> anyhow::Error {
    let (url, features) = match backend {
        CheckpointBackend::Sqlx { url, .. } if url.starts_with("sqlite:") => {
            (url, "'sqlite' or 'sqlx' feature")
        }
        CheckpointBackend::Sqlx { url, .. } => (url, "'sqlx' feature"),
        CheckpointBackend::Mongo { url, .. } => (url, "'mongodb' feature"),
        CheckpointBackend::ObjectStore { url } => (url, "'object-store' feature"),
        other => {
            return anyhow!("{what} needs a postgres, mysql, sqlite or mongodb URL; got {other:?}");
        }
    };
    anyhow!(
        "{what} '{}' requires the {features} to be enabled",
        crate::support::redact::url_password(url)
    )
}

/// A checkpoint store in a shared SQL table, MongoDB collection or object store.
pub(crate) async fn checkpoint_store(
    backend: CheckpointBackend,
    source_name: &str,
    cursor_id: &str,
) -> anyhow::Result<Arc<dyn CheckpointStore>> {
    let _ = (source_name, cursor_id);
    match backend {
        #[cfg(any(feature = "sqlite", feature = "sqlx"))]
        CheckpointBackend::Sqlx { url, table } => {
            super::sqlx::build_sql_checkpoint_store(&url, table, source_name, cursor_id).await
        }
        #[cfg(feature = "mongodb")]
        CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => {
            super::mongodb::build_mongo_checkpoint_store(
                &url,
                &database,
                collection,
                source_name,
                cursor_id,
            )
            .await
        }
        #[cfg(feature = "object-store")]
        CheckpointBackend::ObjectStore { url } => {
            crate::checkpoint::object_store_backend::build_object_store_checkpoint_store(
                &url,
                source_name,
                cursor_id,
            )
            .await
        }
        other => Err(not_built("checkpoint_store", &other)),
    }
}

/// A deduplication store in a shared SQL table or MongoDB collection.
#[cfg(feature = "dedup")]
pub(crate) async fn dedup_store(
    backend: CheckpointBackend,
    ttl_seconds: u64,
    route_name: &str,
    replay_response: bool,
) -> anyhow::Result<Arc<dyn crate::middleware::deduplication::DedupStore>> {
    let _ = (ttl_seconds, route_name, replay_response);
    match backend {
        #[cfg(feature = "mongodb")]
        CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => {
            super::mongodb::build_mongo_dedup_store(
                &url,
                &database,
                collection,
                ttl_seconds,
                route_name,
            )
            .await
        }
        #[cfg(any(feature = "sqlite", feature = "sqlx"))]
        CheckpointBackend::Sqlx { url, table } => {
            super::sqlx::build_sql_dedup_store(
                &url,
                table,
                ttl_seconds,
                route_name,
                replay_response,
            )
            .await
        }
        other => Err(not_built("deduplication store", &other)),
    }
}

/// An `aggregate` state store; `default_name` is the table or collection when the URL has none.
#[cfg(feature = "aggregate")]
pub(crate) async fn state_store(
    backend: CheckpointBackend,
    default_name: String,
) -> anyhow::Result<Arc<dyn crate::middleware::aggregate::store::StateStore>> {
    let _ = &default_name;
    match backend {
        #[cfg(any(feature = "sqlite", feature = "sqlx"))]
        CheckpointBackend::Sqlx { url, table } => {
            super::sqlx::build_sql_state_store(&url, table.unwrap_or(default_name)).await
        }
        #[cfg(feature = "mongodb")]
        CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => {
            super::mongodb::build_mongo_state_store(
                &url,
                &database,
                &collection.unwrap_or(default_name),
            )
            .await
        }
        other => Err(not_built("aggregate store", &other)),
    }
}

/// The resume checkpoint of an input endpoint; see [`crate::checkpoint::open_endpoint_checkpoint`].
pub(crate) async fn endpoint_checkpoint(
    route_name: &str,
    endpoint_type: &EndpointType,
) -> anyhow::Result<Option<Arc<VersionedCheckpoint>>> {
    let _ = route_name;
    match endpoint_type {
        #[cfg(any(feature = "sqlite", feature = "sqlx"))]
        EndpointType::Sqlx(cfg) if cfg.publication.is_none() && cfg.cursor_column.is_some() => {
            super::sqlx::cursor_checkpoint(cfg).await
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => super::clickhouse::cursor_checkpoint(cfg).await,
        #[cfg(feature = "http-bulk")]
        EndpointType::HttpBulk(cfg) => super::http_bulk::cursor_checkpoint(cfg).await,
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => super::object_store::cursor_checkpoint(cfg).await,
        #[cfg(feature = "mongodb")]
        EndpointType::MongoDb(cfg) => {
            use crate::models::MongoConsume;
            if !matches!(
                cfg.resolved_consume(),
                MongoConsume::CaptureNew | MongoConsume::CaptureAll
            ) {
                return Err(anyhow!(
                    "mongodb keeps a resume checkpoint only in capture_new/capture_all mode"
                ));
            }
            let mut cfg = cfg.clone();
            cfg.collection.get_or_insert_with(|| route_name.to_string());
            super::mongodb::change_stream_checkpoint(&cfg).await
        }
        EndpointType::PostgresCdc(_) => Err(anyhow!(
            "postgres_cdc resumes from its replication slot, which is authoritative; manage the slot instead"
        )),
        EndpointType::Sqlx(cfg) if cfg.publication.is_some() => Err(anyhow!(
            "sqlx with `publication` resumes from its replication slot, which is authoritative; manage the slot instead"
        )),
        _ => Err(anyhow!("this endpoint has no mq-bridge resume checkpoint")),
    }
}

/// Whether `no_resume` reaches this input. A compiled-out endpoint is served by a registered
/// factory, and the search presets build their reader without it, so both still resume.
#[allow(clippy::match_like_matches_macro)] // Every `cfg!` is `true` in a full build.
pub(crate) fn input_applies_no_resume(endpoint_type: &EndpointType) -> bool {
    match endpoint_type {
        EndpointType::ObjectStore(_) => cfg!(feature = "object-store"),
        EndpointType::HttpBulk(_) => cfg!(feature = "http-bulk"),
        EndpointType::ClickHouse(_) => cfg!(feature = "clickhouse"),
        EndpointType::Sqlx(_) => cfg!(any(feature = "sqlite", feature = "sqlx")),
        EndpointType::MongoDb(_) => cfg!(feature = "mongodb"),
        EndpointType::Ref(_) => true,
        _ => false,
    }
}

/// The error quotes a peer's status, so generic wording in it (e.g. "authentication failed")
/// may describe another hop rather than this one's credentials.
pub(crate) fn quotes_peer_failure(error: &anyhow::Error) -> bool {
    #[cfg(feature = "grpc")]
    if super::grpc::quotes_peer_failure(error) {
        return true;
    }
    let _ = error;
    false
}
