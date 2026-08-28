//! Machine-local, same-user presence registry.
//!
//! Each long-running process owns one small JSON lease.  Leases are replaced
//! atomically and are ignored after a short TTL, so a crashed process cannot
//! leave a permanent peer row behind.

use anyhow::{Result, anyhow};
use mq_bridge::models::EndpointType;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const STATUS_SCHEMA_VERSION: u32 = 1;
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
pub const LEASE_TTL: Duration = Duration::from_secs(7);
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_TEXT_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceKind {
    Cli,
    /// The default: the hooks-free constructors are all UI-hosted.
    #[default]
    WebUi,
    Mcp,
    Tauri,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusSummary {
    pub running: bool,
    pub healthy: bool,
    pub pending: Option<usize>,
    pub capacity: Option<usize>,
    pub error: Option<String>,
    pub throughput: f64,
    pub message_sequence: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusEntity {
    pub id: String,
    pub label: String,
    /// Connector type only (for example `kafka` or `http`), never its config.
    pub endpoint: String,
    pub summary: StatusSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusRoute {
    pub id: String,
    pub label: String,
    pub input: StatusEntity,
    pub output: StatusEntity,
    pub summary: StatusSummary,
}

/// The mutable part of a lease: what this process is currently running.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub consumers: Vec<StatusEntity>,
    pub publishers: Vec<StatusEntity>,
    pub routes: Vec<StatusRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InstanceStatus {
    pub schema_version: u32,
    pub instance_id: String,
    pub pid: u32,
    pub kind: InstanceKind,
    pub application_version: String,
    pub started_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub workspace_id: String,
    pub workspace_label: String,
    #[serde(default)]
    pub consumers: Vec<StatusEntity>,
    #[serde(default)]
    pub publishers: Vec<StatusEntity>,
    #[serde(default)]
    pub routes: Vec<StatusRoute>,
}

impl InstanceStatus {
    pub fn new(
        kind: InstanceKind,
        application_version: impl Into<String>,
        workspace_path: &str,
    ) -> Self {
        let now = now_ms();
        let (workspace_id, workspace_label) = workspace_identity(workspace_path);
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            instance_id: Uuid::new_v4().to_string(),
            pid: std::process::id(),
            kind,
            application_version: sanitize_text(&application_version.into()),
            started_at_ms: now,
            last_seen_at_ms: now,
            workspace_id,
            workspace_label,
            consumers: Vec::new(),
            publishers: Vec::new(),
            routes: Vec::new(),
        }
    }

    pub fn heartbeat(&mut self) {
        self.last_seen_at_ms = now_ms();
    }

    pub fn apply(&mut self, snapshot: StatusSnapshot) {
        self.consumers = snapshot.consumers;
        self.publishers = snapshot.publishers;
        self.routes = snapshot.routes;
    }

    pub fn is_fresh(&self, now: u64) -> bool {
        self.last_seen_at_ms <= now.saturating_add(60_000)
            && now.saturating_sub(self.last_seen_at_ms) <= LEASE_TTL.as_millis() as u64
    }

    /// Scrub fields at the serialization boundary.  This intentionally keeps
    /// only connector labels and counters; payloads, headers, URLs and config
    /// values never belong in a lease.
    pub fn sanitized(mut self) -> Self {
        self.application_version = sanitize_text(&self.application_version);
        self.workspace_label = sanitize_label(&self.workspace_label);
        for entity in self.consumers.iter_mut().chain(self.publishers.iter_mut()) {
            sanitize_entity(entity);
        }
        for route in &mut self.routes {
            route.id = sanitize_label(&route.id);
            route.label = sanitize_label(&route.label);
            sanitize_entity(&mut route.input);
            sanitize_entity(&mut route.output);
            sanitize_summary(&mut route.summary);
        }
        self
    }
}

fn sanitize_entity(entity: &mut StatusEntity) {
    entity.id = sanitize_label(&entity.id);
    entity.label = sanitize_label(&entity.label);
    entity.endpoint = sanitize_endpoint(&entity.endpoint);
    sanitize_summary(&mut entity.summary);
}

fn sanitize_summary(summary: &mut StatusSummary) {
    // Endpoint and route errors are free-form and may contain payloads,
    // headers, URLs, or credentials. Presence is useful for a status dot, but
    // the diagnostic text is never safe to advertise across processes.
    summary.error = summary.error.as_ref().map(|_| "error".to_string());
    if !summary.throughput.is_finite() || summary.throughput < 0.0 {
        summary.throughput = 0.0;
    }
}

pub fn sanitize_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT_CHARS)
        .collect()
}

/// The connector type of an endpoint, e.g. `"kafka"`, with no config attached.
///
/// [`EndpointType::name`] returns the same tags serde writes, so this is a
/// borrow rather than a serialization: the endpoint's config — and every
/// credential in it — is never materialized just to read its variant name.
pub fn endpoint_type_label(endpoint_type: &EndpointType) -> &'static str {
    endpoint_type.name()
}

fn sanitize_label(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("://")
        || lower.contains('@')
        || lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
    {
        "redacted".to_string()
    } else {
        sanitize_text(value)
    }
}

pub fn sanitize_endpoint(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        value.chars().take(64).collect()
    } else {
        "unknown".to_string()
    }
}

pub fn workspace_identity(path: &str) -> (String, String) {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let id = hex::encode(&digest[..16]);
    let label = canonical
        .file_stem()
        .or_else(|| canonical.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    (id, sanitize_text(label))
}

/// A builder for owner-only directories, on the platforms that have a mode.
fn private_dir_builder() -> fs::DirBuilder {
    #[allow(unused_mut)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}

pub trait StatusRegistry: Send + Sync {
    fn publish(&self, status: &InstanceStatus) -> Result<()>;
    fn remove(&self, instance_id: &str) -> Result<()>;
    fn list(&self) -> Result<Vec<InstanceStatus>>;
}

#[derive(Debug, Clone)]
pub struct LocalStatusRegistry {
    directory: PathBuf,
}

impl LocalStatusRegistry {
    pub fn new() -> Result<Self> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("TMPDIR").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_RUNTIME_DIR")
                .or_else(|| std::env::var_os("TMPDIR"))
                .map(PathBuf::from)
        }
        .ok_or_else(|| anyhow!("no private per-user runtime directory is available"))?;
        Self::new_in(base.join("mq-bridge-app").join("instances"))
    }

    pub fn new_in(directory: impl Into<PathBuf>) -> Result<Self> {
        let directory = directory.into();
        // The parent is created private too, so the leaf can never be created
        // inside a world-writable directory of our own making. Creating the leaf
        // ourselves (rather than via `create_dir_all`) is what keeps it off a
        // permissive umask: it is 0700 from the moment it exists, never after.
        if let Some(parent) = directory.parent() {
            private_dir_builder().recursive(true).create(parent)?;
        }
        #[allow(unused_variables)]
        let existed = match private_dir_builder().create(&directory) {
            Ok(()) => false,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => true,
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
            let directory_file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&directory)?;
            let metadata = directory_file.metadata()?;
            if !metadata.is_dir() {
                return Err(anyhow!("status registry path is not a directory"));
            }
            // Ownership before permissions: a directory another user pre-created
            // (or symlinked into place) can be mode 0700 and still be theirs, and
            // the mode check alone would wave it through.
            if metadata.uid() != unsafe { libc::getuid() } {
                return Err(anyhow!(
                    "status registry directory is owned by another user"
                ));
            }
            if metadata.mode() & 0o077 != 0 && existed {
                return Err(anyhow!("status registry directory is shared"));
            }
            directory_file.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        {
            let metadata = fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("status registry path is a symlink"));
            }
            if !metadata.is_dir() {
                return Err(anyhow!("status registry path is not a directory"));
            }
        }
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn lease_path(&self, instance_id: &str) -> Result<PathBuf> {
        if instance_id.is_empty()
            || !instance_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(anyhow!("invalid instance id"));
        }
        Ok(self.directory.join(format!("{instance_id}.json")))
    }

    fn remove_stale(&self, path: &Path) {
        let _ = fs::remove_file(path);
    }

    /// Whether an abandoned `publish` temporary is old enough to delete.
    ///
    /// A process killed between creating the temporary and renaming it leaves the
    /// file behind for good — nothing else ever names it. The age bound keeps a
    /// concurrent publisher's in-flight temporary safe.
    fn is_abandoned_temporary(path: &Path, metadata: &fs::Metadata) -> bool {
        let is_temporary = path.extension().and_then(|ext| ext.to_str()) == Some("tmp")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
        is_temporary
            && metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > LEASE_TTL)
    }
}

impl StatusRegistry for LocalStatusRegistry {
    fn publish(&self, status: &InstanceStatus) -> Result<()> {
        let status = status.clone().sanitized();
        let path = self.lease_path(&status.instance_id)?;
        let tmp = self
            .directory
            .join(format!(".{}.{}.tmp", status.instance_id, Uuid::new_v4()));
        let bytes = serde_json::to_vec(&status)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(anyhow!("status record exceeds size limit"));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Private from the moment it exists, rather than chmod'ed afterwards.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        drop(file);
        #[cfg(windows)]
        let _ = fs::remove_file(&path);
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    fn remove(&self, instance_id: &str) -> Result<()> {
        let path = self.lease_path(instance_id)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn list(&self) -> Result<Vec<InstanceStatus>> {
        let now = now_ms();
        let mut statuses = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                if let Ok(metadata) = entry.metadata()
                    && Self::is_abandoned_temporary(&path, &metadata)
                {
                    self.remove_stale(&path);
                }
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > MAX_RECORD_BYTES {
                self.remove_stale(&path);
                continue;
            }
            let Ok(mut file) = File::open(&path) else {
                continue;
            };
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            if file.read_to_end(&mut bytes).is_err() {
                continue;
            }
            let Ok(status) = serde_json::from_slice::<InstanceStatus>(&bytes) else {
                self.remove_stale(&path);
                continue;
            };
            // A version this build does not understand belongs to a process that
            // is very likely still running (a half-upgraded install), so it is
            // skipped rather than deleted — otherwise two versions delete each
            // other's leases every heartbeat.
            if status.schema_version != STATUS_SCHEMA_VERSION {
                continue;
            }
            if !status.is_fresh(now) {
                self.remove_stale(&path);
                continue;
            }
            statuses.push(status.sanitized());
        }
        statuses.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
        Ok(statuses)
    }
}

/// A published lease plus the heartbeat task that keeps it alive.
///
/// Owning both in one value is what makes teardown reliable: every exit path,
/// including an early `?`, drops this and removes the lease, instead of each
/// call site remembering to abort a task and delete a file.
pub struct StatusLease {
    registry: LocalStatusRegistry,
    instance_id: String,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Set once the lease has been released, under the same lock the blocking
    /// publish holds. `abort` cannot cancel a `spawn_blocking` that is already
    /// running, so without this a publish in flight during `release` renames the
    /// lease back into place after the file was removed.
    released: Arc<Mutex<bool>>,
}

impl StatusLease {
    /// Publishes a lease and heartbeats it every [`HEARTBEAT_INTERVAL`],
    /// refreshing it from `snapshot` each tick.
    ///
    /// Returns `None` when no private runtime directory is available: status is
    /// best-effort everywhere, and never a reason to fail a process.
    pub fn spawn<F, Fut>(
        kind: InstanceKind,
        application_version: &str,
        workspace_path: &str,
        snapshot: F,
    ) -> Option<Self>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = StatusSnapshot> + Send,
    {
        let registry = LocalStatusRegistry::new()
            .inspect_err(|error| tracing::debug!("local status registry unavailable: {error}"))
            .ok()?;
        Some(Self::spawn_in(
            registry,
            kind,
            application_version,
            workspace_path,
            snapshot,
        ))
    }

    pub fn spawn_in<F, Fut>(
        registry: LocalStatusRegistry,
        kind: InstanceKind,
        application_version: &str,
        workspace_path: &str,
        mut snapshot: F,
    ) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = StatusSnapshot> + Send,
    {
        let mut status = InstanceStatus::new(kind, application_version, workspace_path);
        let instance_id = status.instance_id.clone();
        let task_registry = registry.clone();
        let released = Arc::new(Mutex::new(false));
        let task_released = Arc::clone(&released);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let snapshot = snapshot().await;
                status.heartbeat();
                status.apply(snapshot);
                publish_off_thread(&task_registry, &status, &task_released).await;
            }
        });
        Self {
            registry,
            instance_id,
            task: Some(task),
            released,
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn registry(&self) -> &LocalStatusRegistry {
        &self.registry
    }

    /// Stops heartbeating and deletes the lease. Idempotent, so an explicit
    /// shutdown followed by `Drop` is fine.
    pub fn release(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // Taken before the removal and dropped after it, so a publish that is
        // already inside its blocking section finishes first and no later one
        // can start. The wait is one small fsync at most.
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *released = true;
        let _ = self.registry.remove(&self.instance_id);
    }
}

impl Drop for StatusLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// `publish` fsyncs, so it never runs on an async worker thread.
async fn publish_off_thread(
    registry: &LocalStatusRegistry,
    status: &InstanceStatus,
    released: &Arc<Mutex<bool>>,
) {
    let registry = registry.clone();
    let status = status.clone();
    let released = Arc::clone(released);
    let published = tokio::task::spawn_blocking(move || {
        // Under the same lock `release` takes, so a lease that has been released
        // is never written back.
        let released = released.lock().unwrap_or_else(|error| error.into_inner());
        if *released {
            return Ok(());
        }
        registry.publish(&status).map_err(|e| e.to_string())
    })
    .await;
    if let Ok(Err(error)) = published {
        tracing::debug!("status registry publish failed: {error}");
    }
}

/// `list` walks a directory and reads every lease, so like [`publish_off_thread`]
/// it stays off the async workers.
pub async fn list_off_thread(registry: &LocalStatusRegistry) -> Vec<InstanceStatus> {
    let registry = registry.clone();
    tokio::task::spawn_blocking(move || registry.list().unwrap_or_default())
        .await
        .unwrap_or_default()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_and_malformed_records_are_ignored() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let mut status = InstanceStatus::new(InstanceKind::Mcp, "1", "/tmp/a.yml");
        registry.publish(&status).unwrap();
        assert_eq!(registry.list().unwrap().len(), 1);
        status.last_seen_at_ms = now_ms().saturating_sub(LEASE_TTL.as_millis() as u64 + 1);
        registry.publish(&status).unwrap();
        assert!(registry.list().unwrap().is_empty());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn secrets_and_urls_are_not_serialized() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let mut status = InstanceStatus::new(InstanceKind::WebUi, "1", "/tmp/work.yml");
        status.publishers.push(StatusEntity {
            id: "p".into(),
            label: "publisher".into(),
            endpoint: "https://user:password@example.test/topic".into(),
            summary: StatusSummary {
                error: Some("payload body and Authorization: token=secret".into()),
                ..Default::default()
            },
        });
        registry.publish(&status).unwrap();
        let raw = fs::read_to_string(
            registry
                .directory()
                .join(format!("{}.json", status.instance_id)),
        )
        .unwrap();
        assert!(!raw.contains("password"));
        assert!(!raw.contains("example.test"));
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("payload body"));
        assert!(raw.contains("\"error\":\"error\""));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn concurrent_instances_keep_independent_leases() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let first = InstanceStatus::new(InstanceKind::Cli, "1", "/tmp/workspace.yml");
        let second = InstanceStatus::new(InstanceKind::Mcp, "1", "/tmp/workspace.yml");
        registry.publish(&first).unwrap();
        registry.publish(&second).unwrap();
        let leases = registry.list().unwrap();
        assert_eq!(leases.len(), 2);
        assert_ne!(leases[0].instance_id, leases[1].instance_id);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn malformed_and_oversized_files_are_removed() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        fs::write(temp.join("malformed.json"), b"not-json").unwrap();
        fs::write(
            temp.join("oversized.json"),
            vec![b'x'; MAX_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        assert!(registry.list().unwrap().is_empty());
        assert!(!temp.join("malformed.json").exists());
        assert!(!temp.join("oversized.json").exists());
        let _ = fs::remove_dir_all(temp);
    }

    // A half-upgraded install runs two schema versions side by side. Each must
    // ignore the other's leases without deleting them, or they spend every
    // heartbeat removing each other from the UI.
    #[test]
    fn unknown_schema_versions_are_skipped_but_kept() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let mut future = InstanceStatus::new(InstanceKind::Cli, "2", "/tmp/workspace.yml");
        future.schema_version = STATUS_SCHEMA_VERSION + 1;
        let path = temp.join(format!("{}.json", future.instance_id));
        fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();

        assert!(registry.list().unwrap().is_empty());
        assert!(path.exists(), "a newer lease must survive an older reader");
        let _ = fs::remove_dir_all(temp);
    }

    // The lease must outlive neither its process nor its owner's intent: the
    // guard removes it on every exit path, including an early return.
    #[tokio::test]
    async fn dropping_a_lease_removes_it() {
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let lease = StatusLease::spawn_in(
            registry.clone(),
            InstanceKind::Cli,
            "1",
            "/tmp/workspace.yml",
            || async { StatusSnapshot::default() },
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let id = lease.instance_id().to_string();
        assert!(registry.list().unwrap().iter().any(|s| s.instance_id == id));

        drop(lease);
        assert!(!registry.list().unwrap().iter().any(|s| s.instance_id == id));
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn registry_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = std::env::temp_dir().join(format!("mq-bridge-status-test-{}", Uuid::new_v4()));
        let registry = LocalStatusRegistry::new_in(&temp).unwrap();
        let status = InstanceStatus::new(InstanceKind::WebUi, "1", "/tmp/workspace.yml");
        registry.publish(&status).unwrap();
        assert_eq!(
            fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(temp.join(format!("{}.json", status.instance_id)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(temp);
    }
}
