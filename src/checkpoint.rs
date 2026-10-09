//! Durable resume-cursor checkpoint storage for non-destructive copy sources.
//!
//! A cursor-read source persists the last successfully-sunk key so a restart resumes
//! without re-emitting already-copied rows. The backing store is selected by `checkpoint_store`:
//! the default keeps the cursor in the source datastore (a per-source `mqb_cursors_<source>`
//! collection/table); a `file:///…` URL keeps it in a local JSON file for read-only sources; a
//! `postgres|mysql|mongodb://…` URL points it at a separate database entirely; and an
//! `s3|gs|az|abfs://…` URL persists it to a cloud object store (one object per cursor).
//!
//! Values are opaque strings; each endpoint encodes its native key (a BSON `_id`, a SQL
//! column value) into a string it can decode back. Endpoints persist them through
//! [`VersionedCheckpoint`], which records which source wrote the value and when.

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

/// A durable store for a single cursor position, keyed by `cursor_id` at construction.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Returns the persisted cursor value, or `None` if no checkpoint exists yet.
    async fn load(&self) -> anyhow::Result<Option<String>>;
    /// Persists the cursor value, overwriting any previous position.
    async fn save(&self, value: &str) -> anyhow::Result<()>;
    /// Removes the persisted cursor, so the next run starts from the beginning.
    async fn clear(&self) -> anyhow::Result<()> {
        Err(anyhow!("this checkpoint store does not support clearing"))
    }
}

const ENVELOPE_VERSION: u64 = 1;

/// The stored form of a checkpoint value.
#[derive(Serialize, Deserialize)]
struct Envelope {
    mqb_checkpoint: u64,
    source: String,
    value: String,
    updated_at_ms: u64,
}

/// A checkpoint as stored, with what is known about who wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointEntry {
    /// The endpoint-encoded cursor value.
    pub value: String,
    /// The source that wrote it; `None` for a value saved before checkpoints were versioned.
    pub source: Option<String>,
    /// Unix time of the last save in milliseconds; `None` for an unversioned value.
    pub updated_at_ms: Option<u64>,
}

impl CheckpointEntry {
    fn decode(raw: String) -> anyhow::Result<Self> {
        let versioned = raw.starts_with('{')
            && serde_json::from_str::<serde_json::Value>(&raw)
                .is_ok_and(|v| v.get("mqb_checkpoint").is_some());
        if !versioned {
            return Ok(Self {
                value: raw,
                source: None,
                updated_at_ms: None,
            });
        }
        let envelope: Envelope =
            serde_json::from_str(&raw).context("Failed to parse versioned checkpoint")?;
        if envelope.mqb_checkpoint > ENVELOPE_VERSION {
            return Err(anyhow!(
                "checkpoint format v{} was written by a newer mq-bridge (this one reads up to v{ENVELOPE_VERSION})",
                envelope.mqb_checkpoint
            ));
        }
        Ok(Self {
            value: envelope.value,
            source: Some(envelope.source),
            updated_at_ms: Some(envelope.updated_at_ms),
        })
    }
}

/// Wraps a [`CheckpointStore`] so values are saved with the identity of the source that wrote
/// them. Loading refuses a value written by a different source (e.g. after `cursor_column`
/// changed) instead of resuming from a position that does not apply. Unversioned values from
/// older releases load as-is and are upgraded on the next save.
pub struct VersionedCheckpoint {
    inner: Arc<dyn CheckpointStore>,
    source: String,
}

impl VersionedCheckpoint {
    pub fn new(inner: Arc<dyn CheckpointStore>, source: impl Into<String>) -> Self {
        Self {
            inner,
            source: source.into(),
        }
    }

    /// The identity this checkpoint saves under, e.g. `sqlx:orders:id`.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The stored entry without the source check, for inspection.
    pub async fn entry(&self) -> anyhow::Result<Option<CheckpointEntry>> {
        self.inner
            .load()
            .await?
            .map(CheckpointEntry::decode)
            .transpose()
    }
}

#[async_trait]
impl CheckpointStore for VersionedCheckpoint {
    async fn load(&self) -> anyhow::Result<Option<String>> {
        let Some(entry) = self.entry().await? else {
            return Ok(None);
        };
        match &entry.source {
            Some(stored) if stored != &self.source => Err(anyhow!(
                "checkpoint was written by source `{stored}`, but this source is `{}`; the stored position does not apply. \
                 Reset it with `mqb checkpoint reset` or use a different cursor_id",
                self.source
            )),
            _ => Ok(Some(entry.value)),
        }
    }

    async fn save(&self, value: &str) -> anyhow::Result<()> {
        let updated_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let envelope = serde_json::to_string(&Envelope {
            mqb_checkpoint: ENVELOPE_VERSION,
            source: self.source.clone(),
            value: value.to_string(),
            updated_at_ms,
        })?;
        self.inner.save(&envelope).await
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.inner.clear().await
    }
}

/// Opens the resume checkpoint a consumer built from `endpoint_type` would use, without starting
/// it. `route_name` fills defaults a route derives from its name (a MongoDB `collection`).
/// Returns `None` when the endpoint has no durable checkpoint configured (no `cursor_id`).
pub async fn open_endpoint_checkpoint(
    route_name: &str,
    endpoint_type: &crate::models::EndpointType,
) -> anyhow::Result<Option<Arc<VersionedCheckpoint>>> {
    use crate::models::EndpointType;
    let _ = route_name;
    match endpoint_type {
        #[cfg(feature = "sqlx")]
        EndpointType::Sqlx(cfg) if cfg.publication.is_none() && cfg.cursor_column.is_some() => {
            crate::endpoints::sqlx::cursor_checkpoint(cfg).await
        }
        #[cfg(feature = "clickhouse")]
        EndpointType::ClickHouse(cfg) => crate::endpoints::clickhouse::cursor_checkpoint(cfg).await,
        #[cfg(feature = "http-bulk")]
        EndpointType::HttpBulk(cfg) => crate::endpoints::http_bulk::cursor_checkpoint(cfg).await,
        #[cfg(feature = "object-store")]
        EndpointType::ObjectStore(cfg) => {
            crate::endpoints::object_store::cursor_checkpoint(cfg).await
        }
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
            crate::endpoints::mongodb::change_stream_checkpoint(&cfg).await
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

/// Returns a process-wide async lock for `path`, so concurrent saves to the same checkpoint
/// file (even from different routes) serialize their read-modify-write instead of racing.
fn path_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    // Drop locks no caller holds any more, so the map doesn't grow with cursor paths.
    guard.retain(|_, v| Arc::strong_count(v) > 1);
    guard
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// A file-backed checkpoint store: a single JSON object mapping cursor keys to values,
/// written atomically (unique temp file + rename). Concurrent saves to the same path are
/// serialized in-process via `path_lock` only: two processes sharing one file can overwrite
/// each other's keys. Suitable for read-only sources and dev/CLI one-offs.
pub struct FileCheckpointStore {
    path: PathBuf,
    key: String,
}

impl FileCheckpointStore {
    pub fn new(path: impl Into<PathBuf>, key: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            key: key.into(),
        }
    }

    async fn read_map(&self) -> anyhow::Result<HashMap<String, String>> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!("Failed to parse checkpoint file '{}'", self.path.display())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e).with_context(|| {
                format!("Failed to read checkpoint file '{}'", self.path.display())
            }),
        }
    }
}

#[async_trait]
impl CheckpointStore for FileCheckpointStore {
    async fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(self.read_map().await?.get(&self.key).cloned())
    }

    async fn save(&self, value: &str) -> anyhow::Result<()> {
        self.update(|map| {
            map.insert(self.key.clone(), value.to_string());
        })
        .await
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.update(|map| {
            map.remove(&self.key);
        })
        .await
    }
}

impl FileCheckpointStore {
    async fn update(
        &self,
        change: impl FnOnce(&mut HashMap<String, String>),
    ) -> anyhow::Result<()> {
        // Serialize the read-modify-write so parallel saves to the same file can't lose updates.
        let lock = path_lock(&self.path);
        let _guard = lock.lock().await;

        let mut map = self.read_map().await?;
        change(&mut map);
        let bytes =
            serde_json::to_vec_pretty(&map).context("Failed to serialize checkpoint map")?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
        }
        // Atomic write: write to a unique sibling temp file, then rename over the target. A
        // per-write suffix avoids clobbering a leftover temp from another process/crash.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        // Synced before the rename, or a power loss can leave an empty file under the final name.
        let write = async {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&tmp).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await
        };
        write
            .await
            .with_context(|| format!("Failed to write checkpoint temp '{}'", tmp.display()))?;
        if let Err(e) = tokio::fs::rename(&tmp, &self.path).await {
            tokio::fs::remove_file(&tmp).await.ok();
            return Err(e)
                .with_context(|| format!("Failed to commit checkpoint '{}'", self.path.display()));
        }
        // Best-effort: without it a power loss can still undo the rename.
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            let dir = if parent.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                parent
            };
            if let Ok(dir) = tokio::fs::File::open(dir).await {
                dir.sync_all().await.ok();
            }
        }
        Ok(())
    }
}

// --- `checkpoint_store` URL parsing and backend selection ---

/// A parsed `checkpoint_store` destination. A recognized scheme selects a file/external
/// backend; a bare name reuses the source datastore.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointBackend {
    /// Schemeless value: reuse the source datastore; `name` is the explicit table/collection.
    Source { name: String },
    /// `file:///abs/path` — a local JSON key/value file.
    File { path: PathBuf },
    /// `postgres|postgresql|mysql|mariadb|sqlite://…[/table]` — an external SQL table.
    Sqlx { url: String, table: Option<String> },
    /// `mongodb://host/db[/collection]` — an external MongoDB collection.
    Mongo {
        url: String,
        database: String,
        collection: Option<String>,
    },
    /// `s3|gs|az|abfs://…` — a cloud object store; the full URL is handed to `object_store`.
    ObjectStore { url: String },
}

/// Sanitize a source table/collection into an identifier-safe token (`[^A-Za-z0-9_] -> _`).
pub fn sanitize_ident(source: &str) -> String {
    source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Default meta table/collection name for a source: `mqb_cursors_<source>`, capped to a safe
/// identifier length (63 bytes; a short hash suffix disambiguates truncated names).
pub fn default_meta_name(source: &str) -> String {
    let ident = sanitize_ident(source);
    let full = format!("{META_PREFIX}{ident}");
    if full.len() <= META_MAX {
        return full;
    }
    hashed_meta_name(source, &ident)
}

const META_PREFIX: &str = "mqb_cursors_";
const META_MAX: usize = 63;

fn hashed_meta_name(source: &str, ident: &str) -> String {
    let suffix = format!("_{:08x}", fnv1a(source.as_bytes()));
    let keep = (META_MAX - META_PREFIX.len() - suffix.len()).min(ident.len());
    format!("{META_PREFIX}{}{suffix}", &ident[..keep])
}

/// Meta name for a source that sanitization changed (`orders.v1`), hashed so it cannot collide
/// with `orders_v1`. `None` when [`default_meta_name`] is already unique for the source.
/// Callers use it only when no table exists under [`default_meta_name`].
pub fn disambiguated_meta_name(source: &str) -> Option<String> {
    let ident = sanitize_ident(source);
    (ident != source && META_PREFIX.len() + ident.len() <= META_MAX)
        .then(|| hashed_meta_name(source, &ident))
}

/// Stable FNV-1a hash, used to disambiguate identifiers that collide after sanitization.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Collision-resistant key for a cursor within an object-store checkpoint prefix. Sanitization
/// alone can collide (`a.b` and `a_b` both become `a_b`), so a hash of the raw, pre-sanitized
/// `source:cursor_id` pair is appended to keep distinct sources apart.
#[cfg(feature = "object-store")]
fn object_store_checkpoint_key(source_name: &str, cursor_id: &str) -> String {
    let sanitized = sanitize_ident(&checkpoint_key(source_name, cursor_id));
    let hash = fnv1a(format!("{source_name}\0{cursor_id}").as_bytes());
    format!("{sanitized}_{hash:08x}")
}

/// Namespaced key for a cursor within a (possibly shared) checkpoint store, so multiple sources
/// pointed at one store/file never collide: `<source>:<cursor_id>`.
pub fn checkpoint_key(source: &str, cursor_id: &str) -> String {
    format!("{}:{}", sanitize_ident(source), cursor_id)
}

/// Parse a `checkpoint_store` config value. A recognized `<scheme>:` selects a file/external
/// backend; anything else is a bare name for the source datastore (a leading `/` is stripped).
pub fn parse_checkpoint_store(spec: &str) -> anyhow::Result<CheckpointBackend> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("checkpoint_store is empty"));
    }
    let scheme = spec
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    match scheme.as_str() {
        "file" => parse_file_url(spec),
        "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite" => parse_sqlx_url(spec, &scheme),
        "mongodb" | "mongodb+srv" => parse_mongo_url(spec),
        // Cloud object stores: hand the whole URL to `object_store` (creds via env). `file` stays
        // on the local JSON store above; R2 uses `s3://` + a custom `AWS_ENDPOINT_URL`.
        "s3" | "s3a" | "gs" | "gcs" | "az" | "azure" | "abfs" | "abfss" => {
            // `object_store` only recognizes `gs://` for GCS, not the `gcs://` alias.
            let url = if scheme == "gcs" {
                format!("gs:{}", spec.split_once(':').unwrap().1)
            } else {
                spec.to_string()
            };
            Ok(CheckpointBackend::ObjectStore { url })
        }
        _ => {
            // Schemeless -> source datastore, using the given name (strip a leading '/').
            let name = spec.strip_prefix('/').unwrap_or(spec).to_string();
            Ok(CheckpointBackend::Source { name })
        }
    }
}

fn parse_file_url(spec: &str) -> anyhow::Result<CheckpointBackend> {
    let url =
        url::Url::parse(spec).with_context(|| format!("Invalid file checkpoint URL '{spec}'"))?;
    let to_path = |u: &url::Url| {
        u.to_file_path().map_err(|_| {
            anyhow!("Invalid file checkpoint path in '{spec}'; use 'file:///absolute/path'")
        })
    };
    match url.host_str() {
        None | Some("") => Ok(CheckpointBackend::File {
            path: to_path(&url)?,
        }),
        Some("localhost") => {
            // Rewrite `file://localhost/…` to the empty-authority form so to_file_path works.
            let rebuilt = url::Url::parse(&spec.replacen("//localhost/", "///", 1))
                .with_context(|| format!("Invalid file checkpoint URL '{spec}'"))?;
            Ok(CheckpointBackend::File {
                path: to_path(&rebuilt)?,
            })
        }
        Some(host) => Err(anyhow!(
            "Invalid file checkpoint URL '{spec}': '{host}' is parsed as a host. Use the three-slash form, e.g. 'file:///{host}{}'.",
            url.path()
        )),
    }
}

fn path_segments(url: &url::Url) -> Vec<String> {
    url.path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn parse_sqlx_url(spec: &str, scheme: &str) -> anyhow::Result<CheckpointBackend> {
    if scheme == "sqlite" {
        // SQLite URLs are file-path based; there is no path slot for a table name.
        return Ok(CheckpointBackend::Sqlx {
            url: spec.to_string(),
            table: None,
        });
    }
    let mut url =
        url::Url::parse(spec).with_context(|| format!("Invalid checkpoint URL '{spec}'"))?;
    let segments = path_segments(&url);
    match segments.len() {
        0 => Err(anyhow!(
            "checkpoint_store '{spec}' is missing a database name (e.g. postgres://host/db/table)"
        )),
        1 => Ok(CheckpointBackend::Sqlx {
            url: spec.to_string(),
            table: None,
        }),
        _ => {
            let table = segments.last().unwrap().clone();
            url.set_path(&format!("/{}", segments[..segments.len() - 1].join("/")));
            Ok(CheckpointBackend::Sqlx {
                url: url.to_string(),
                table: Some(table),
            })
        }
    }
}

fn parse_mongo_url(spec: &str) -> anyhow::Result<CheckpointBackend> {
    let mut url =
        url::Url::parse(spec).with_context(|| format!("Invalid checkpoint URL '{spec}'"))?;
    let segments = path_segments(&url);
    match segments.as_slice() {
        [] => Err(anyhow!(
            "checkpoint_store '{spec}' is missing a database name (mongodb://host/db[/collection])"
        )),
        [db] => Ok(CheckpointBackend::Mongo {
            url: spec.to_string(),
            database: db.clone(),
            collection: None,
        }),
        [db, coll] => {
            url.set_path(&format!("/{db}"));
            Ok(CheckpointBackend::Mongo {
                url: url.to_string(),
                database: db.clone(),
                collection: Some(coll.clone()),
            })
        }
        _ => Err(anyhow!(
            "checkpoint_store '{spec}' has too many path segments (expected mongodb://host/db[/collection])"
        )),
    }
}

/// Build a store for a scheme-based (file/external) backend, independent of the source datastore.
/// The schemeless `Source` variant is handled by the caller, which owns the live source connection.
pub async fn build_external_store(
    backend: CheckpointBackend,
    source_name: &str,
    cursor_id: &str,
) -> anyhow::Result<Arc<dyn CheckpointStore>> {
    match backend {
        CheckpointBackend::File { path } => Ok(Arc::new(FileCheckpointStore::new(
            path,
            checkpoint_key(source_name, cursor_id),
        ))),
        CheckpointBackend::Sqlx { url, table } => {
            #[cfg(feature = "sqlx")]
            {
                crate::endpoints::sqlx::build_sql_checkpoint_store(
                    &url,
                    table,
                    source_name,
                    cursor_id,
                )
                .await
            }
            #[cfg(not(feature = "sqlx"))]
            {
                let _ = (table, source_name, cursor_id);
                Err(anyhow!(
                    "checkpoint_store '{url}' requires the 'sqlx' feature to be enabled"
                ))
            }
        }
        CheckpointBackend::Mongo {
            url,
            database,
            collection,
        } => {
            #[cfg(feature = "mongodb")]
            {
                crate::endpoints::mongodb::build_mongo_checkpoint_store(
                    &url,
                    &database,
                    collection,
                    source_name,
                    cursor_id,
                )
                .await
            }
            #[cfg(not(feature = "mongodb"))]
            {
                let _ = (database, collection, source_name, cursor_id);
                Err(anyhow!(
                    "checkpoint_store '{url}' requires the 'mongodb' feature to be enabled"
                ))
            }
        }
        CheckpointBackend::ObjectStore { url } => {
            #[cfg(feature = "object-store")]
            {
                object_store_backend::build_object_store_checkpoint_store(
                    &url,
                    source_name,
                    cursor_id,
                )
                .await
            }
            #[cfg(not(feature = "object-store"))]
            {
                let _ = (source_name, cursor_id);
                Err(anyhow!(
                    "checkpoint_store '{url}' requires the 'object-store' feature to be enabled"
                ))
            }
        }
        CheckpointBackend::Source { .. } => Err(anyhow!(
            "internal: Source checkpoint backend must be built by the caller"
        )),
    }
}

/// Cloud object-store checkpoint backend: one object per cursor key (no read-modify-write,
/// no cross-process locking needed). URL/creds are resolved by `object_store` from env vars.
#[cfg(feature = "object-store")]
pub(crate) mod object_store_backend {
    use super::{object_store_checkpoint_key, CheckpointStore};
    use anyhow::Context;
    use async_trait::async_trait;
    use object_store::{path::Path as ObjPath, ObjectStore, ObjectStoreExt};
    use std::sync::Arc;

    /// Builds an `object_store` backend and its base prefix `Path` from a URL. Credentials and
    /// backend options (`AWS_ACCESS_KEY_ID`, `AWS_ENDPOINT`, `AWS_REGION`, `AWS_ALLOW_HTTP`,
    /// `GOOGLE_SERVICE_ACCOUNT`, ...) are read from the process environment.
    ///
    /// The backend config-key parsers only accept the lowercase form (`aws_access_key_id`), so
    /// env-var names are lowercased before being folded into the builder — the same
    /// normalization `AmazonS3Builder::from_env` does. Unrecognized keys are ignored. Bare
    /// `parse_url` reads no env at all, which would fall through to the EC2/GCE metadata service.
    pub(crate) fn build_store(url: &str) -> anyhow::Result<(Box<dyn ObjectStore>, ObjPath)> {
        let parsed =
            url::Url::parse(url).with_context(|| format!("Invalid object_store url '{url}'"))?;
        // `vars_os`, not `vars`: a single non-UTF-8 env var would panic the latter.
        let env = std::env::vars_os().filter_map(|(k, v)| {
            Some((
                k.into_string().ok()?.to_ascii_lowercase(),
                v.into_string().ok()?,
            ))
        });
        object_store::parse_url_opts(&parsed, env)
            .with_context(|| format!("Failed to build object store for '{url}'"))
    }

    struct ObjectStoreCheckpointStore {
        store: Arc<dyn ObjectStore>,
        path: ObjPath,
    }

    #[async_trait]
    impl CheckpointStore for ObjectStoreCheckpointStore {
        async fn load(&self) -> anyhow::Result<Option<String>> {
            match self.store.get(&self.path).await {
                Ok(result) => {
                    let bytes = result.bytes().await.with_context(|| {
                        format!("Failed to read checkpoint object '{}'", self.path)
                    })?;
                    let value = String::from_utf8(bytes.to_vec()).with_context(|| {
                        format!("Checkpoint object '{}' is not valid UTF-8", self.path)
                    })?;
                    Ok(Some(value))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e)
                    .with_context(|| format!("Failed to load checkpoint object '{}'", self.path)),
            }
        }

        async fn save(&self, value: &str) -> anyhow::Result<()> {
            self.store
                .put(&self.path, value.to_string().into())
                .await
                .with_context(|| format!("Failed to save checkpoint object '{}'", self.path))?;
            Ok(())
        }

        async fn clear(&self) -> anyhow::Result<()> {
            match self.store.delete(&self.path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(e)
                    .with_context(|| format!("Failed to delete checkpoint object '{}'", self.path)),
            }
        }
    }

    /// Build an object-store checkpoint store from a cloud URL. Each cursor gets its own object at
    /// `<url prefix>/<sanitized source:cursor_id>_<hash>`, so multiple cursors in one prefix never
    /// collide, even when sanitization alone would make two distinct sources look the same.
    pub(super) async fn build_object_store_checkpoint_store(
        url: &str,
        source_name: &str,
        cursor_id: &str,
    ) -> anyhow::Result<Arc<dyn CheckpointStore>> {
        let (store, base) = build_store(url)?;
        let key = object_store_checkpoint_key(source_name, cursor_id);
        let path = base.join(key);
        Ok(Arc::new(ObjectStoreCheckpointStore {
            store: Arc::from(store),
            path,
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use object_store::memory::InMemory;

        #[tokio::test]
        async fn object_store_round_trips_and_overwrites() {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let make = |key: &str| ObjectStoreCheckpointStore {
                store: store.clone(),
                path: ObjPath::from(format!("cursors/{key}")),
            };

            let c1 = make("a");
            assert_eq!(c1.load().await.unwrap(), None);
            c1.save("v1").await.unwrap();
            assert_eq!(c1.load().await.unwrap(), Some("v1".to_string()));
            c1.save("v2").await.unwrap();
            assert_eq!(c1.load().await.unwrap(), Some("v2".to_string()));

            // A second object in the same prefix is independent.
            let c2 = make("b");
            assert_eq!(c2.load().await.unwrap(), None);
            c2.save("other").await.unwrap();
            assert_eq!(c1.load().await.unwrap(), Some("v2".to_string()));
        }

        #[test]
        fn distinct_sources_that_collide_after_sanitization_get_distinct_keys() {
            // "a.b" and "a_b" both sanitize to "a_b"; the hash suffix must still separate them.
            let k1 = object_store_checkpoint_key("a.b", "cursor");
            let k2 = object_store_checkpoint_key("a_b", "cursor");
            assert_ne!(k1, k2);
        }

        // Exercises the real builder end-to-end (URL parse -> object_store::parse_url ->
        // key sanitize -> path.child) via the `memory://` scheme, so the path-derivation
        // wiring is covered without any external service.
        #[tokio::test]
        async fn builder_round_trips_via_memory_url() {
            let store =
                build_object_store_checkpoint_store("memory:///mqb/cursors", "orders", "default")
                    .await
                    .unwrap();
            assert_eq!(store.load().await.unwrap(), None);
            store.save("42").await.unwrap();
            assert_eq!(store.load().await.unwrap(), Some("42".to_string()));
            store.save("43").await.unwrap();
            assert_eq!(store.load().await.unwrap(), Some("43".to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_store_round_trips_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("mqb_ckpt_{}", fast_uuid_v7::gen_id()));
        let path = dir.join("cursors.json");
        let store = FileCheckpointStore::new(path.clone(), "coll:cursor:c1");

        assert_eq!(store.load().await.unwrap(), None);
        store.save("oid:abc").await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some("oid:abc".to_string()));
        store.save("oid:def").await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some("oid:def".to_string()));

        // A second key in the same file is independent.
        let other = FileCheckpointStore::new(path, "coll:cursor:c2");
        assert_eq!(other.load().await.unwrap(), None);
        other.save("int:5").await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some("oid:def".to_string()));

        tokio::fs::remove_dir_all(dir).await.ok();
    }

    #[tokio::test]
    async fn versioned_checkpoint_upgrades_legacy_and_rejects_foreign_source() {
        let dir = std::env::temp_dir().join(format!("mqb_ckpt_{}", fast_uuid_v7::gen_id()));
        let raw: Arc<dyn CheckpointStore> =
            Arc::new(FileCheckpointStore::new(dir.join("cursors.json"), "t:c1"));
        raw.save("int:7").await.unwrap();

        let store = VersionedCheckpoint::new(raw.clone(), "sqlx:t:id");
        assert_eq!(store.load().await.unwrap(), Some("int:7".to_string()));
        assert_eq!(store.entry().await.unwrap().unwrap().source, None);

        store.save("int:9").await.unwrap();
        let entry = store.entry().await.unwrap().unwrap();
        assert_eq!(entry.value, "int:9");
        assert_eq!(entry.source.as_deref(), Some("sqlx:t:id"));
        assert!(entry.updated_at_ms.is_some());

        let renamed = VersionedCheckpoint::new(raw.clone(), "sqlx:t:updated_at");
        let err = renamed.load().await.unwrap_err().to_string();
        assert!(err.contains("sqlx:t:id"), "{err}");

        renamed.clear().await.unwrap();
        assert_eq!(renamed.load().await.unwrap(), None);
        tokio::fs::remove_dir_all(dir).await.ok();
    }

    #[test]
    fn checkpoint_entry_keeps_json_legacy_values_and_refuses_newer_format() {
        let token = r#"{"_data":"8263"}"#.to_string();
        let entry = CheckpointEntry::decode(token.clone()).unwrap();
        assert_eq!(entry.value, token);
        assert_eq!(entry.source, None);

        let newer = r#"{"mqb_checkpoint":2,"source":"s","value":"v","updated_at_ms":1}"#;
        assert!(CheckpointEntry::decode(newer.to_string()).is_err());
    }

    #[test]
    fn parse_schemeless_is_source_datastore() {
        assert_eq!(
            parse_checkpoint_store("/my_cursors").unwrap(),
            CheckpointBackend::Source {
                name: "my_cursors".into()
            }
        );
        assert_eq!(
            parse_checkpoint_store("my_cursors").unwrap(),
            CheckpointBackend::Source {
                name: "my_cursors".into()
            }
        );
    }

    #[test]
    fn parse_object_store_schemes() {
        for url in ["s3://bucket/pre", "gs://bucket/pre", "az://acct/container"] {
            assert_eq!(
                parse_checkpoint_store(url).unwrap(),
                CheckpointBackend::ObjectStore {
                    url: url.to_string()
                }
            );
        }
        // `file://` stays on the local JSON store, not the object-store backend.
        // Build the URL from a real host-absolute path so `Url::to_file_path`
        // round-trips on Windows (needs a drive letter) as well as Unix.
        let file_url = url::Url::from_file_path(std::env::temp_dir().join("cursors.json"))
            .unwrap()
            .to_string();
        assert!(matches!(
            parse_checkpoint_store(&file_url).unwrap(),
            CheckpointBackend::File { .. }
        ));
        // `gcs://` is an alias `object_store` doesn't recognize; normalize it to `gs://`.
        assert_eq!(
            parse_checkpoint_store("gcs://bucket/pre").unwrap(),
            CheckpointBackend::ObjectStore {
                url: "gs://bucket/pre".to_string()
            }
        );
    }

    // Uses Unix absolute paths; `Url::to_file_path` only maps these to a real path on Unix
    // (Windows file URLs need a drive letter), so the success assertions are Unix-only.
    #[cfg(unix)]
    #[test]
    fn parse_file_requires_three_slashes() {
        assert_eq!(
            parse_checkpoint_store("file:///var/lib/mqb/cursors.json").unwrap(),
            CheckpointBackend::File {
                path: PathBuf::from("/var/lib/mqb/cursors.json")
            }
        );
        // localhost authority is accepted and normalized to a local path.
        assert_eq!(
            parse_checkpoint_store("file://localhost/var/lib/c.json").unwrap(),
            CheckpointBackend::File {
                path: PathBuf::from("/var/lib/c.json")
            }
        );
        // A bare host (two-slash + path) is rejected with a hint.
        assert!(parse_checkpoint_store("file://var/lib/c.json").is_err());
    }

    #[test]
    fn parse_sqlx_splits_trailing_table() {
        let b = parse_checkpoint_store("postgres://u@h:5432/mydb/cursors").unwrap();
        match b {
            CheckpointBackend::Sqlx { url, table } => {
                assert_eq!(table, Some("cursors".to_string()));
                assert!(
                    !url.contains("/cursors"),
                    "table stripped from conn url: {url}"
                );
                assert!(url.contains("/mydb"));
            }
            other => panic!("expected Sqlx, got {other:?}"),
        }
        // No trailing table -> default later.
        assert_eq!(
            parse_checkpoint_store("mysql://h/db").unwrap(),
            CheckpointBackend::Sqlx {
                url: "mysql://h/db".into(),
                table: None
            }
        );
        // SQLite is file-path based: never split a table out of the path.
        assert_eq!(
            parse_checkpoint_store("sqlite:///tmp/x.db").unwrap(),
            CheckpointBackend::Sqlx {
                url: "sqlite:///tmp/x.db".into(),
                table: None
            }
        );
    }

    #[test]
    fn parse_mongo_db_and_optional_collection() {
        let b = parse_checkpoint_store("mongodb://h:27017/mydb/cursors").unwrap();
        match b {
            CheckpointBackend::Mongo {
                url,
                database,
                collection,
            } => {
                assert_eq!(database, "mydb");
                assert_eq!(collection, Some("cursors".to_string()));
                assert!(!url.contains("/cursors"), "collection stripped: {url}");
            }
            other => panic!("expected Mongo, got {other:?}"),
        }
        assert_eq!(
            parse_checkpoint_store("mongodb://h/mydb").unwrap(),
            CheckpointBackend::Mongo {
                url: "mongodb://h/mydb".into(),
                database: "mydb".into(),
                collection: None
            }
        );
    }

    #[test]
    fn default_meta_name_is_unique_and_sanitized() {
        assert_eq!(default_meta_name("orders"), "mqb_cursors_orders");
        // Schema-qualified / odd chars are sanitized so the name stays a single identifier.
        assert_eq!(
            default_meta_name("public.orders"),
            "mqb_cursors_public_orders"
        );
        // Over-long sources are capped under the 63-char identifier limit.
        let long = "a".repeat(200);
        let name = default_meta_name(&long);
        assert!(name.len() <= 63, "capped: {} ({})", name, name.len());
        assert!(name.starts_with("mqb_cursors_"));
    }

    #[test]
    fn a_sanitized_source_gets_a_name_distinct_from_its_lookalike() {
        assert_eq!(disambiguated_meta_name("orders_v1"), None);
        let dotted = disambiguated_meta_name("orders.v1").unwrap();
        assert!(dotted.starts_with("mqb_cursors_orders_v1_"), "{dotted}");
        assert_ne!(dotted, disambiguated_meta_name("orders-v1").unwrap());
        assert!(disambiguated_meta_name(&"a.".repeat(40)).is_none());
        let near_limit = format!("a.{}", "b".repeat(48));
        assert!(disambiguated_meta_name(&near_limit).unwrap().len() <= 63);
    }

    #[test]
    fn checkpoint_key_namespaces_by_source() {
        assert_eq!(checkpoint_key("orders", "copy-1"), "orders:copy-1");
        assert_eq!(checkpoint_key("a.b", "c1"), "a_b:c1");
    }

    // Regression: many stores writing distinct keys to one shared file concurrently must not
    // lose any update (unsynchronized read-modify-write + a shared temp name would).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file_store_concurrent_saves_do_not_lose_updates() {
        let dir = std::env::temp_dir().join(format!("mqb_ckpt_conc_{}", fast_uuid_v7::gen_id()));
        let path = dir.join("cursors.json");
        const N: usize = 50;

        let mut handles = Vec::new();
        for i in 0..N {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                FileCheckpointStore::new(p, format!("key-{i}"))
                    .save(&format!("val-{i}"))
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        for i in 0..N {
            let store = FileCheckpointStore::new(path.clone(), format!("key-{i}"));
            assert_eq!(
                store.load().await.unwrap(),
                Some(format!("val-{i}")),
                "lost update for key-{i}"
            );
        }
        tokio::fs::remove_dir_all(dir).await.ok();
    }
}
