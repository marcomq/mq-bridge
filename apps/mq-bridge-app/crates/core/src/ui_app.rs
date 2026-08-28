use crate::config::{
    AppConfig, ConfigSecurityMode, ConsumerConfig, ConsumerMessageCaptureConfig,
    ConsumerOutputConfig, ConsumerResponseConfig, EnvFileSecretStore, PublisherClient, SecretStore,
};
use crate::encrypted_config::has_config_master_key;
use crate::route_metrics::{
    CAPTURE_SOURCE_KEY, CAPTURE_TIME_KEY, MessageCapture, RouteMetrics, is_redelivery,
};
use crate::status_registry::{
    InstanceKind, InstanceStatus, LocalStatusRegistry, StatusEntity, StatusLease, StatusRoute,
    StatusSnapshot, StatusSummary, endpoint_type_label, list_off_thread,
};
use anyhow::{Result, anyhow};
use chrono;
use metrics_exporter_prometheus::PrometheusHandle;
use mq_bridge::models::{Endpoint, EndpointType, Route, StaticConfig};
use mq_bridge::route::{RouteHandle, RouteOutcome};
use mq_bridge::{
    CanonicalMessage, Handled, HandlerError, Publisher, Sent, msg, unregister_publisher,
};
use schemars::JsonSchema;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

fn generate_ephemeral_message_key() -> (String, String) {
    use aes_gcm::aead::Generate;
    let bytes = <[u8; 32]>::generate();
    let kid = Uuid::new_v4().to_string();
    (hex::encode(bytes), kid)
}

static EPHEMERAL_MESSAGE_KEY: LazyLock<(String, String)> =
    LazyLock::new(generate_ephemeral_message_key);

const RUNTIME_STATUS_CACHE_TTL: Duration = Duration::from_secs(2);
const RUNTIME_STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub fn storage_security_for_cli(config: &AppConfig) -> StorageSecurityInfoResponse {
    match config.security_mode() {
        ConfigSecurityMode::Unencrypted => StorageSecurityInfoResponse {
            target: "cli".to_string(),
            encrypted: false,
            persistent: true,
            key_source: "none".to_string(),
            key_store_available: false,
            encrypted_config_available: has_config_master_key(),
            persistent_messages_available: false,
            config_encrypted: false,
            messages_encrypted: false,
            messages_persistent: true,
            reason: Some("cli-mode".to_string()),
            message_key_hex: None,
            kid: None,
        },
        ConfigSecurityMode::Balanced => StorageSecurityInfoResponse {
            target: "cli".to_string(),
            encrypted: false,
            persistent: true,
            key_source: "env".to_string(),
            key_store_available: false,
            encrypted_config_available: has_config_master_key(),
            persistent_messages_available: false,
            config_encrypted: false,
            messages_encrypted: false,
            messages_persistent: true,
            reason: Some("cli-mode".to_string()),
            message_key_hex: None,
            kid: None,
        },
        ConfigSecurityMode::EnvTemporaryMessages | ConfigSecurityMode::TemporaryMessages => {
            let (message_key_hex, kid) = &*EPHEMERAL_MESSAGE_KEY;
            StorageSecurityInfoResponse {
                target: "cli".to_string(),
                encrypted: true,
                persistent: false,
                key_source: "ephemeral-process".to_string(),
                key_store_available: false,
                encrypted_config_available: has_config_master_key(),
                persistent_messages_available: false,
                config_encrypted: false,
                messages_encrypted: true,
                messages_persistent: false,
                reason: Some("cli-mode".to_string()),
                message_key_hex: Some(message_key_hex.clone()),
                kid: Some(kid.clone()),
            }
        }
        ConfigSecurityMode::Sensitive | ConfigSecurityMode::Durable => {
            let (message_key_hex, kid) = &*EPHEMERAL_MESSAGE_KEY;
            StorageSecurityInfoResponse {
                target: "cli".to_string(),
                encrypted: true,
                persistent: false,
                key_source: "ephemeral-process".to_string(),
                key_store_available: false,
                encrypted_config_available: has_config_master_key(),
                persistent_messages_available: false,
                config_encrypted: has_config_master_key(),
                messages_encrypted: true,
                messages_persistent: false,
                reason: Some("cli-mode".to_string()),
                message_key_hex: Some(message_key_hex.clone()),
                kid: Some(kid.clone()),
            }
        }
    }
}

type StorageSecurityResolver =
    dyn Fn(&AppConfig) -> StorageSecurityInfoResponse + Send + Sync + 'static;
type StorageSavePrepare = dyn Fn(&AppConfig) -> anyhow::Result<()> + Send + Sync + 'static;
type ConfigRecoveryReset =
    dyn Fn(&AppConfig) -> anyhow::Result<ConfigRecoveryResetResponse> + Send + Sync + 'static;

#[derive(Default)]
pub struct UiAppRuntimeHooks {
    storage_security_resolver: Option<Arc<StorageSecurityResolver>>,
    storage_save_prepare: Option<Arc<StorageSavePrepare>>,
    config_recovery: Option<ConfigRecoveryStatusResponse>,
    config_recovery_reset: Option<Arc<ConfigRecoveryReset>>,
    instance_kind: InstanceKind,
}

impl UiAppRuntimeHooks {
    pub fn for_cli() -> Self {
        Self {
            storage_security_resolver: Some(Arc::new(storage_security_for_cli)),
            instance_kind: InstanceKind::Cli,
            ..Self::default()
        }
    }

    pub fn with_storage_security_resolver(
        mut self,
        resolver: Arc<StorageSecurityResolver>,
    ) -> Self {
        self.storage_security_resolver = Some(resolver);
        self
    }

    pub fn with_storage_save_prepare(mut self, prepare: Arc<StorageSavePrepare>) -> Self {
        self.storage_save_prepare = Some(prepare);
        self
    }

    pub fn with_config_recovery(mut self, recovery: Option<ConfigRecoveryStatusResponse>) -> Self {
        self.config_recovery = recovery;
        self
    }

    pub fn with_config_recovery_reset(mut self, reset: Option<Arc<ConfigRecoveryReset>>) -> Self {
        self.config_recovery_reset = reset;
        self
    }

    pub fn with_instance_kind(mut self, kind: InstanceKind) -> Self {
        self.instance_kind = kind;
        self
    }
}

#[derive(Clone)]
pub struct UiApp {
    config: Arc<RwLock<AppConfig>>,
    trusted_plugin_paths: Arc<HashSet<PathBuf>>,
    metrics_handle: PrometheusHandle,
    config_file_path: Arc<String>,
    secret_store: Arc<dyn SecretStore>,
    ui_handles: Arc<RwLock<HashMap<String, RouteHandle>>>,
    /// Per-consumer message counters and their smoothed throughput. Shared with
    /// the MCP server via [`crate::route_metrics`] so both report the same rate.
    metrics: RouteMetrics,
    /// `None` when no private per-user runtime directory exists. Presence
    /// reporting is best-effort: the UI runs unchanged without it.
    status_registry: Option<Arc<LocalStatusRegistry>>,
    instance_kind: InstanceKind,
    status_lease: Arc<StdRwLock<Option<StatusLease>>>,
    storage_security: Arc<StdRwLock<StorageSecurityInfoResponse>>,
    storage_security_resolver: Option<Arc<StorageSecurityResolver>>,
    storage_save_prepare: Option<Arc<StorageSavePrepare>>,
    config_recovery: Arc<StdRwLock<Option<ConfigRecoveryStatusResponse>>>,
    config_recovery_reset: Option<Arc<ConfigRecoveryReset>>,
    /// The last `/runtime-status` result and when it was produced. Building one
    /// probes every stopped consumer's endpoint, so the status heartbeat reuses
    /// what the UI's polling already paid for instead of reconnecting every tick.
    runtime_status_cache: Arc<StdRwLock<Option<(Instant, RuntimeStatusResponse)>>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, JsonSchema)]
pub struct RuntimeStatusResponse {
    pub active_consumers: Vec<String>,
    pub active_routes: Vec<String>,
    pub route_throughput: HashMap<String, f64>,
    pub consumers: HashMap<String, ConsumerStatusSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct PeerStatusResponse {
    pub current_instance_id: String,
    pub instances: Vec<InstanceStatus>,
}

/// Runtime status of one configured consumer. MCP status is converted directly
/// into the separate, sanitized registry schema before it leaves that process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ConsumerStatusSnapshot {
    pub running: bool,
    pub status: EndpointStatusSnapshot,
    pub throughput: f64,
    pub message_sequence: u64,
    pub capture_enabled: bool,
    pub capture_keep_last: usize,
    /// How the route ended, or `None` while it is still running.
    ///
    /// `running: false` alone cannot tell a drained `exit_on_empty` job from one
    /// that was stopped by hand or died on a permanent error — all three leave a
    /// grey badge. `#[serde(default)]` so reports from an older MCP process,
    /// which never sends the field, still deserialize.
    #[serde(default)]
    pub outcome: Option<RouteOutcomeSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct ConsumerStatusResponse {
    pub running: bool,
    pub status: EndpointStatusSnapshot,
    /// See [`ConsumerStatusSnapshot::outcome`].
    pub outcome: Option<RouteOutcomeSnapshot>,
}

/// How a route's task ended, mirroring [`mq_bridge::route::RouteOutcome`].
///
/// Mirrored rather than re-exported because this crosses the UI type generator;
/// the core enum derives neither. The wire representation is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouteOutcomeSnapshot {
    /// The source drained and the route exited on its own. The job succeeded.
    Completed,
    /// Terminated by an explicit stop or shutdown.
    Stopped,
    /// Terminated by a permanent error; the cause is in `status.error`.
    Failed,
}

impl From<RouteOutcome> for RouteOutcomeSnapshot {
    fn from(outcome: RouteOutcome) -> Self {
        match outcome {
            RouteOutcome::Completed => Self::Completed,
            RouteOutcome::Stopped => Self::Stopped,
            RouteOutcome::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct EndpointStatusSnapshot {
    pub healthy: bool,
    pub target: String,
    #[serde(default)]
    pub pending: Option<usize>,
    #[serde(default)]
    pub capacity: Option<usize>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub details: serde_json::Value,
}

impl From<mq_bridge::traits::EndpointStatus> for EndpointStatusSnapshot {
    fn from(status: mq_bridge::traits::EndpointStatus) -> Self {
        Self {
            healthy: status.healthy,
            target: status.target,
            pending: status.pending,
            capacity: status.capacity,
            error: status.error,
            details: status.details,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct PublishRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub publisher_id: Option<String>,
    pub payload: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub endpoint: Option<mq_bridge::models::Endpoint>,
}

#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct PublishResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ConfigRecoveryStatusResponse {
    pub mode: Option<String>,
    pub reason: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ConfigRecoveryResetResponse {
    pub backup_path: String,
}

trait CanonicalMessageExt {
    fn with_content_type(self, content_type: impl Into<String>) -> Self;
    fn with_status_code(self, status_code: impl Into<String>) -> Self;
}

impl CanonicalMessageExt for CanonicalMessage {
    fn with_content_type(self, content_type: impl Into<String>) -> Self {
        self.with_metadata_kv("Content-Type", content_type)
    }

    fn with_status_code(self, status_code: impl Into<String>) -> Self {
        self.with_metadata_kv("http_status_code", status_code)
    }
}

fn sanitize_relative_path(request_path: &str) -> Option<PathBuf> {
    let mut sanitized = PathBuf::new();

    for component in Path::new(request_path).components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if sanitized.as_os_str().is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// The web UI bundle, embedded into the binary at compile time so a single
/// `cargo install`ed executable can serve the UI without shipping a `static/`
/// directory. In debug builds rust-embed reads the files from disk (live edit);
/// in release builds the bytes are baked into the binary.
#[derive(rust_embed::RustEmbed)]
#[folder = "static/"]
struct StaticAssets;

/// Maps an HTTP request path to a key inside the embedded [`StaticAssets`]
/// bundle. Returns `None` for paths that try to escape the asset root.
fn embedded_asset_key(request_path: &str) -> Option<String> {
    if request_path == "/" || request_path.is_empty() {
        return Some("index.html".to_string());
    }

    let relative = request_path.trim_start_matches('/');
    // rust-embed keys are always forward-slash separated.
    sanitize_relative_path(relative).map(|path| path.to_string_lossy().replace('\\', "/"))
}

/// Resolves a `/node_modules/...` request to an on-disk path. This only matters
/// for the dev server; production bundles inline their dependencies, so an
/// installed binary never serves from here.
#[cfg(debug_assertions)]
fn resolve_node_modules_path(request_path: &str) -> Option<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let relative = request_path.strip_prefix("/node_modules/")?;
    sanitize_relative_path(relative).map(|path| workspace_root.join("node_modules").join(path))
}

fn query_param(msg: &CanonicalMessage, key: &str) -> Option<String> {
    let query = msg.metadata.get("http_query")?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .and_then(|raw| {
            url::form_urlencoded::parse(format!("{key}={raw}").as_bytes())
                .into_owned()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
        })
}

fn request_peer_is_loopback(msg: &CanonicalMessage) -> bool {
    msg.metadata
        .get("mqb_peer_loopback")
        .is_some_and(|value| value == "true")
}

impl From<&ConsumerStatusSnapshot> for StatusSummary {
    fn from(snapshot: &ConsumerStatusSnapshot) -> Self {
        Self {
            running: snapshot.running,
            healthy: snapshot.status.healthy,
            pending: snapshot.status.pending,
            capacity: snapshot.status.capacity,
            error: snapshot.status.error.clone(),
            throughput: snapshot.throughput,
            message_sequence: snapshot.message_sequence,
        }
    }
}

fn header_value<'a>(msg: &'a CanonicalMessage, key: &str) -> Option<&'a str> {
    msg.metadata
        .get(key)
        .or_else(|| msg.metadata.get(&key.to_ascii_lowercase()))
        .map(String::as_str)
}

fn extract_authority(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let after_scheme = if let Some((_, remainder)) = trimmed.split_once("://") {
        remainder
    } else {
        trimmed
    };

    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();

    if authority.is_empty() {
        None
    } else {
        Some(authority)
    }
}

fn is_same_origin_request(msg: &CanonicalMessage) -> bool {
    let Some(host) = header_value(msg, "Host").and_then(extract_authority) else {
        return true;
    };

    for header_name in ["Origin", "Referer"] {
        let Some(header) = header_value(msg, header_name) else {
            continue;
        };
        let Some(authority) = extract_authority(header) else {
            return false;
        };
        if !authority.eq_ignore_ascii_case(host) {
            return false;
        }
    }

    true
}

/// The key a consumer is known by at runtime: its id, or its name when it has
/// none. Shared with the CLI so both surfaces report the same identity.
pub fn consumer_runtime_key(consumer: &ConsumerConfig) -> String {
    let trimmed_id = consumer.id.trim();
    if trimmed_id.is_empty() {
        consumer.name.trim().to_string()
    } else {
        trimmed_id.to_string()
    }
}

fn encode_collector_route_key(consumer_key: &str) -> String {
    let mut encoded = String::with_capacity(consumer_key.len() * 2);
    for byte in consumer_key.as_bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn decode_collector_route_key(encoded: &str) -> Option<String> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }

    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    let mut index = 0;
    while index < encoded.len() {
        let next = index + 2;
        let value = u8::from_str_radix(&encoded[index..next], 16).ok()?;
        bytes.push(value);
        index = next;
    }

    String::from_utf8(bytes).ok()
}

/// The mq-bridge route name a consumer runs under, which is what
/// [`mq_bridge::list_routes`] reports — not the consumer key itself.
pub fn collector_route_name(consumer_key: &str) -> String {
    format!(
        "ui_collector_route_{}",
        encode_collector_route_key(consumer_key)
    )
}

fn consumer_matches_lookup(consumer: &ConsumerConfig, lookup: &str) -> bool {
    consumer_runtime_key(consumer) == lookup || consumer.name == lookup
}

fn endpoint_supports_consumer_response(endpoint: &Endpoint) -> bool {
    matches!(
        endpoint.endpoint_type,
        EndpointType::Http(_)
            | EndpointType::Nats(_)
            | EndpointType::Memory(_)
            | EndpointType::Amqp(_)
            | EndpointType::MongoDb(_)
            | EndpointType::Mqtt(_)
            | EndpointType::ZeroMq(_)
            | EndpointType::Kafka(_)
    )
}

fn normalize_consumer_response(
    response: Option<ConsumerResponseConfig>,
) -> Option<ConsumerResponseConfig> {
    response.and_then(|mut response| {
        response.headers.retain(|key, value| {
            let trimmed_key = key.trim();
            let trimmed_value = value.trim();
            !trimmed_key.is_empty() && !trimmed_value.is_empty()
        });

        if response.headers.is_empty() && response.payload.trim().is_empty() {
            None
        } else {
            Some(response)
        }
    })
}

fn normalize_consumer_output(
    output: ConsumerOutputConfig,
    fallback_response: Option<ConsumerResponseConfig>,
) -> ConsumerOutputConfig {
    let normalized_fallback = normalize_consumer_response(fallback_response);

    match output {
        ConsumerOutputConfig::Publisher {
            publisher,
            publisher_id,
        } => {
            let publisher = publisher.trim().to_string();
            ConsumerOutputConfig::Publisher {
                publisher,
                publisher_id,
            }
        }
        ConsumerOutputConfig::Response { response } => ConsumerOutputConfig::Response {
            response: normalize_consumer_response(response).or(normalized_fallback),
        },
        ConsumerOutputConfig::None => {
            if let Some(response) = normalized_fallback {
                ConsumerOutputConfig::Response {
                    response: Some(response),
                }
            } else {
                ConsumerOutputConfig::None
            }
        }
    }
}

fn consumer_output_response_compat(
    output: &ConsumerOutputConfig,
) -> Option<ConsumerResponseConfig> {
    match output {
        ConsumerOutputConfig::Response { response } => {
            normalize_consumer_response(response.clone())
        }
        _ => None,
    }
}

fn normalize_consumer_capture(
    capture: ConsumerMessageCaptureConfig,
) -> ConsumerMessageCaptureConfig {
    ConsumerMessageCaptureConfig {
        enabled: capture.enabled,
        keep_last: capture.keep_last.max(1),
    }
}

#[derive(Clone)]
enum ResolvedConsumerOutput {
    None,
    Publisher {
        endpoint: Arc<Endpoint>,
    },
    Response {
        response: Option<Arc<ConsumerResponseConfig>>,
    },
}

struct CollectorContext {
    source_key: String,
    capture: MessageCapture,
    counter: Arc<AtomicU64>,
    output: ResolvedConsumerOutput,
    capture_enabled: bool,
}

fn resolve_consumer_output(
    consumer: &ConsumerConfig,
    publishers: &[PublisherClient],
) -> std::result::Result<ResolvedConsumerOutput, UpdateConfigError> {
    match &consumer.output {
        ConsumerOutputConfig::None => Ok(ResolvedConsumerOutput::None),
        ConsumerOutputConfig::Response { response } => {
            if !endpoint_supports_consumer_response(&consumer.endpoint) {
                return Err(UpdateConfigError::UnsupportedCustomResponses(format!(
                    "Consumer {}: custom responses are not supported for endpoint type {}",
                    consumer.name,
                    consumer.endpoint.endpoint_type.name()
                )));
            }

            Ok(ResolvedConsumerOutput::Response {
                response: normalize_consumer_response(response.clone()).map(Arc::new),
            })
        }
        ConsumerOutputConfig::Publisher {
            publisher,
            publisher_id,
        } => {
            let publisher_name = publisher.trim();
            let publisher_id = publisher_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty());
            if publisher_name.is_empty() && publisher_id.is_none() {
                return Err(UpdateConfigError::Validation(format!(
                    "Consumer {}: publisher output requires a selected publisher",
                    consumer.name
                )));
            }
            let Some(publisher_config) = publishers.iter().find(|candidate| {
                publisher_id.is_some_and(|id| candidate.id == id)
                    || (!publisher_name.is_empty() && candidate.name == publisher_name)
            }) else {
                return Err(UpdateConfigError::Validation(format!(
                    "Consumer {}: referenced publisher not found: {}",
                    consumer.name,
                    publisher_id.unwrap_or(publisher_name)
                )));
            };

            Ok(ResolvedConsumerOutput::Publisher {
                endpoint: Arc::new(publisher_config.endpoint.clone()),
            })
        }
    }
}

impl UiApp {
    /// Override the advertised process kind for callers using one of the
    /// compatibility constructors. Must be set before
    /// [`Self::spawn_status_registry_publisher`], which is what creates the lease.
    pub fn with_instance_kind(mut self, kind: InstanceKind) -> Self {
        self.instance_kind = kind;
        self
    }

    pub fn new(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
    ) -> Result<Self> {
        let storage_security = storage_security_for_cli(&initial_config);
        Self::new_internal(
            initial_config,
            metrics_handle,
            config_file_path,
            Arc::new(EnvFileSecretStore::new(".env")),
            storage_security,
            UiAppRuntimeHooks::for_cli(),
            &[],
        )
    }

    pub fn new_with_startup_plugins(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        startup_plugins: &[String],
    ) -> Result<Self> {
        let storage_security = storage_security_for_cli(&initial_config);
        Self::new_internal(
            initial_config,
            metrics_handle,
            config_file_path,
            Arc::new(EnvFileSecretStore::new(".env")),
            storage_security,
            UiAppRuntimeHooks::for_cli(),
            startup_plugins,
        )
    }

    pub fn new_with_secret_store(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        secret_store: Arc<dyn SecretStore>,
    ) -> Result<Self> {
        Self::new_internal(
            initial_config.clone(),
            metrics_handle,
            config_file_path,
            secret_store,
            storage_security_for_cli(&initial_config),
            UiAppRuntimeHooks::for_cli(),
            &[],
        )
    }

    pub fn new_with_secret_store_and_storage_security(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        secret_store: Arc<dyn SecretStore>,
        storage_security: StorageSecurityInfoResponse,
    ) -> Result<Self> {
        Self::new_internal(
            initial_config,
            metrics_handle,
            config_file_path,
            secret_store,
            storage_security,
            UiAppRuntimeHooks::default(),
            &[],
        )
    }

    pub fn new_with_secret_store_and_storage_hooks(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        secret_store: Arc<dyn SecretStore>,
        storage_security: StorageSecurityInfoResponse,
        storage_security_resolver: Arc<StorageSecurityResolver>,
        storage_save_prepare: Arc<StorageSavePrepare>,
    ) -> Result<Self> {
        Self::new_internal(
            initial_config,
            metrics_handle,
            config_file_path,
            secret_store,
            storage_security,
            UiAppRuntimeHooks::default()
                .with_storage_security_resolver(storage_security_resolver)
                .with_storage_save_prepare(storage_save_prepare),
            &[],
        )
    }

    pub fn new_with_secret_store_and_runtime_hooks(
        initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        secret_store: Arc<dyn SecretStore>,
        storage_security: StorageSecurityInfoResponse,
        runtime_hooks: UiAppRuntimeHooks,
    ) -> Result<Self> {
        Self::new_internal(
            initial_config,
            metrics_handle,
            config_file_path,
            secret_store,
            storage_security,
            runtime_hooks,
            &[],
        )
    }

    fn new_internal(
        mut initial_config: AppConfig,
        metrics_handle: PrometheusHandle,
        config_file_path: String,
        secret_store: Arc<dyn SecretStore>,
        storage_security: StorageSecurityInfoResponse,
        runtime_hooks: UiAppRuntimeHooks,
        startup_plugins: &[String],
    ) -> Result<Self> {
        initial_config.migrate_legacy_security_mode();
        let mut trusted_plugin_paths = crate::plugins::load_trusted_plugins(
            &initial_config.plugins,
            &initial_config.env_vars,
        )?
        .into_iter()
        .map(|plugin| plugin.path)
        .collect::<HashSet<_>>();
        trusted_plugin_paths.extend(
            crate::plugins::load_trusted_plugins(startup_plugins, &initial_config.env_vars)?
                .into_iter()
                .map(|plugin| plugin.path),
        );
        initial_config
            .plugins
            .extend(startup_plugins.iter().cloned());
        let app = Self {
            config: Arc::new(RwLock::new(initial_config)),
            trusted_plugin_paths: Arc::new(trusted_plugin_paths),
            metrics_handle,
            config_file_path: Arc::new(config_file_path.clone()),
            secret_store,
            ui_handles: Arc::new(RwLock::new(HashMap::new())),
            metrics: RouteMetrics::new(),
            status_registry: LocalStatusRegistry::new()
                .inspect_err(|error| tracing::debug!("local status registry unavailable: {error}"))
                .ok()
                .map(Arc::new),
            instance_kind: runtime_hooks.instance_kind,
            status_lease: Arc::new(StdRwLock::new(None)),
            storage_security: Arc::new(StdRwLock::new(storage_security)),
            storage_security_resolver: runtime_hooks.storage_security_resolver,
            storage_save_prepare: runtime_hooks.storage_save_prepare,
            config_recovery: Arc::new(StdRwLock::new(runtime_hooks.config_recovery)),
            config_recovery_reset: runtime_hooks.config_recovery_reset,
            runtime_status_cache: Arc::new(StdRwLock::new(None)),
        };

        app.ensure_throughput_updater();

        Ok(app)
    }

    /// Spawns a background task to periodically update throughput metrics for all active consumers if not already started.
    fn ensure_throughput_updater(&self) {
        self.metrics.ensure_updater();
    }

    pub async fn get_config(&self) -> AppConfig {
        let mut config = self.config.read().await.clone();
        config.migrate_legacy_security_mode();
        config
    }

    pub fn storage_security(&self) -> StorageSecurityInfoResponse {
        self.storage_security
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn config_recovery(&self) -> Option<ConfigRecoveryStatusResponse> {
        self.config_recovery
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn config_file_path(&self) -> &str {
        self.config_file_path.as_str()
    }

    pub async fn handle_ui_message(
        &self,
        msg: CanonicalMessage,
        serve_static_assets: bool,
    ) -> Result<Handled, HandlerError> {
        self.ensure_throughput_updater();

        let method = msg
            .metadata
            .get("http_method")
            .map(|s| s.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let raw_path = msg
            .metadata
            .get("http_path")
            .map(|s| s.as_str())
            .unwrap_or("/");

        // Strip anything after '#' to allow the UI to send "visible identifiers"
        // that appear in the browser network tab but are ignored for routing.
        let routing_path = raw_path.split('#').next().unwrap_or("/").to_lowercase();
        // Use the original case for parameters if needed, but lowercase for routing
        let path = routing_path.as_str();

        if method == "POST"
            && matches!(
                path,
                "/config"
                    | "/config-recovery/reset"
                    | "/publish"
                    | "/consumer-start"
                    | "/consumer-stop"
            )
            && !is_same_origin_request(&msg)
        {
            return Ok(Handled::Publish(
                msg!("Forbidden")
                    .with_status_code("403")
                    .with_content_type("text/plain; charset=utf-8"),
            ));
        }

        let result = match (method.as_str(), path) {
            ("GET", "/health") => Ok(Handled::Publish(msg!("OK"))),
            ("GET", "/schema.json") => {
                let schema = crate::config::app_config_schema();
                self.ok_json(&schema, false)
            }
            ("GET", "/config") => self.ok_json(&self.get_config().await, false),
            ("GET", "/config-recovery") => self.ok_json(&self.config_recovery(), true),
            ("GET", "/storage-security") => self.ok_json(&self.storage_security(), true),
            ("GET", "/features") => self.ok_json(&FeatureAvailabilityResponse::detect(), true),
            ("GET", "/consumer-status") => {
                let consumer_key = query_param(&msg, "consumer_id")
                    .or_else(|| query_param(&msg, "consumer"))
                    .unwrap_or_default();
                match self.consumer_status(&consumer_key).await {
                    Some(status) => self.ok_json(&status, false),
                    None => self.err_response(404, format!("Consumer not found: {consumer_key}")),
                }
            }
            ("POST", "/consumer-start") => {
                let consumer_key = query_param(&msg, "consumer_id")
                    .or_else(|| query_param(&msg, "consumer"))
                    .unwrap_or_default();
                match self.start_consumer(&consumer_key).await {
                    Ok(true) => Ok(Handled::Publish(msg!("Started"))),
                    Ok(false) => {
                        self.err_response(404, format!("Consumer not found: {consumer_key}"))
                    }
                    Err(e) => self.err_response(500, e.to_string()),
                }
            }
            ("POST", "/consumer-stop") => {
                let consumer_key = query_param(&msg, "consumer_id")
                    .or_else(|| query_param(&msg, "consumer"))
                    .unwrap_or_default();
                self.stop_consumer(&consumer_key).await;
                Ok(Handled::Publish(msg!("Stopped")))
            }
            ("GET", "/messages") => {
                let consumer =
                    query_param(&msg, "consumer_id").or_else(|| query_param(&msg, "consumer"));
                self.ok_json(&self.get_messages(consumer.as_deref()).await, true)
            }
            ("POST", "/config") => self.handle_update_config_message(msg).await,
            ("POST", "/config-recovery/reset") => self.handle_reset_config_recovery().await,
            ("POST", "/publish") => self.handle_publish_message(msg).await,
            ("GET", "/runtime-status") => self.ok_json(&self.runtime_status().await, true),
            ("GET", "/peer-status") => {
                if !request_peer_is_loopback(&msg) {
                    self.err_response(403, "Peer status is available only to loopback clients")
                } else {
                    self.ok_json(&self.peer_status().await, true)
                }
            }
            ("GET", "/metrics") => Ok(Handled::Publish(
                CanonicalMessage::from(self.render_metrics())
                    .with_content_type("text/plain; version=0.0.4"),
            )),
            ("GET", _) if serve_static_assets => self.handle_static_asset(raw_path),
            _ => Ok(Handled::Publish(msg!("Not Found").with_status_code("404"))),
        };

        let handled = match result {
            Ok(h) => h,
            Err(e) => Handled::Publish(
                msg!(format!("Internal Server Error: {}", e)).with_status_code("500"),
            ),
        };

        let response = match handled {
            Handled::Ack => msg!("OK"),
            Handled::Publish(m) => m,
        };

        let response = if !response.metadata.contains_key("http_status_code") {
            response.with_status_code("200")
        } else {
            response
        };

        Ok(Handled::Publish(response))
    }

    fn ok_json<T: serde::Serialize>(
        &self,
        value: &T,
        no_cache: bool,
    ) -> Result<Handled, HandlerError> {
        let mut m = CanonicalMessage::from_type(value)
            .map_err(|e| HandlerError::NonRetryable(e.into()))?
            .with_content_type("application/json");
        if no_cache {
            m = m.with_metadata_kv("Cache-Control", "no-cache, no-store, must-revalidate");
        }
        Ok(Handled::Publish(m))
    }

    fn err_response(
        &self,
        status: u16,
        message: impl Into<String>,
    ) -> Result<Handled, HandlerError> {
        let msg_str = message.into();
        self.ok_json(&serde_json::json!({ "error": msg_str }), false)
            .map(|h| match h {
                Handled::Publish(m) => Handled::Publish(m.with_status_code(status.to_string())),
                other => other,
            })
    }

    pub fn render_metrics(&self) -> String {
        self.metrics_handle.render()
    }

    pub async fn consumer_status(&self, consumer_key: &str) -> Option<ConsumerStatusResponse> {
        let config = self.config.read().await;
        let consumer_config = config
            .consumers
            .iter()
            .find(|c| consumer_matches_lookup(c, consumer_key));

        consumer_config
            .map(|c| async move { self.consumer_status_snapshot(c).await })?
            .await
            .map(|snapshot| ConsumerStatusResponse {
                running: snapshot.running,
                status: snapshot.status,
                outcome: snapshot.outcome,
            })
    }

    pub async fn start_consumer(&self, consumer_key: &str) -> Result<bool> {
        let consumer_config = {
            let config = self.config.read().await;
            config
                .consumers
                .iter()
                .find(|c| consumer_matches_lookup(c, consumer_key))
                .cloned()
                .map(|consumer| (consumer, config.publishers.clone()))
        };

        if let Some((consumer, publishers)) = consumer_config {
            self.start_ui_collector_routes(&[(consumer, publishers)])
                .await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Starts every enabled consumer, logging and skipping failures rather than
    /// aborting the rest. Returns the number started so every caller can report
    /// partial or complete startup failure appropriately.
    pub async fn start_configured_consumers(&self) -> usize {
        let consumers = {
            let config = self.config.read().await;
            config
                .consumers
                .iter()
                .filter(|consumer| consumer.enabled)
                .cloned()
                .map(|consumer| (consumer, config.publishers.clone()))
                .collect::<Vec<_>>()
        };

        let mut started = 0;
        for entry in &consumers {
            match self
                .start_ui_collector_routes(std::slice::from_ref(entry))
                .await
            {
                Ok(()) => started += 1,
                Err(e) => tracing::error!("Consumer '{}' failed to start: {:#}", entry.0.name, e),
            }
        }
        if !consumers.is_empty() {
            tracing::info!(
                "Started {started} of {} configured consumer(s)",
                consumers.len()
            );
        }
        started
    }

    pub async fn stop_consumer(&self, consumer_key: &str) -> bool {
        let resolved_consumer_key = {
            let config = self.config.read().await;
            config
                .consumers
                .iter()
                .find(|c| consumer_matches_lookup(c, consumer_key))
                .map(consumer_runtime_key)
        };
        let handle_key = resolved_consumer_key.as_deref().unwrap_or(consumer_key);
        let mut handles = self.ui_handles.write().await;
        if let Some(handle) = handles.remove(handle_key) {
            handle.stop().await;
            true
        } else {
            false
        }
    }

    pub async fn get_messages(
        &self,
        target_consumer: Option<&str>,
    ) -> HashMap<String, VecDeque<serde_json::Value>> {
        let mut grouped_messages: HashMap<String, VecDeque<serde_json::Value>> = HashMap::new();

        if let Some(target_consumer) = target_consumer {
            let (consumer_key, keep_last) = {
                let config = self.config.read().await;
                config
                    .consumers
                    .iter()
                    .find(|consumer| consumer_matches_lookup(consumer, target_consumer))
                    .map(|consumer| {
                        (
                            consumer_runtime_key(consumer),
                            Some(consumer.message_capture.keep_last),
                        )
                    })
                    .unwrap_or_else(|| (target_consumer.to_string(), None))
            };
            let topic = format!("ui_collector_{consumer_key}");
            {
                // Reading can precede the collector route that writes, and the
                // underlying channel is created on first access — so open it
                // with the consumer's configured capacity rather than letting
                // the default one win and silently override `keep_last`.
                let capture = match keep_last {
                    Some(keep_last) => MessageCapture::with_capacity(&topic, keep_last),
                    None => MessageCapture::open(&topic),
                };
                for m in capture.drain() {
                    let mut metadata = m.metadata.clone();
                    let source = metadata
                        .remove(CAPTURE_SOURCE_KEY)
                        .unwrap_or_else(|| "unknown".into());

                    if let Some(capture_time_ms) = metadata.remove(CAPTURE_TIME_KEY) {
                        let id = fast_uuid_v7::format_uuid(m.message_id).to_string();
                        let time = capture_time_ms
                            .parse::<i64>()
                            .ok()
                            .and_then(chrono::DateTime::from_timestamp_millis)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default();

                        let response = metadata.remove("ui_response_payload");
                        let response_metadata_str = metadata.remove("ui_response_metadata");
                        let response_metadata: Option<serde_json::Value> =
                            response_metadata_str.and_then(|s| serde_json::from_str(&s).ok());

                        let payload_json: serde_json::Value = serde_json::from_slice(&m.payload)
                            .unwrap_or_else(|_| {
                                serde_json::Value::String(m.get_payload_str().to_string())
                            });

                        grouped_messages
                            .entry(source.clone())
                            .or_default()
                            .push_back(serde_json::json!({
                                "id": id,
                                "time": time,
                                "metadata": metadata,
                                "payload": payload_json,
                                "response": response,
                                "response_metadata": response_metadata
                            }));
                    } else {
                        let body: serde_json::Value =
                            serde_json::from_slice(&m.payload).unwrap_or_default();
                        grouped_messages.entry(source).or_default().push_back(body);
                    }
                }
            }
        }

        grouped_messages
    }

    pub async fn publish(&self, request: PublishRequest) -> Result<Option<PublishResponse>> {
        let PublishRequest {
            name,
            publisher_id,
            payload,
            metadata,
            endpoint,
        } = request;
        let publisher_lookup_key = publisher_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let trimmed_name = name.trim();
                (!trimmed_name.is_empty()).then(|| trimmed_name.to_string())
            });
        let endpoint = if let Some(ep) = endpoint {
            Some(ep)
        } else {
            let config = self.config.read().await;
            config
                .publishers
                .iter()
                .find(|publisher| {
                    publisher_lookup_key
                        .as_ref()
                        .is_some_and(|lookup| publisher.id == *lookup || publisher.name == *lookup)
                })
                .map(|p| p.endpoint.clone())
        };

        let publisher = if let Some(endpoint) = endpoint {
            let trimmed_name = name.trim();
            if !trimmed_name.is_empty() {
                unregister_publisher(trimmed_name);
            }
            match tokio::time::timeout(Duration::from_secs(5), Publisher::new(endpoint)).await {
                Ok(Ok(p)) => Some(p),
                Ok(Err(e)) => return Err(anyhow!("Failed to initialize publisher: {e}")),
                Err(_) => return Err(anyhow!("Publisher initialization timed out after 5s")),
            }
        } else {
            None
        };

        if let Some(publisher) = publisher {
            let mut canonical = CanonicalMessage::from(payload);
            for (k, v) in metadata {
                canonical.metadata.insert(k, v);
            }

            let response = match publisher.send(canonical).await {
                Ok(Sent::Ack) => PublishResponse {
                    status: "Ack".to_string(),
                    payload: None,
                    metadata: None,
                },
                Ok(Sent::Response(message)) => PublishResponse {
                    status: "Response".to_string(),
                    payload: Some(message.get_payload_str().to_string()),
                    metadata: Some(message.metadata),
                },
                Err(e) => return Err(anyhow!("Publish error: {e}")),
            };

            Ok(Some(response))
        } else {
            Ok(None)
        }
    }

    async fn handle_reset_config_recovery(&self) -> Result<Handled, HandlerError> {
        let Some(reset_hook) = self.config_recovery_reset.as_ref() else {
            return self.err_response(404, "Config recovery reset is not available");
        };
        if self.config_recovery().is_none() {
            return self.err_response(409, "No config recovery action is pending");
        }

        let current_config = self.get_config().await;
        let result = reset_hook(&current_config).map_err(HandlerError::NonRetryable)?;

        {
            let mut recovery = self
                .config_recovery
                .write()
                .unwrap_or_else(|error| error.into_inner());
            *recovery = None;
        }

        self.ok_json(&result, true)
    }

    pub async fn runtime_status(&self) -> RuntimeStatusResponse {
        let fallback = self
            .runtime_status_cache
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|(_, response)| response.clone())
            .unwrap_or_default();
        match tokio::time::timeout(RUNTIME_STATUS_PROBE_TIMEOUT, self.probe_runtime_status()).await
        {
            Ok(response) => {
                *self
                    .runtime_status_cache
                    .write()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some((Instant::now(), response.clone()));
                response
            }
            Err(_) => fallback,
        }
    }

    async fn probe_runtime_status(&self) -> RuntimeStatusResponse {
        let config = self.config.read().await;
        // Finished routes keep their handle so the outcome stays queryable, so
        // map membership is not liveness; a route with an outcome is done.
        let (finished_consumers, mut active_consumers): (HashSet<String>, Vec<String>) = {
            let handles = self.ui_handles.read().await;
            let (finished, active): (Vec<_>, Vec<_>) = handles
                .iter()
                .partition(|(_, handle)| handle.outcome().is_some());
            (
                finished.into_iter().map(|(key, _)| key.clone()).collect(),
                active.into_iter().map(|(key, _)| key.clone()).collect(),
            )
        };

        // Consumers may run as internal collector routes even when ui_handles does not currently
        // track them (for example after restarts). Surface those as active consumers too.
        let consumer_route_ids: Vec<String> = mq_bridge::list_routes()
            .into_iter()
            .filter_map(|runtime_id| {
                runtime_id
                    .strip_prefix("ui_collector_route_")
                    .and_then(decode_collector_route_key)
            })
            // The route registry keeps a route registered after its task ends, so
            // it would re-add a drained consumer here. Where we hold a handle, the
            // handle decides.
            .filter(|consumer_key| !finished_consumers.contains(consumer_key))
            .collect();
        active_consumers.extend(consumer_route_ids);
        let active_routes: Vec<String> = mq_bridge::list_routes()
            .into_iter() // Filter out internal UI routes
            .filter(|name| name != "web_ui" && !name.starts_with("ui_collector_route_"))
            .collect();

        let throughputs = self.metrics.throughputs().await;
        // Only consumers that are actually running: a configured-but-stopped
        // consumer keeps its metrics entry (nothing forgets it on stop), so
        // filtering by configuration alone would report it as a live route.
        let active_consumer_keys: HashSet<String> = active_consumers.iter().cloned().collect();
        let route_throughput: HashMap<String, f64> = throughputs
            .iter()
            .filter(|(key, _)| active_consumer_keys.contains(*key))
            .map(|(key, throughput)| (key.clone(), *throughput))
            .collect();

        active_consumers.sort();
        active_consumers.dedup();
        let mut active_routes = active_routes;
        active_routes.sort();

        let consumer_sequences = self.metrics.sequences().await;
        let mut consumers = HashMap::new();
        for consumer in &config.consumers {
            let consumer_key = consumer_runtime_key(consumer);
            let running = active_consumers.contains(&consumer_key);
            if let Some(snapshot) = self
                .consumer_status_snapshot_with_running(consumer, running)
                .await
            {
                let message_sequence = consumer_sequences.get(&consumer_key).copied().unwrap_or(0);
                let throughput = throughputs.get(&consumer_key).copied().unwrap_or(0.0);
                let status_snapshot = ConsumerStatusSnapshot {
                    throughput,
                    message_sequence,
                    capture_enabled: consumer.message_capture.enabled,
                    capture_keep_last: consumer.message_capture.keep_last,
                    ..snapshot
                };
                consumers.insert(consumer_key, status_snapshot);
            }
        }

        RuntimeStatusResponse {
            active_consumers,
            active_routes,
            route_throughput,
            consumers,
        }
    }

    /// The last runtime status while it is still fresh, so a
    /// heartbeat costs nothing whenever the UI is already polling. With nothing
    /// polling it falls back to computing one, which also refreshes the cache.
    async fn cached_runtime_status(&self) -> RuntimeStatusResponse {
        let cached = self
            .runtime_status_cache
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|(at, _)| at.elapsed() < RUNTIME_STATUS_CACHE_TTL)
            .map(|(_, response)| response.clone());
        match cached {
            Some(response) => response,
            None => self.runtime_status().await,
        }
    }

    /// What this process is currently running, in registry terms.
    ///
    /// Only entities with real runtime state carry live values. A configured
    /// publisher is a destination rather than a running thing, so it advertises
    /// the same idle summary the local publisher rows render.
    async fn status_snapshot(&self) -> StatusSnapshot {
        let runtime = self.cached_runtime_status().await;
        let config = self.config.read().await;
        StatusSnapshot {
            consumers: config
                .consumers
                .iter()
                .filter_map(|consumer| {
                    let key = consumer_runtime_key(consumer);
                    runtime.consumers.get(&key).map(|snapshot| StatusEntity {
                        id: key,
                        label: consumer.name.clone(),
                        endpoint: endpoint_type_label(&consumer.endpoint.endpoint_type).to_string(),
                        summary: StatusSummary::from(snapshot),
                    })
                })
                .collect(),
            publishers: config
                .publishers
                .iter()
                .map(|publisher| StatusEntity {
                    id: publisher.id.clone(),
                    label: publisher.name.clone(),
                    endpoint: endpoint_type_label(&publisher.endpoint.endpoint_type).to_string(),
                    summary: StatusSummary::default(),
                })
                .collect(),
            routes: runtime
                .active_routes
                .iter()
                .map(|name| {
                    // Membership in `active_routes` is what "running" means here;
                    // the rate comes from the same metrics the local rows use.
                    let summary = StatusSummary {
                        running: true,
                        healthy: true,
                        throughput: runtime.route_throughput.get(name).copied().unwrap_or(0.0),
                        ..Default::default()
                    };
                    let (input, output) = config
                        .routes
                        .get(name)
                        .map(|route| {
                            (
                                endpoint_type_label(&route.route.input.endpoint_type),
                                endpoint_type_label(&route.route.output.endpoint_type),
                            )
                        })
                        .unwrap_or(("unknown", "unknown"));
                    StatusRoute {
                        id: name.clone(),
                        label: name.clone(),
                        input: StatusEntity {
                            id: format!("{name}:input"),
                            label: name.clone(),
                            endpoint: input.to_string(),
                            summary: summary.clone(),
                        },
                        output: StatusEntity {
                            id: format!("{name}:output"),
                            label: name.clone(),
                            endpoint: output.to_string(),
                            summary: summary.clone(),
                        },
                        summary,
                    }
                })
                .collect(),
        }
    }

    /// Reads the registry. Deliberately does not publish: the heartbeat task
    /// owns that, and a poll-rate fsync on the read path buys nothing.
    pub async fn peer_status(&self) -> PeerStatusResponse {
        let current_instance_id = self
            .status_lease
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|lease| lease.instance_id().to_string())
            .unwrap_or_default();
        let instances = match &self.status_registry {
            Some(registry) => list_off_thread(registry).await,
            None => Vec::new(),
        };
        PeerStatusResponse {
            current_instance_id,
            instances,
        }
    }

    pub fn spawn_status_registry_publisher(&self) {
        let mut slot = self
            .status_lease
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_some() {
            return;
        }
        let app = self.clone();
        *slot = StatusLease::spawn(
            self.instance_kind,
            env!("CARGO_PKG_VERSION"),
            self.config_file_path(),
            move || {
                let app = app.clone();
                async move { app.status_snapshot().await }
            },
        );
    }

    /// Stops heartbeating and removes this process's lease.
    ///
    /// Still explicit rather than left to `Drop`: the heartbeat task holds a
    /// `UiApp` clone, so the `Arc` graph is cyclic and nothing here is dropped
    /// on its own.
    pub fn remove_status_registry_lease(&self) {
        if let Some(mut lease) = self
            .status_lease
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            lease.release();
        }
    }

    async fn consumer_status_snapshot(
        &self,
        consumer: &ConsumerConfig,
    ) -> Option<ConsumerStatusSnapshot> {
        // This path has no route registry to consult, so a tracked handle is the
        // only liveness there is.
        self.consumer_status_snapshot_with_running(consumer, false)
            .await
    }

    /// `running_untracked` is the caller's liveness for a consumer this app holds
    /// no [`RouteHandle`] for — one recovered from the route registry after a
    /// restart. Where a handle exists it decides, because map membership is not
    /// liveness: a route whose task ended on its own keeps its handle so the
    /// outcome stays queryable.
    async fn consumer_status_snapshot_with_running(
        &self,
        consumer: &ConsumerConfig,
        running_untracked: bool,
    ) -> Option<ConsumerStatusSnapshot> {
        let name = consumer.name.clone();

        let tracked = {
            let runtime_key = consumer_runtime_key(consumer);
            self.ui_handles
                .read()
                .await
                .get(&runtime_key)
                .map(|handle| {
                    (
                        handle.status(),
                        handle.outcome().map(RouteOutcomeSnapshot::from),
                    )
                })
        };

        let running = match &tracked {
            Some((_, outcome)) => outcome.is_none(),
            None => running_untracked,
        };
        let outcome = tracked.as_ref().and_then(|(_, outcome)| *outcome);

        let status = if let Some((handle_status, _)) = tracked {
            // Report from the handle whether the route is running or finished.
            // Running: prefer its live health so a consumer that connected then
            // dropped (or is stuck reconnecting) shows unhealthy with the last
            // error instead of a hardcoded green badge (issue #12). Finished: the
            // last health it recorded, and never the probe below — that would
            // open a fresh connection to a source the route already drained.
            //
            // Take live healthy/error/pending/capacity from the handle, but keep the
            // consumer's display name as `target` (the handle's target is the internal
            // collector route id, which should not leak to the UI).
            mq_bridge::traits::EndpointStatus {
                target: name.clone(),
                ..handle_status
            }
        } else if running {
            // Running as an internal collector route (e.g. after a restart)
            // with no live handle to inspect; best effort is healthy.
            mq_bridge::traits::EndpointStatus {
                healthy: true,
                target: name.clone(),
                ..Default::default()
            }
        } else if matches!(consumer.endpoint.endpoint_type, EndpointType::Http(_)) {
            mq_bridge::traits::EndpointStatus {
                healthy: false,
                target: name.clone(),
                ..Default::default()
            }
        } else if name.trim().is_empty() {
            mq_bridge::traits::EndpointStatus {
                healthy: false,
                target: "Unnamed Consumer".to_string(),
                ..Default::default()
            }
        } else {
            let status_future = consumer.endpoint.create_consumer(&name);
            match tokio::time::timeout(Duration::from_secs(2), status_future).await {
                Ok(Ok(endpoint)) => endpoint.status().await,
                Ok(Err(e)) => mq_bridge::traits::EndpointStatus {
                    healthy: false,
                    target: name.clone(),
                    error: Some(format!("Creation failed: {e}")),
                    ..Default::default()
                },
                Err(_) => mq_bridge::traits::EndpointStatus {
                    healthy: false,
                    target: name.clone(),
                    error: Some("Status check timed out".to_string()),
                    ..Default::default()
                },
            }
        };

        Some(ConsumerStatusSnapshot {
            running,
            status: status.into(),
            throughput: 0.0,
            message_sequence: 0,
            capture_enabled: consumer.message_capture.enabled,
            capture_keep_last: consumer.message_capture.keep_last,
            outcome,
        })
    }

    pub async fn update_config(
        &self,
        mut new_config: AppConfig,
    ) -> std::result::Result<(), UpdateConfigError> {
        tracing::info!("Received new configuration via Web UI. Reloading...");
        let requested_plugin_paths = crate::plugins::canonical_plugin_paths(
            &new_config.plugins,
            &new_config.env_vars,
        )
        .map_err(|error| {
            UpdateConfigError::Validation(format!(
                "{error:#}. Native plugins are startup-only; edit the trusted startup configuration and restart the application to change them"
            ))
        })?;
        if requested_plugin_paths != *self.trusted_plugin_paths {
            return Err(UpdateConfigError::Validation(
                "Native plugins are startup-only; edit the trusted startup configuration and restart the application to change them"
                    .to_string(),
            ));
        }
        new_config.migrate_legacy_routes();
        let consumers: Vec<crate::config::ConsumerConfig> = new_config
            .consumers
            .iter()
            .cloned()
            .map(|mut c| {
                c.name = c.name.trim().to_string();
                c.output = normalize_consumer_output(c.output, c.response.clone());
                c.response = consumer_output_response_compat(&c.output);
                c.message_capture = normalize_consumer_capture(c.message_capture);
                c
            })
            .collect();

        for consumer in &consumers {
            let resolved_output = resolve_consumer_output(consumer, &new_config.publishers)?;
            let output_endpoint = match &resolved_output {
                ResolvedConsumerOutput::None => Endpoint::null(),
                ResolvedConsumerOutput::Response { .. } => Endpoint::new_response(),
                ResolvedConsumerOutput::Publisher { endpoint, .. } => (*endpoint).as_ref().clone(),
            };
            let temp_route = Route::new(consumer.endpoint.clone(), output_endpoint);
            let consumer_key = consumer_runtime_key(consumer);
            temp_route.check(&consumer_key, None).map_err(|e| {
                UpdateConfigError::Validation(format!(
                    "Consumer {}: validation failed: {}",
                    if consumer.name.is_empty() {
                        consumer_key.as_str()
                    } else {
                        consumer.name.as_str()
                    },
                    e
                ))
            })?;
        }

        if let Some(prepare) = &self.storage_save_prepare {
            prepare(&new_config).map_err(|error| {
                UpdateConfigError::Other(anyhow!("Failed to prepare encrypted storage: {error}"))
            })?;
        }

        let old_config = self.config.read().await.clone();

        {
            let mut handles = self.ui_handles.write().await;
            let mut collectors_to_remove = Vec::new();

            for consumer_key in handles.keys() {
                let should_stop = if let (Some(old_consumer), Some(new_consumer)) = (
                    old_config
                        .consumers
                        .iter()
                        .find(|consumer| consumer_runtime_key(consumer) == *consumer_key),
                    consumers
                        .iter()
                        .find(|consumer| consumer_runtime_key(consumer) == *consumer_key),
                ) {
                    serde_json::to_value(old_consumer).unwrap()
                        != serde_json::to_value(new_consumer).unwrap()
                } else {
                    true
                };

                if should_stop {
                    collectors_to_remove.push(consumer_key.clone());
                }
            }

            for consumer_key in collectors_to_remove {
                if let Some(handle) = handles.remove(&consumer_key) {
                    handle.stop().await;
                }
            }
        }

        for consumer in &consumers {
            let resolved_output = resolve_consumer_output(consumer, &new_config.publishers)?;
            let output_endpoint = match &resolved_output {
                ResolvedConsumerOutput::None => Endpoint::null(),
                ResolvedConsumerOutput::Response { .. } => Endpoint::new_response(),
                ResolvedConsumerOutput::Publisher { endpoint, .. } => (*endpoint).as_ref().clone(),
            };
            let route = Route::new(consumer.endpoint.clone(), output_endpoint);
            if route.is_ref() {
                route.register_output_endpoint(None).map_err(|e| {
                    UpdateConfigError::RegisterOutputEndpoint(format!(
                        "register_output_endpoint failed: {e}"
                    ))
                })?;
            }
        }
        new_config.consumers = consumers.clone();

        let config_file = &*self.config_file_path;
        new_config
            .save_with_secret_store(config_file, self.secret_store.as_ref())
            .map_err(|e| {
                tracing::error!("Failed to save config to '{}': {}", config_file, e);
                UpdateConfigError::Other(anyhow!("Failed to save configuration: {e}"))
            })?;
        tracing::info!("Configuration saved to {}", config_file);

        if let Some(resolver) = &self.storage_security_resolver {
            let mut storage_security = self
                .storage_security
                .write()
                .unwrap_or_else(|error| error.into_inner());
            *storage_security = resolver(&new_config);
        }

        {
            let mut config_guard = self.config.write().await;
            *config_guard = new_config;
        }

        Ok(())
    }

    fn handle_static_asset(&self, request_path: &str) -> Result<Handled, HandlerError> {
        // Dev server only: serve node_modules from disk. Compiled out of release
        // builds so only debug builds can hit the filesystem here.
        #[cfg(debug_assertions)]
        if request_path.starts_with("/node_modules/") {
            let Some(file_path) = resolve_node_modules_path(request_path) else {
                return Ok(Handled::Publish(msg!("Not Found").with_status_code("404")));
            };
            return match std::fs::read(&file_path) {
                Ok(contents) => Ok(Handled::Publish(msg!(contents).with_content_type(
                    mq_bridge::endpoints::http::guess_content_type(&file_path.to_string_lossy()),
                ))),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Ok(Handled::Publish(msg!("Not Found").with_status_code("404")))
                }
                Err(err) => Err(HandlerError::NonRetryable(err.into())),
            };
        }

        // Everything else is served from the embedded UI bundle.
        let Some(key) = embedded_asset_key(request_path) else {
            return Ok(Handled::Publish(msg!("Not Found").with_status_code("404")));
        };

        match StaticAssets::get(&key) {
            Some(asset) => Ok(Handled::Publish(
                msg!(asset.data.into_owned())
                    .with_content_type(mq_bridge::endpoints::http::guess_content_type(&key)),
            )),
            None => Ok(Handled::Publish(msg!("Not Found").with_status_code("404"))),
        }
    }

    async fn handle_publish_message(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        let request: PublishRequest = match serde_json::from_slice(&msg.payload) {
            Ok(req) => req,
            Err(e) => {
                return Ok(Handled::Publish(
                    msg!(format!("Json deserialize error: {e}")).with_status_code("400"),
                ));
            }
        };

        let name = request
            .publisher_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| request.name.clone());
        match self.publish(request).await {
            Ok(Some(response)) => self.ok_json(&response, false),
            Ok(None) => self.err_response(404, format!("Publisher not found: {name}")),
            Err(e) => self.err_response(500, e.to_string()),
        }
    }

    async fn handle_update_config_message(
        &self,
        msg: CanonicalMessage,
    ) -> Result<Handled, HandlerError> {
        let body = msg.payload;
        let new_config: AppConfig = match serde_json::from_slice(&body) {
            Ok(cfg) => cfg,
            Err(e) => {
                let mut msg_str = format!("Json deserialize error: {}", e);
                if e.line() == 1 {
                    let col = e.column();
                    let idx = col.saturating_sub(1);
                    let len = body.len();
                    let start = idx.saturating_sub(30);
                    let end = (idx + 30).min(len);
                    let snippet = String::from_utf8_lossy(&body[start..end]);
                    msg_str.push_str(&format!("\nAt: ...{}...", snippet));
                }
                tracing::error!("{}", msg_str);
                return Ok(Handled::Publish(
                    CanonicalMessage::from(msg_str).with_status_code("400"),
                ));
            }
        };

        match self.update_config(new_config).await {
            Ok(()) => Ok(Handled::Publish(msg!("Configuration updated"))),
            Err(e) => {
                let status_code = match e {
                    UpdateConfigError::Other(_) => "500",
                    _ => "400",
                };
                Ok(Handled::Publish(
                    msg!(e.to_string()).with_status_code(status_code),
                ))
            }
        }
    }

    async fn start_ui_collector_routes(
        &self,
        consumers: &[(ConsumerConfig, Vec<PublisherClient>)],
    ) -> Result<()> {
        let mut handles = self.ui_handles.write().await;
        for (consumer, publishers) in consumers {
            if matches!(consumer.endpoint.endpoint_type, EndpointType::Null) {
                continue;
            }
            let consumer_key = consumer_runtime_key(consumer);
            // Starting a consumer that is already running is a no-op. Inserting a
            // second handle would drop the first, and `RouteHandle` has no `Drop`,
            // so the route behind it would keep consuming the same source
            // untracked — invisible to the UI and impossible to stop. A handle
            // whose task has ended is kept only so its outcome stays queryable,
            // and may be replaced by a restart.
            if handles
                .get(&consumer_key)
                .is_some_and(|handle| handle.outcome().is_none())
            {
                continue;
            }
            let topic = format!("ui_collector_{consumer_key}");
            let capture_enabled = consumer.message_capture.enabled;
            let capture = MessageCapture::with_capacity(&topic, consumer.message_capture.keep_last);
            let sequence_counter = self.metrics.counter_for(&consumer_key).await;

            let resolved_output =
                resolve_consumer_output(consumer, publishers).map_err(anyhow::Error::msg)?;
            let output_for_route = match &resolved_output {
                ResolvedConsumerOutput::None => Endpoint::null(),
                ResolvedConsumerOutput::Publisher { endpoint, .. } => (*endpoint).as_ref().clone(),
                // A `Static` output endpoint emits the configured payload as the
                // route response, attaching `metadata` as response headers. This
                // replaces the previous hand-built response message.
                ResolvedConsumerOutput::Response { response } => match response {
                    Some(r) => Endpoint::new(EndpointType::Static(StaticConfig {
                        body: r.payload.clone(),
                        raw: true,
                        metadata: r.headers.clone(),
                    })),
                    None => Endpoint::null(),
                },
            };

            let context = Arc::new(CollectorContext {
                source_key: consumer_key.clone(),
                capture,
                counter: sequence_counter,
                output: resolved_output,
                capture_enabled,
            });

            let route = Route::new(consumer.endpoint.clone(), output_for_route)
                .with_options(consumer.options.clone())
                .with_handler(move |msg: CanonicalMessage| {
                    let ctx = Arc::clone(&context);
                    async move {
                        // A `retry` redelivery re-enters the handler, so counting
                        // it would inflate the consumer's total by `max_attempts`.
                        if !is_redelivery(&msg) {
                            ctx.counter.fetch_add(1, Ordering::Relaxed);
                        }

                        if ctx.capture_enabled {
                            let mut enriched = msg.clone();

                            // The configured response, if any, is recorded alongside the
                            // request so the UI can show both halves of a request/reply.
                            if let ResolvedConsumerOutput::Response { response: Some(r) } =
                                &ctx.output
                            {
                                let meta = &mut enriched.metadata;
                                meta.insert("ui_response_payload".into(), r.payload.clone());
                                if let Ok(headers) = serde_json::to_string(&r.headers) {
                                    meta.insert("ui_response_metadata".into(), headers);
                                }
                            }

                            // Tags the source/capture time and never blocks the bridge:
                            // a full buffer drops its oldest entry to keep "last N".
                            ctx.capture.push(&ctx.source_key, enriched);
                        }

                        match &ctx.output {
                            ResolvedConsumerOutput::None => Ok(Handled::Ack),
                            ResolvedConsumerOutput::Publisher { endpoint, .. } => {
                                if matches!(endpoint.endpoint_type, EndpointType::Null) {
                                    Ok(Handled::Ack)
                                } else {
                                    Ok(Handled::Publish(msg))
                                }
                            }
                            ResolvedConsumerOutput::Response { response } => {
                                // The `Static` output endpoint produces the response
                                // (payload + headers); we just trigger it.
                                if response.is_some() {
                                    Ok(Handled::Publish(msg))
                                } else {
                                    Ok(Handled::Ack)
                                }
                            }
                        }
                    }
                });
            let internal_route_name = collector_route_name(&consumer_key);
            let handle = route.run(&internal_route_name).await?;
            handles.insert(consumer_key, handle);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum UpdateConfigError {
    Validation(String),
    UnsupportedCustomResponses(String),
    RegisterOutputEndpoint(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for UpdateConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::UnsupportedCustomResponses(message)
            | Self::RegisterOutputEndpoint(message) => write!(f, "{message}"),
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for UpdateConfigError {}
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct StorageSecurityInfoResponse {
    pub target: String,
    pub encrypted: bool,
    pub persistent: bool,
    pub key_source: String,
    pub key_store_available: bool,
    pub encrypted_config_available: bool,
    pub persistent_messages_available: bool,
    pub config_encrypted: bool,
    pub messages_encrypted: bool,
    pub messages_persistent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_key_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct FeatureAvailabilityResponse {
    pub ibm_mq: bool,
    pub kafka: bool,
    pub nats: bool,
    pub amqp: bool,
    pub mqtt: bool,
    pub http: bool,
    pub grpc: bool,
    pub zeromq: bool,
    pub mongodb: bool,
    pub aws: bool,
    pub sled: bool,
    pub redis_streams: bool,
    pub object_store: bool,
}

impl FeatureAvailabilityResponse {
    pub fn detect() -> Self {
        Self {
            ibm_mq: cfg!(feature = "ibm-mq"),
            kafka: cfg!(feature = "kafka") || cfg!(feature = "full"),
            nats: cfg!(feature = "nats") || cfg!(feature = "full"),
            amqp: cfg!(feature = "amqp") || cfg!(feature = "full"),
            mqtt: cfg!(feature = "mqtt") || cfg!(feature = "full"),
            http: cfg!(feature = "http") || cfg!(feature = "full"),
            grpc: cfg!(feature = "grpc") || cfg!(feature = "full"),
            zeromq: cfg!(feature = "zeromq") || cfg!(feature = "full"),
            mongodb: cfg!(feature = "mongodb") || cfg!(feature = "full"),
            aws: cfg!(feature = "aws") || cfg!(feature = "full"),
            sled: cfg!(feature = "sled") || cfg!(feature = "full"),
            redis_streams: cfg!(feature = "redis-streams") || cfg!(feature = "full"),
            object_store: cfg!(feature = "object-store") || cfg!(feature = "full"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigSecurity;
    use crate::encrypted_config::{
        clear_process_config_master_key, set_process_config_master_key_hex,
        test_config_master_key_lock,
    };
    use mq_bridge::models::MemoryConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct NoopSecretStore;

    impl SecretStore for NoopSecretStore {
        fn store(&self, _secrets: &HashMap<String, String>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn sample_storage_security(config: &AppConfig) -> StorageSecurityInfoResponse {
        match config.security_mode() {
            ConfigSecurityMode::Sensitive => StorageSecurityInfoResponse {
                target: "cli".to_string(),
                encrypted: true,
                persistent: false,
                key_source: "ephemeral-process".to_string(),
                key_store_available: false,
                encrypted_config_available: true,
                persistent_messages_available: false,
                config_encrypted: true,
                messages_encrypted: true,
                messages_persistent: false,
                reason: None,
                message_key_hex: Some("abc".to_string()),
                kid: Some("kid".to_string()),
            },
            _ => StorageSecurityInfoResponse {
                target: "cli".to_string(),
                encrypted: false,
                persistent: true,
                key_source: "none".to_string(),
                key_store_available: false,
                encrypted_config_available: false,
                persistent_messages_available: false,
                config_encrypted: false,
                messages_encrypted: false,
                messages_persistent: true,
                reason: None,
                message_key_hex: None,
                kid: None,
            },
        }
    }

    #[tokio::test]
    async fn update_config_runs_storage_hooks_and_refreshes_security() {
        let _guard = test_config_master_key_lock().lock().await;
        let initial_config = AppConfig {
            config_security: Some(ConfigSecurity {
                mode: ConfigSecurityMode::Balanced,
            }),
            ..Default::default()
        };

        let temp_path =
            std::env::temp_dir().join(format!("mqb-ui-app-test-{}.yaml", Uuid::new_v4()));
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = Arc::new(AtomicUsize::new(0));

        let prepare_counter = Arc::clone(&prepare_calls);
        let resolver_counter = Arc::clone(&resolver_calls);
        let app = UiApp::new_with_secret_store_and_storage_hooks(
            initial_config.clone(),
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
            temp_path.to_string_lossy().to_string(),
            Arc::new(NoopSecretStore),
            sample_storage_security(&initial_config),
            Arc::new(move |config| {
                resolver_counter.fetch_add(1, Ordering::SeqCst);
                sample_storage_security(config)
            }),
            Arc::new(move |config| {
                prepare_counter.fetch_add(1, Ordering::SeqCst);
                if matches!(
                    config.security_mode(),
                    ConfigSecurityMode::Sensitive | ConfigSecurityMode::Durable
                ) {
                    set_process_config_master_key_hex(
                        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
                            .to_string(),
                    );
                }
                Ok(())
            }),
        )
        .unwrap();

        let mut next_config = initial_config.clone();
        next_config.config_security = Some(ConfigSecurity {
            mode: ConfigSecurityMode::Sensitive,
        });

        app.update_config(next_config).await.unwrap();

        assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert!(app.storage_security().config_encrypted);

        let _ = std::fs::remove_file(temp_path);
        clear_process_config_master_key();
    }

    #[tokio::test]
    async fn reset_config_recovery_clears_pending_status() {
        let initial_config = AppConfig {
            config_security: Some(ConfigSecurity {
                mode: ConfigSecurityMode::Balanced,
            }),
            ..Default::default()
        };

        let app = UiApp::new_with_secret_store_and_runtime_hooks(
            initial_config.clone(),
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
            "/tmp/mqb-config.yml".to_string(),
            Arc::new(NoopSecretStore),
            sample_storage_security(&initial_config),
            UiAppRuntimeHooks::default()
                .with_storage_security_resolver(Arc::new(sample_storage_security))
                .with_storage_save_prepare(Arc::new(|_| Ok(())))
                .with_config_recovery(Some(ConfigRecoveryStatusResponse {
                    mode: Some("sensitive".to_string()),
                    reason: "decrypt-failed".to_string(),
                    message: "The encrypted config could not be decrypted.".to_string(),
                    detail: Some("Failed to decrypt sensitive config".to_string()),
                }))
                .with_config_recovery_reset(Some(Arc::new(|_| {
                    Ok(ConfigRecoveryResetResponse {
                        backup_path: "/tmp/mqb-config.yml.recovery.bak".to_string(),
                    })
                }))),
        )
        .unwrap();

        let handled = app.handle_reset_config_recovery().await.unwrap();
        let Handled::Publish(message) = handled else {
            panic!("expected publish response");
        };
        let payload = message.get_payload_str();

        assert!(payload.contains("backup_path"));
        assert!(app.config_recovery().is_none());
    }

    #[test]
    fn storage_security_for_cli_uses_explicit_temporary_messages_mode() {
        let config = AppConfig {
            config_security: Some(ConfigSecurity {
                mode: ConfigSecurityMode::EnvTemporaryMessages,
            }),
            ..Default::default()
        };

        let info = storage_security_for_cli(&config);

        assert_eq!(info.target, "cli");
        assert!(info.encrypted);
        assert!(!info.persistent);
        assert_eq!(info.key_source, "ephemeral-process");
        assert!(!info.config_encrypted);
        assert!(info.messages_encrypted);
        assert!(!info.messages_persistent);
        assert!(info.message_key_hex.is_some());
        assert!(info.kid.is_some());
    }

    #[test]
    fn storage_security_for_cli_reuses_ephemeral_message_key_within_process() {
        let config = AppConfig {
            config_security: Some(ConfigSecurity {
                mode: ConfigSecurityMode::TemporaryMessages,
            }),
            ..Default::default()
        };

        let first = storage_security_for_cli(&config);
        let second = storage_security_for_cli(&config);

        assert_eq!(first.message_key_hex, second.message_key_hex);
        assert_eq!(first.kid, second.kid);
    }

    #[test]
    fn storage_security_for_cli_uses_config_master_key_for_sensitive_modes() {
        let _guard = test_config_master_key_lock().blocking_lock();
        clear_process_config_master_key();

        let config = AppConfig {
            config_security: Some(ConfigSecurity {
                mode: ConfigSecurityMode::Sensitive,
            }),
            ..Default::default()
        };
        assert!(!storage_security_for_cli(&config).config_encrypted);

        set_process_config_master_key_hex(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_string(),
        );
        assert!(storage_security_for_cli(&config).config_encrypted);

        clear_process_config_master_key();
    }

    fn memory_consumer(id: &str, name: &str, topic: &str) -> crate::config::ConsumerConfig {
        crate::config::ConsumerConfig {
            id: id.to_string(),
            name: name.to_string(),
            enabled: true,
            endpoint: Endpoint::new(EndpointType::Memory(MemoryConfig::new(topic, Some(8)))),
            comment: String::new(),
            response: None,
            output: crate::config::ConsumerOutputConfig::None,
            message_capture: crate::config::ConsumerMessageCaptureConfig::default(),
            options: Default::default(),
        }
    }

    fn test_app(config: AppConfig) -> UiApp {
        UiApp::new(
            config,
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
            std::env::temp_dir()
                .join(format!("mqb-status-{}.yml", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn peer_status_requires_server_asserted_loopback() {
        let app = test_app(AppConfig::default());
        let request = CanonicalMessage::new(Vec::new(), None)
            .with_metadata_kv("http_method", "GET")
            .with_metadata_kv("http_path", "/peer-status");
        let response = app.handle_ui_message(request, false).await.unwrap();
        let Handled::Publish(response) = response else {
            panic!("peer status should return an HTTP response")
        };
        assert_eq!(
            response
                .metadata
                .get("http_status_code")
                .map(String::as_str),
            Some("403")
        );

        let request = CanonicalMessage::new(Vec::new(), None)
            .with_metadata_kv("http_method", "GET")
            .with_metadata_kv("http_path", "/peer-status")
            .with_metadata_kv("mqb_peer_loopback", "true");
        let response = app.handle_ui_message(request, false).await.unwrap();
        let Handled::Publish(response) = response else {
            panic!("peer status should return an HTTP response")
        };
        assert_eq!(
            response
                .metadata
                .get("http_status_code")
                .map(String::as_str),
            Some("200")
        );
        // Reading peers must not publish: without a running heartbeat there is
        // no lease for this process, and the read path never creates one.
        let peers: PeerStatusResponse = serde_json::from_slice(&response.payload).unwrap();
        assert!(peers.current_instance_id.is_empty());
        assert!(
            !peers
                .instances
                .iter()
                .any(|peer| peer.instance_id == "not-running-instance")
        );
    }

    // Once the heartbeat is running, this process is one of the instances the
    // registry reports — that is what makes it visible to other UIs.
    #[tokio::test]
    async fn a_running_publisher_lists_its_own_instance() {
        let app = test_app(AppConfig::default());
        app.spawn_status_registry_publisher();
        let peers = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let peers = app.peer_status().await;
                if !peers.current_instance_id.is_empty()
                    && peers
                        .instances
                        .iter()
                        .any(|peer| peer.instance_id == peers.current_instance_id)
                {
                    break peers;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("status registry publication should complete within 2 seconds");
        app.remove_status_registry_lease();

        assert!(!peers.current_instance_id.is_empty());
        assert!(
            peers
                .instances
                .iter()
                .any(|peer| peer.instance_id == peers.current_instance_id)
        );

        let peers = app.peer_status().await;
        assert!(
            !peers
                .instances
                .iter()
                .any(|peer| peer.pid == std::process::id())
        );
    }

    // Reading `/messages` before the collector route exists must not create the
    // capture buffer at some default size, or the writer that follows silently
    // inherits it and `keep_last` stops meaning anything.
    #[tokio::test]
    async fn reading_messages_before_the_collector_starts_keeps_the_configured_capacity() {
        let consumer_id = format!("cap-{}", uuid::Uuid::new_v4());
        let mut consumer = memory_consumer(&consumer_id, "capacity probe", "capacity_probe_topic");
        consumer.message_capture = crate::config::ConsumerMessageCaptureConfig {
            enabled: true,
            keep_last: 2,
        };
        let config = AppConfig {
            consumers: vec![consumer],
            ..Default::default()
        };
        let app = test_app(config);

        // Reader first: no collector route has run yet.
        assert!(app.get_messages(Some(&consumer_id)).await.is_empty());

        // Writer second, as `start_ui_collector_routes` would open it.
        let topic = format!("ui_collector_{consumer_id}");
        let capture = MessageCapture::with_capacity(&topic, 2);
        capture.push("src", CanonicalMessage::from("first"));
        capture.push("src", CanonicalMessage::from("second"));
        capture.push("src", CanonicalMessage::from("third"));

        let payloads: Vec<String> = capture
            .drain()
            .iter()
            .map(|m| m.get_payload_str().to_string())
            .collect();
        assert_eq!(payloads, vec!["second", "third"]);
    }

    // A running consumer's status must reflect the live route health read from the
    // stored handle (`RouteHandle::status()`), not a hardcoded `healthy: true`.
    // The unhealthy/reconnecting transition itself is covered by mq-bridge's own
    // route-status test; the app side is a direct passthrough of `handle.status()`.
    #[tokio::test]
    async fn running_consumer_status_reflects_live_route_health() {
        let mut config = AppConfig::default();
        config.consumers.push(memory_consumer(
            "mem-status-test",
            "MemConsumer",
            "mem_status_test_topic",
        ));
        let app = test_app(config);

        assert!(app.start_consumer("mem-status-test").await.unwrap());

        let mut snapshot = None;
        for _ in 0..100 {
            let rs = app.runtime_status().await;
            if let Some(s) = rs.consumers.get("mem-status-test")
                && s.running
                && s.status.healthy
            {
                snapshot = Some(s.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let snapshot =
            snapshot.expect("consumer should become running and healthy via live status");
        assert!(snapshot.running);
        assert!(snapshot.status.healthy);
        // Live status target is the internal collector route id; the snapshot must keep
        // the consumer's display name so it does not leak to the UI.
        assert_eq!(snapshot.status.target, "MemConsumer");
        assert!(snapshot.status.error.is_none());

        app.stop_consumer("mem-status-test").await;
    }

    // The UI's start button is a plain POST, so a double click (or two browser
    // tabs) calls `start_consumer` twice. The second start used to insert its
    // handle over the first, and `RouteHandle` has no `Drop` — so the displaced
    // route kept running, consuming the same source, invisible to the UI and
    // impossible to stop, while every message it handled was counted again.
    //
    // What a config file describes has to actually run at boot, and a consumer
    // the operator turned off has to stay off.
    #[tokio::test]
    async fn boot_starts_the_enabled_consumers_only() {
        let mut config = AppConfig::default();
        config
            .consumers
            .push(memory_consumer("on", "On", "boot-enabled"));
        let mut paused = memory_consumer("off", "Off", "boot-disabled");
        paused.enabled = false;
        config.consumers.push(paused);

        let app = test_app(config);
        assert_eq!(app.start_configured_consumers().await, 1);

        let handles = app.ui_handles.read().await;
        assert!(handles.contains_key("on"));
        assert!(
            !handles.contains_key("off"),
            "a disabled consumer must not be started at boot"
        );
        drop(handles);
        app.stop_consumer("on").await;
    }

    // A file source makes the duplicate unambiguous: each route reads the whole
    // file, so a second one doubles the message count instead of merely sharing
    // a queue with the first.
    #[tokio::test]
    async fn starting_a_running_consumer_twice_leaves_no_untracked_route() {
        let source = std::env::temp_dir().join(format!("mqb-restart-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&source, "a\nb\nc\n").expect("write the source file");

        let mut consumer = memory_consumer("double-start", "DoubleStart", "unused");
        consumer.endpoint = Endpoint::new(EndpointType::File(mq_bridge::models::FileConfig {
            path: source.to_string_lossy().to_string(),
            mode: None,
            ..Default::default()
        }));
        let mut config = AppConfig::default();
        config.consumers.push(consumer);
        let app = test_app(config);

        assert!(app.start_consumer("double-start").await.unwrap());
        for _ in 0..100 {
            if app.metrics.sequence("double-start").await >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The second start must be a no-op while the first route is alive.
        assert!(app.start_consumer("double-start").await.unwrap());
        assert_eq!(
            app.ui_handles.read().await.len(),
            1,
            "one consumer must never track more than one route"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            app.metrics.sequence("double-start").await,
            3,
            "the file must be read once; a higher count means a second, untracked \
             route is consuming the same source"
        );

        app.stop_consumer("double-start").await;
        let _ = std::fs::remove_file(&source);
    }

    // A consumer configured to drain and exit finishes on its own, with nothing
    // removing its handle. Membership of `ui_handles` therefore reported it as
    // running forever, with a green badge taken from its last connection health.
    // The handle's own outcome is what says it is done, and how it ended.
    #[tokio::test]
    async fn a_drained_consumer_reports_not_running_with_a_completed_outcome() {
        // A file source, as in the reported case: it has a real end, so the route
        // drains and exits on its own with nobody calling `stop`.
        let source = std::env::temp_dir().join(format!("mqb-drain-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&source, "first\nsecond\n").expect("write drain source");

        let mut consumer = memory_consumer("file-drain-test", "FileDrain", "file_drain_test_topic");
        consumer.endpoint = Endpoint::new(EndpointType::File(mq_bridge::models::FileConfig {
            path: source.to_string_lossy().to_string(),
            mode: None,
            ..Default::default()
        }));
        consumer.options.exit_on_empty = true;
        let mut config = AppConfig::default();
        config.consumers.push(consumer);
        let app = test_app(config);

        assert!(app.start_consumer("file-drain-test").await.unwrap());

        // Wait for the drain, then for the task to publish its outcome.
        let mut response = None;
        for _ in 0..100 {
            let status = app
                .consumer_status("file-drain-test")
                .await
                .expect("configured consumer has a status");
            if !status.running {
                response = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let response = response.expect("drained consumer should stop reporting as running");
        assert_eq!(response.outcome, Some(RouteOutcomeSnapshot::Completed));
        // Reported from the retained handle, not from a fresh probe of a source
        // the route already drained.
        assert_eq!(response.status.target, "FileDrain");

        // `/runtime-status` reads the same liveness, so the consumer must also
        // drop out of `active_consumers`.
        let runtime = app.runtime_status().await;
        assert!(
            !runtime
                .active_consumers
                .contains(&"file-drain-test".to_string())
        );
        let snapshot = runtime
            .consumers
            .get("file-drain-test")
            .expect("drained consumer is still configured");
        assert!(!snapshot.running);
        assert_eq!(snapshot.outcome, Some(RouteOutcomeSnapshot::Completed));
    }
}
