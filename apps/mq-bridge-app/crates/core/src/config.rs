use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::encrypted_config::{
    config_file_format_from_path, encode_sensitive_config_file, maybe_decrypt_config_source,
};
use anyhow::Result;
use config::Config;
use mq_bridge::{
    Route,
    models::{Endpoint, EndpointType, Middleware, RouteOptions, SecretExtractor},
};
use schemars::JsonSchema;
use uuid::Uuid;

fn default_log_level() -> String {
    "info".to_string()
}

fn default_route_enabled() -> bool {
    true
}

fn default_consumer_capture_enabled() -> bool {
    true
}

fn default_consumer_capture_keep_last() -> usize {
    100
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_route_migrated_capture() -> ConsumerMessageCaptureConfig {
    ConsumerMessageCaptureConfig {
        enabled: false,
        keep_last: default_consumer_capture_keep_last(),
    }
}

fn generate_config_id() -> String {
    Uuid::now_v7().to_string()
}

#[derive(
    Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSecurityMode {
    Unencrypted,
    #[default]
    Balanced,
    EnvTemporaryMessages,
    TemporaryMessages,
    Sensitive,
    Durable,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
pub struct ConfigSecurity {
    /// Configuration security.
    /// balanced: Extract secrets to the target secret store and keep message history plain.
    /// env_temporary_messages: Extract secrets to env or placeholders and encrypt message history temporarily.
    /// temporary_messages: Keep config plain and encrypt message history temporarily.
    /// sensitive: Encrypt config and encrypt message history temporarily.
    /// durable: Encrypt config and keep encrypted message history between restarts when supported.
    #[serde(default)]
    pub mode: ConfigSecurityMode,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
pub struct AppConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub logger: String,
    /// Optional url of the ui endpoint. For example "0.0.0.0:9090".
    #[serde(default)]
    pub ui_addr: String,
    /// Optional url of a standalone metrics endpoint. For example "0.0.0.0:9091".
    /// If set, a standalone metrics server will be started on this address.
    /// If it matches `ui_addr`, the standalone server is skipped as the UI handles it.
    #[serde(default)]
    pub metrics_addr: String,
    /// Native plugin libraries to load before starting anything. Each provides
    /// an endpoint (and possibly a middleware) usable by name in routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, RouteConfig>,
    #[serde(default)]
    pub consumers: Vec<ConsumerConfig>,
    #[serde(default)]
    pub publishers: Vec<PublisherClient>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub history: HashMap<String, serde_json::Value>,
    #[serde(default, alias = "envVars")]
    pub env_vars: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_security: Option<ConfigSecurity>,
    /// Legacy compatibility flag. Prefer config_security.mode instead.
    #[serde(default, skip_serializing_if = "is_false")]
    pub extract_secrets: bool,
    /// The default tab to show in the UI upon loading.
    #[serde(default)]
    pub default_tab: String,
}

/// Generates the application schema with runtime defaults sourced from the
/// actual mq-bridge model rather than schemars' fallback values.
pub fn app_config_schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(AppConfig))
        .expect("AppConfig schema should serialize");
    let default = serde_json::Value::from(RouteOptions::default().batch_size);
    for definition in ["RouteConfig", "ConsumerConfig"] {
        schema
            .pointer_mut(&format!("/$defs/{definition}/properties/batch_size"))
            .and_then(serde_json::Value::as_object_mut)
            .unwrap_or_else(|| panic!("AppConfig schema should contain {definition}.batch_size"))
            .insert("default".to_string(), default.clone());
    }

    schema["$defs"]["PulsarConfig"] = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "url": { "type": "string" },
            "topic": { "type": "string" },
            "subscription": { "type": "string" }
        },
        "required": ["url"]
    });
    schema["$defs"]["Endpoint"]["oneOf"]
        .as_array_mut()
        .expect("AppConfig schema should contain Endpoint variants")
        .push(serde_json::json!({
            "type": "object",
            "properties": {
                "pulsar": { "$ref": "#/$defs/PulsarConfig" }
            },
            "required": ["pulsar"]
        }));
    schema
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
pub struct HeaderRow {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default = "default_route_enabled")]
    pub enabled: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema, Clone, Default)]
pub struct SecretReferenceSummary {
    #[serde(default)]
    pub routes: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub consumers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub publishers: HashMap<String, Vec<String>>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone)]
pub struct RouteConfig {
    #[serde(default = "default_route_enabled")]
    pub enabled: bool,
    #[serde(flatten)]
    pub route: Route,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone)]
pub struct ConsumerConfig {
    #[serde(default = "generate_config_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Whether this consumer starts at boot; carries a migrated route's `enabled`
    /// flag. A disabled one still starts on demand from the UI.
    #[serde(default = "default_route_enabled", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub endpoint: Endpoint,
    #[serde(default)]
    pub comment: String,
    // TODO: remove, as already implemented in output
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ConsumerResponseConfig>,
    #[serde(default, skip_serializing_if = "consumer_output_is_none")]
    pub output: ConsumerOutputConfig,
    #[serde(default)]
    pub message_capture: ConsumerMessageCaptureConfig,
    #[serde(flatten, default)]
    pub options: mq_bridge::models::RouteOptions,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
pub struct ConsumerResponseConfig {
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub payload: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ConsumerOutputConfig {
    #[default]
    None,
    Publisher {
        publisher: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        publisher_id: Option<String>,
    },
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<ConsumerResponseConfig>,
    },
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone)]
pub struct ConsumerMessageCaptureConfig {
    #[serde(default = "default_consumer_capture_enabled")]
    pub enabled: bool,
    #[serde(default = "default_consumer_capture_keep_last", alias = "keepLast")]
    pub keep_last: usize,
}

impl Default for ConsumerMessageCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: default_consumer_capture_enabled(),
            keep_last: default_consumer_capture_keep_last(),
        }
    }
}

fn consumer_output_is_none(output: &ConsumerOutputConfig) -> bool {
    matches!(output, ConsumerOutputConfig::None)
}

#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Clone)]
pub struct PublisherClient {
    #[serde(default = "generate_config_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub endpoint: Endpoint,
    #[serde(default)]
    pub comment: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub payload: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRow>,
    #[serde(default, alias = "sortOrder", skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<i32>,
}

fn next_unique_name(base: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(base) {
        return base.to_string();
    }

    let mut index = 1;
    loop {
        let candidate = format!("{base}_{index}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn endpoint_value(endpoint: &Endpoint) -> serde_json::Value {
    serde_json::to_value(endpoint).unwrap_or(serde_json::Value::Null)
}

pub trait SecretStore: Send + Sync {
    fn store(&self, secrets: &HashMap<String, String>) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct EnvFileSecretStore {
    path: PathBuf,
}

impl EnvFileSecretStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SecretStore for EnvFileSecretStore {
    fn store(&self, secrets: &HashMap<String, String>) -> Result<()> {
        if secrets.is_empty() {
            return Ok(());
        }

        let existing_content = if self.path.exists() {
            std::fs::read_to_string(&self.path)?
        } else {
            String::new()
        };

        let mut new_lines = Vec::new();
        let mut processed_keys = HashSet::new();

        for line in existing_content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                new_lines.push(line.to_string());
                continue;
            }

            if let Some((key, _)) = line.split_once('=') {
                let key = key.trim();
                if let Some(new_value) = secrets.get(key) {
                    new_lines.push(format!("{}={}", key, new_value));
                    processed_keys.insert(key.to_string());
                } else {
                    new_lines.push(line.to_string());
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        for (key, value) in secrets {
            if !processed_keys.contains(key) {
                new_lines.push(format!("{}={}", key, value));
            }
        }

        let mut final_content = new_lines.join("\n");
        if !final_content.ends_with('\n') {
            final_content.push('\n');
        }
        std::fs::write(&self.path, final_content)?;
        Ok(())
    }
}

fn extract_inline_env_vars(content: &str, format: config::FileFormat) -> HashMap<String, String> {
    let parsed = match format {
        config::FileFormat::Json => serde_json::from_str::<serde_json::Value>(content).ok(),
        _ => serde_yaml_ng::from_str::<serde_json::Value>(content).ok(),
    };

    let Some(root) = parsed else {
        return HashMap::new();
    };

    root.get("env_vars")
        .or_else(|| root.get("envVars"))
        .and_then(serde_json::Value::as_object)
        .map(|env_vars| {
            env_vars
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn expand_variables(content: &str, format: config::FileFormat) -> Result<String, anyhow::Error> {
    let inline_env_vars = extract_inline_env_vars(content, format);
    let expanded = shellexpand::env_with_context_no_errors(content, |key| {
        std::env::var(key)
            .ok()
            .or_else(|| inline_env_vars.get(key).cloned())
    });
    Ok(expanded.to_string())
}

// New helper function to create a config source from a string.
// It expands environment variables and assumes a given format.
fn source_from_str(
    content: &str,
    format: config::FileFormat,
) -> Result<config::File<config::FileSourceString, config::FileFormat>, anyhow::Error> {
    let expanded = expand_variables(content, format)?;
    // Using `required(false)` means an empty or whitespace-only string won't cause an error.
    Ok(config::File::from_str(&expanded, format).required(false))
}

/// Top-level keys that belong to the application rather than to a route.
/// Everything else in a single-route config is the route.
fn app_level_fields() -> &'static HashSet<String> {
    static FIELDS: OnceLock<HashSet<String>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        let mut fields: HashSet<String> = serde_json::to_value(AppConfig::default())
            .expect("AppConfig default should serialize")
            .as_object()
            .expect("AppConfig should serialize as an object")
            .keys()
            .cloned()
            .collect();
        // Fields that `AppConfig::default()` skips when serializing, so they are
        // absent from the set above and would otherwise be lifted into the route.
        for field in [
            "envVars",
            "routes",
            "plugins",
            "history",
            "config_security",
            "extract_secrets",
        ] {
            fields.insert(field.to_string());
        }
        fields
    })
}

/// The name a bare single-route config runs under.
pub const SINGLE_ROUTE_NAME: &str = "route";

/// Lifts routes written at the top level into the `routes:` map, so a config
/// that is just a route needs neither the wrapper nor a name.
///
/// A bare `input:` becomes one route named [`SINGLE_ROUTE_NAME`] that takes every
/// key which is not an application setting — so route options belong to it, and a
/// misspelled one is rejected by name rather than ignored. Otherwise each
/// top-level map holding an `input` is a route under its own key, the shape the
/// engine's configuration guide uses. `None` when there is nothing to lift.
fn lift_bare_routes(mut raw: serde_json::Value) -> Option<serde_json::Value> {
    let map = raw.as_object_mut()?;
    if map.contains_key("routes") {
        return None;
    }

    let is_app_field = |key: &str| app_level_fields().contains(key);

    let routes = if map.contains_key("input") {
        let keys = map
            .keys()
            .filter(|key| !is_app_field(key))
            .cloned()
            .collect();
        serde_json::json!({ SINGLE_ROUTE_NAME: take_keys(map, keys) })
    } else {
        // A named route is recognised by what it holds, not by its key, so an
        // unrelated top-level key stays where it is.
        let keys: Vec<String> = map
            .iter()
            .filter(|(key, value)| {
                !is_app_field(key)
                    && value
                        .as_object()
                        .is_some_and(|route| route.contains_key("input"))
            })
            .map(|(key, _)| key.clone())
            .collect();
        if keys.is_empty() {
            return None;
        }
        serde_json::Value::Object(take_keys(map, keys))
    };

    map.insert("routes".to_string(), routes);
    Some(raw)
}

fn take_keys(
    map: &mut serde_json::Map<String, serde_json::Value>,
    keys: Vec<String>,
) -> serde_json::Map<String, serde_json::Value> {
    keys.into_iter()
        .filter_map(|key| map.remove(&key).map(|value| (key, value)))
        .collect()
}

fn load_config_internal(
    config_path: Option<String>,
    init_config_path: Option<String>,
    init_config_str: Option<String>,
    config_str: Option<String>,
    load_dotenv: bool,
    use_env_overrides: bool,
) -> Result<(AppConfig, String), anyhow::Error> {
    if load_dotenv {
        // Diagnostics go to stderr: `mqb mcp --transport stdio` uses
        // stdout as the MCP protocol channel, so anything printed there corrupts
        // the stream.
        match dotenvy::dotenv() {
            Ok(path) => eprintln!("INFO: Loaded .env file from {:?}", path),
            Err(e) => eprintln!("DEBUG: No .env file loaded: {}", e),
        }
    }

    let persistent_file = if use_env_overrides {
        config_file_path(config_path)
    } else {
        config_path.unwrap_or_else(|| "config.yml".to_string())
    };

    let init_config_path = if use_env_overrides {
        init_config_path.or_else(|| std::env::var("INIT_CONFIG_FILE").ok())
    } else {
        init_config_path
    };
    let init_config_str = if use_env_overrides {
        init_config_str.or_else(|| std::env::var("INIT_CONFIG_STRING").ok())
    } else {
        init_config_str
    };
    let config_str = if use_env_overrides {
        config_str.or_else(|| std::env::var("CONFIG_STRING").ok())
    } else {
        config_str
    };

    let mut builder = Config::builder().set_default("log_level", "info")?;

    // --- Configuration Loading Hierarchy ---
    // The `config` crate merges sources, with later sources overriding earlier ones.
    // 1. Initialization sources (if main config file doesn't exist)
    // 2. Main config file
    // 3. Override config string
    // 4. Environment variables

    let persistent_file_exists = Path::new(&persistent_file).exists();

    if !persistent_file_exists {
        // Try to initialize. Precedence: init_config_path > init_config_str
        if let Some(template_path) = &init_config_path {
            if Path::new(template_path).exists() {
                eprintln!(
                    "INFO: Main config '{}' not found. Initializing from template file '{}'.",
                    persistent_file, template_path
                );
                let content = std::fs::read_to_string(template_path)?;
                let format = config_file_format_from_path(template_path);
                builder = builder.add_source(source_from_str(&content, format)?);
            } else {
                eprintln!(
                    "WARN: Template file '{}' not found. It will be ignored.",
                    template_path
                );
            }
        } else if let Some(init_str) = &init_config_str {
            eprintln!(
                "INFO: Main config '{}' not found. Initializing from string (assuming YAML format).",
                persistent_file
            );
            builder = builder.add_source(source_from_str(init_str, config::FileFormat::Yaml)?);
        } else {
            eprintln!(
                "INFO: Main config '{}' not found. Starting with default settings.",
                persistent_file
            );
        }
    } else {
        // Main config file exists, load it.
        eprintln!("INFO: Loading configuration from '{}'.", persistent_file);
        let content = std::fs::read_to_string(&persistent_file)?;
        let format = config_file_format_from_path(&persistent_file);
        let effective_content = maybe_decrypt_config_source(&content, format)?.unwrap_or(content);
        builder = builder.add_source(source_from_str(&effective_content, format)?);

        if init_config_path.is_some() || init_config_str.is_some() {
            eprintln!(
                "INFO: Main config '{}' found. Ignoring initialization options.",
                persistent_file
            );
        }
    }

    // Add override string if present. This will override file/init sources.
    if let Some(override_str) = &config_str {
        eprintln!("INFO: Applying configuration override from string (assuming YAML format).");
        builder = builder.add_source(source_from_str(override_str, config::FileFormat::Yaml)?);
    }

    let builder = if use_env_overrides {
        builder.add_source(
            config::Environment::default()
                .prefix("MQB")
                .separator("__")
                .ignore_empty(true)
                .try_parsing(true),
        )
    } else {
        builder
    };

    let settings = builder.build()?;

    let raw = settings.clone().try_deserialize::<serde_json::Value>()?;

    let mut config: AppConfig = match lift_bare_routes(raw) {
        Some(lifted) => Config::builder()
            .add_source(config::File::from_str(
                &serde_json::to_string(&lifted)?,
                config::FileFormat::Json,
            ))
            .build()?
            .try_deserialize()?,
        None => settings.try_deserialize()?,
    };
    config.migrate_legacy_routes();
    Ok((config, persistent_file))
}

pub fn load_config(
    config_path: Option<String>,
    init_config_path: Option<String>,
    init_config_str: Option<String>,
    config_str: Option<String>,
) -> Result<(AppConfig, String), anyhow::Error> {
    load_config_internal(
        config_path,
        init_config_path,
        init_config_str,
        config_str,
        true,
        true,
    )
}

pub fn config_file_path(config_path: Option<String>) -> String {
    config_path.unwrap_or_else(|| {
        std::env::var("CONFIG_FILE").unwrap_or_else(|_| "config.yml".to_string())
    })
}

pub fn load_config_at_path(
    config_path: impl Into<String>,
) -> Result<(AppConfig, String), anyhow::Error> {
    load_config_internal(Some(config_path.into()), None, None, None, false, false)
}

impl AppConfig {
    pub fn ensure_entity_ids(&mut self) {
        let mut known_ids = HashSet::new();
        for publisher in &mut self.publishers {
            if publisher.id.trim().is_empty() || !known_ids.insert(publisher.id.clone()) {
                publisher.id = generate_config_id();
                known_ids.insert(publisher.id.clone());
            }
        }

        known_ids.clear();
        for consumer in &mut self.consumers {
            if consumer.id.trim().is_empty() || !known_ids.insert(consumer.id.clone()) {
                consumer.id = generate_config_id();
                known_ids.insert(consumer.id.clone());
            }
        }
    }

    fn normalize_consumer_publisher_outputs(&mut self) {
        let publishers_by_id: HashMap<String, String> = self
            .publishers
            .iter()
            .map(|publisher| (publisher.id.clone(), publisher.name.clone()))
            .collect();
        let publisher_ids_by_name: HashMap<String, String> = self
            .publishers
            .iter()
            .map(|publisher| (publisher.name.clone(), publisher.id.clone()))
            .collect();

        for consumer in &mut self.consumers {
            if let ConsumerOutputConfig::Publisher {
                publisher,
                publisher_id,
            } = &mut consumer.output
            {
                let trimmed_name = publisher.trim().to_string();
                let resolved_by_id = publisher_id
                    .as_ref()
                    .and_then(|id| publishers_by_id.get(id).cloned());

                if let Some(name) = resolved_by_id {
                    *publisher = name;
                    continue;
                }

                if let Some(id) = publisher_ids_by_name.get(&trimmed_name) {
                    *publisher = trimmed_name;
                    *publisher_id = Some(id.clone());
                } else {
                    *publisher = trimmed_name;
                    *publisher_id = None;
                }
            }
        }
    }

    fn uses_encrypted_config_mode(mode: ConfigSecurityMode) -> bool {
        matches!(
            mode,
            ConfigSecurityMode::Sensitive | ConfigSecurityMode::Durable
        )
    }

    fn security_mode_label(mode: ConfigSecurityMode) -> &'static str {
        match mode {
            ConfigSecurityMode::Unencrypted => "unencrypted",
            ConfigSecurityMode::Balanced => "balanced",
            ConfigSecurityMode::EnvTemporaryMessages => "env_temporary_messages",
            ConfigSecurityMode::TemporaryMessages => "temporary_messages",
            ConfigSecurityMode::Sensitive => "sensitive",
            ConfigSecurityMode::Durable => "durable",
        }
    }

    pub fn security_mode(&self) -> ConfigSecurityMode {
        self.config_security
            .as_ref()
            .map(|security| security.mode)
            .unwrap_or_else(|| {
                if self.extract_secrets {
                    ConfigSecurityMode::Balanced
                } else {
                    ConfigSecurityMode::Unencrypted
                }
            })
    }

    pub fn migrate_legacy_security_mode(&mut self) {
        let mode = self.security_mode();
        self.config_security = Some(ConfigSecurity { mode });
        // Keep the legacy flag read-compatible, but normalize runtime state onto
        // config_security.mode so the UI and save path have a single source of truth.
        self.extract_secrets = false;
    }

    pub fn migrate_legacy_consumer_response(&mut self) {
        for consumer in &mut self.consumers {
            if matches!(consumer.output, ConsumerOutputConfig::None)
                && let Some(response) = consumer.response.take()
            {
                consumer.output = ConsumerOutputConfig::Response {
                    response: Some(response),
                };
            }
        }
    }

    pub fn migrate_legacy_routes(&mut self) {
        self.migrate_legacy_security_mode();
        self.migrate_legacy_consumer_response();

        if self.default_tab.trim() == "routes" {
            self.default_tab = "consumers".to_string();
        }

        if !self.routes.is_empty() {
            let mut existing_publisher_names: HashSet<String> = self
                .publishers
                .iter()
                .map(|publisher| publisher.name.clone())
                .collect();
            let mut existing_consumer_names: HashSet<String> = self
                .consumers
                .iter()
                .map(|consumer| consumer.name.clone())
                .collect();
            let mut routes = std::mem::take(&mut self.routes);

            for (route_name, route_config) in routes.drain() {
                let normalized_route_name = route_name.trim().to_string();
                let output =
                    if matches!(route_config.route.output.endpoint_type, EndpointType::Null) {
                        ConsumerOutputConfig::None
                    } else if let Some(existing) = self.publishers.iter().find(|publisher| {
                        endpoint_value(&publisher.endpoint)
                            == endpoint_value(&route_config.route.output)
                    }) {
                        ConsumerOutputConfig::Publisher {
                            publisher: existing.name.clone(),
                            publisher_id: Some(existing.id.clone()),
                        }
                    } else {
                        let publisher_name = next_unique_name(
                            &format!("{normalized_route_name}_publisher"),
                            &existing_publisher_names,
                        );
                        existing_publisher_names.insert(publisher_name.clone());
                        let publisher = PublisherClient {
                            id: generate_config_id(),
                            name: publisher_name.clone(),
                            endpoint: route_config.route.output.clone(),
                            comment: String::new(),
                            payload: String::new(),
                            headers: Vec::new(),
                            sort_order: None,
                        };
                        let publisher_id = publisher.id.clone();
                        self.publishers.push(publisher);
                        ConsumerOutputConfig::Publisher {
                            publisher: publisher_name,
                            publisher_id: Some(publisher_id),
                        }
                    };

                let consumer_name =
                    next_unique_name(&normalized_route_name, &existing_consumer_names);
                existing_consumer_names.insert(consumer_name.clone());
                self.consumers.push(ConsumerConfig {
                    id: generate_config_id(),
                    name: consumer_name,
                    enabled: route_config.enabled,
                    endpoint: route_config.route.input,
                    comment: String::new(),
                    response: None,
                    output,
                    message_capture: default_route_migrated_capture(),
                    options: route_config.route.options,
                });
            }
        }

        self.ensure_entity_ids();
        self.normalize_consumer_publisher_outputs();
    }

    pub fn save(&self, path: &str) -> Result<()> {
        let env_store = EnvFileSecretStore::new(".env");
        self.save_with_secret_store(path, &env_store)
    }

    pub fn save_with_secret_store(&self, path: &str, secret_store: &dyn SecretStore) -> Result<()> {
        let mut config_to_save = self.clone();
        config_to_save.migrate_legacy_routes();

        let trimmed_routes: HashMap<String, RouteConfig> = config_to_save
            .routes
            .drain()
            .map(|(k, v)| (k.trim().to_string(), v))
            .collect();
        config_to_save.routes = trimmed_routes;

        for consumer in &mut config_to_save.consumers {
            consumer.name = consumer.name.trim().to_string();
        }

        for pub_client in &mut config_to_save.publishers {
            pub_client.name = pub_client.name.trim().to_string();
        }

        let mode = config_to_save.security_mode();
        if matches!(
            mode,
            ConfigSecurityMode::Balanced | ConfigSecurityMode::EnvTemporaryMessages
        ) {
            // Extract secrets from config_to_save (modifies it) and store them externally.
            let secrets_to_store = config_to_save.extract_secrets();
            secret_store.store(&secrets_to_store)?;
        }
        config_to_save.config_security = Some(ConfigSecurity { mode });
        config_to_save.extract_secrets = false;

        let mut config_value = serde_json::to_value(&config_to_save)?;
        strip_nulls(&mut config_value);
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let format = config_file_format_from_path(path);
        let output = if Self::uses_encrypted_config_mode(mode) {
            let plaintext = match format {
                config::FileFormat::Json => serde_json::to_string_pretty(&config_value)?,
                _ => serde_yaml_ng::to_string(&config_value)?,
            };
            encode_sensitive_config_file(&plaintext, format, Self::security_mode_label(mode))?
        } else {
            match format {
                config::FileFormat::Json => serde_json::to_string_pretty(&config_value)?,
                _ => serde_yaml_ng::to_string(&config_value)?,
            }
        };
        std::fs::write(path, output)?;
        Ok(())
    }

    fn extract_secrets_to_all(
        name: &str,
        id: &str,
        entity_type: &str,
        endpoint: &mut Endpoint,
        all_secrets: &mut HashMap<String, String>,
    ) {
        let mut endpoint_secrets = HashMap::new();
        let id_part = sanitize_id_for_env(id);
        let final_prefix = format!("MQB__{}__{}__", entity_type, id_part);
        extract_all_secrets_from_endpoint(endpoint, &final_prefix, &mut endpoint_secrets);

        if !endpoint_secrets.is_empty() {
            let name_part = sanitize_name_for_env(name);

            for (k, v) in endpoint_secrets {
                let suffix = k.strip_prefix(&final_prefix).unwrap();
                all_secrets.insert(
                    format!("MQB__{}__{}{}", entity_type, name_part, suffix),
                    v.clone(),
                );
                all_secrets.insert(format!("MQB__{}__{}{}", entity_type, id_part, suffix), v);
            }
        }
    }

    pub(crate) fn extract_secrets(&mut self) -> HashMap<String, String> {
        let mut all_secrets = HashMap::new();
        for (name, route) in &mut self.routes {
            let prefix = format!("MQB__ROUTES__{}__", sanitize_name_for_env(name));
            route.route.extract_secrets(&prefix, &mut all_secrets);
            extract_http_header_secrets_from_route(&mut route.route, &prefix, &mut all_secrets);
        }
        for consumer in &mut self.consumers {
            Self::extract_secrets_to_all(
                &consumer.name,
                &consumer.id,
                "CONSUMERS",
                &mut consumer.endpoint,
                &mut all_secrets,
            );
        }
        for publisher in &mut self.publishers {
            Self::extract_secrets_to_all(
                &publisher.name,
                &publisher.id,
                "PUBLISHERS",
                &mut publisher.endpoint,
                &mut all_secrets,
            );
            Self::extract_publisher_header_secrets(publisher, &mut all_secrets);
        }
        all_secrets
    }

    /// A publisher's request headers are rows beside the endpoint, not inside it,
    /// so `extract_secrets_to_all` never reaches them. Without this an
    /// `Authorization: Bearer …` row is exported verbatim by
    /// [`crate::cli_command::inline_config_command`].
    fn extract_publisher_header_secrets(
        publisher: &mut PublisherClient,
        all_secrets: &mut HashMap<String, String>,
    ) {
        let name_part = sanitize_name_for_env(&publisher.name);
        let id_part = sanitize_id_for_env(&publisher.id);
        for header in &mut publisher.headers {
            if header.value.is_empty() || !is_sensitive_http_header(&header.key) {
                continue;
            }
            let suffix = publisher_header_env_suffix(&header.key);
            let value = std::mem::take(&mut header.value);
            all_secrets.insert(
                format!("MQB__PUBLISHERS__{name_part}{suffix}"),
                value.clone(),
            );
            all_secrets.insert(format!("MQB__PUBLISHERS__{id_part}{suffix}"), value);
        }
    }

    pub fn referenced_secret_keys(&self) -> SecretReferenceSummary {
        let mut routes = HashMap::new();
        for (name, route_config) in &self.routes {
            let prefix = format!("MQB__ROUTES__{}__", sanitize_name_for_env(name));
            let mut route = route_config.route.clone();
            let mut secrets = HashMap::new();
            route.extract_secrets(&prefix, &mut secrets);
            extract_http_header_secrets_from_route(&mut route, &prefix, &mut secrets);
            if !secrets.is_empty() {
                let mut keys: Vec<String> = secrets.into_keys().collect();
                keys.sort();
                routes.insert(name.clone(), keys);
            }
        }

        let mut consumers = HashMap::new();
        for consumer in &self.consumers {
            let keys = self.get_referenced_keys_for_entity(
                &consumer.name,
                &consumer.id,
                "CONSUMERS",
                &consumer.endpoint,
            );
            if !keys.is_empty() {
                consumers.insert(consumer.name.clone(), keys);
            }
        }

        let mut publishers = HashMap::new();
        for publisher in &self.publishers {
            let mut keys = self.get_referenced_keys_for_entity(
                &publisher.name,
                &publisher.id,
                "PUBLISHERS",
                &publisher.endpoint,
            );
            keys.extend(Self::publisher_header_secret_keys(publisher));
            keys.sort();
            if !keys.is_empty() {
                publishers.insert(publisher.name.clone(), keys);
            }
        }

        SecretReferenceSummary {
            routes,
            consumers,
            publishers,
        }
    }

    fn get_referenced_keys_for_entity(
        &self,
        name: &str,
        id: &str,
        entity_type: &str,
        endpoint: &Endpoint,
    ) -> Vec<String> {
        let mut endpoint = endpoint.clone();
        let mut endpoint_secrets = HashMap::new();
        let temp_prefix = "SECRET__";
        extract_all_secrets_from_endpoint(&mut endpoint, temp_prefix, &mut endpoint_secrets);

        let name_part = sanitize_name_for_env(name);
        let id_part = sanitize_id_for_env(id);
        let mut keys = Vec::new();
        for k in endpoint_secrets.keys() {
            let suffix = k.strip_prefix(temp_prefix).unwrap();
            keys.push(format!("MQB__{}__{}{}", entity_type, name_part, suffix));
            keys.push(format!("MQB__{}__{}{}", entity_type, id_part, suffix));
        }
        keys.sort();
        keys
    }

    /// The env keys [`Self::extract_publisher_header_secrets`] would produce.
    fn publisher_header_secret_keys(publisher: &PublisherClient) -> Vec<String> {
        let name_part = sanitize_name_for_env(&publisher.name);
        let id_part = sanitize_id_for_env(&publisher.id);
        let mut keys = Vec::new();
        for header in &publisher.headers {
            if header.value.is_empty() || !is_sensitive_http_header(&header.key) {
                continue;
            }
            let suffix = publisher_header_env_suffix(&header.key);
            keys.push(format!("MQB__PUBLISHERS__{name_part}{suffix}"));
            keys.push(format!("MQB__PUBLISHERS__{id_part}{suffix}"));
        }
        keys
    }
}

fn publisher_header_env_suffix(key: &str) -> String {
    format!("__HEADERS__{}", key.trim().replace('-', "_").to_uppercase())
}

fn is_sensitive_http_header(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "api-key" | "x-auth-token" | "proxy-authorization"
    )
}

fn sanitize_name_for_env(name: &str) -> String {
    // Everything that is not ASCII alphanumeric becomes `_`, so a name like
    // "kafka-to-nats" yields a portable `MQB__ROUTES__KAFKA_TO_NATS__…` key
    // that a shell can actually set and that matches what the loader derives.
    name.trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sanitize_id_for_env(id: &str) -> String {
    id.trim().replace('-', "_").to_uppercase()
}

fn extract_all_secrets_from_endpoint(
    endpoint: &mut Endpoint,
    prefix: &str,
    secrets: &mut HashMap<String, String>,
) {
    endpoint.extract_secrets(prefix, secrets);
    extract_http_header_secrets_from_endpoint(endpoint, prefix, secrets);
}

fn extract_http_header_secrets_from_route(
    route: &mut Route,
    prefix: &str,
    secrets: &mut HashMap<String, String>,
) {
    extract_http_header_secrets_from_endpoint(
        &mut route.input,
        &format!("{}__INPUT", prefix),
        secrets,
    );
    extract_http_header_secrets_from_endpoint(
        &mut route.output,
        &format!("{}__OUTPUT", prefix),
        secrets,
    );
}

fn extract_http_header_secrets_from_endpoint(
    endpoint: &mut Endpoint,
    prefix: &str,
    secrets: &mut HashMap<String, String>,
) {
    for (index, middleware) in endpoint.middlewares.iter_mut().enumerate() {
        if let Middleware::Dlq(cfg) = middleware {
            extract_http_header_secrets_from_endpoint(
                &mut cfg.endpoint,
                &format!("{}__MIDDLEWARES__{}__DLQ__ENDPOINT", prefix, index),
                secrets,
            );
        }
    }

    match &mut endpoint.endpoint_type {
        EndpointType::Http(cfg) => {
            let keys: Vec<String> = cfg.custom_headers.keys().cloned().collect();
            for key in keys {
                if !is_sensitive_http_header(&key) {
                    continue;
                }
                if let Some(value) = cfg.custom_headers.remove(&key) {
                    secrets.insert(
                        format!(
                            "{}__HTTP__CUSTOM_HEADERS__{}",
                            prefix,
                            key.trim().replace('-', "_").to_uppercase()
                        ),
                        value,
                    );
                }
            }
        }
        EndpointType::Fanout(endpoints) => {
            for (index, nested) in endpoints.iter_mut().enumerate() {
                extract_http_header_secrets_from_endpoint(
                    nested,
                    &format!("{}__FANOUT__{}", prefix, index),
                    secrets,
                );
            }
        }
        EndpointType::Switch(cfg) => {
            for (case_name, nested) in &mut cfg.cases {
                extract_http_header_secrets_from_endpoint(
                    nested,
                    &format!("{}__SWITCH__CASES__{}", prefix, case_name.to_uppercase()),
                    secrets,
                );
            }
            if let Some(default) = &mut cfg.default {
                extract_http_header_secrets_from_endpoint(
                    default,
                    &format!("{}__SWITCH__DEFAULT", prefix),
                    secrets,
                );
            }
        }
        EndpointType::Reader(nested) => {
            extract_http_header_secrets_from_endpoint(
                nested,
                &format!("{}__READER", prefix),
                secrets,
            );
        }
        _ => {}
    }
}

fn strip_nulls(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                strip_nulls(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                strip_nulls(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSecretStore {
        stored: Mutex<Vec<HashMap<String, String>>>,
    }

    impl SecretStore for RecordingSecretStore {
        fn store(&self, secrets: &HashMap<String, String>) -> Result<()> {
            self.stored.lock().unwrap().push(secrets.clone());
            Ok(())
        }
    }

    fn sample_security_config(mode: &str) -> AppConfig {
        serde_yaml_ng::from_str(&format!(
            r#"
config_security:
  mode: {mode}
publishers:
  - name: "orders_http"
    endpoint:
      http:
        url: "https://example.test/orders"
        custom_headers:
          authorization: "Bearer token"
"#
        ))
        .unwrap()
    }

    #[test]
    fn test_config_deserialization() {
        let yaml_config = r#"
log_level: debug
logger: plain
routes:
  kafka_to_nats:
    input:
      kafka:
        brokers: "kafka:9092"
        group_id: "bridge_group"
        topic: "in_topic"
    output:
      nats:
        url: "nats://nats:4222"
        subject: "out_subject"
"#;

        let config: Result<AppConfig, _> = serde_yaml_ng::from_str(yaml_config);
        dbg!(&config);
        assert!(config.is_ok());
        let mut config = config.unwrap();
        config.migrate_legacy_routes();

        assert_eq!(config.log_level, "debug");
        assert!(config.routes.is_empty());
        assert_eq!(config.publishers.len(), 1);
        assert_eq!(config.consumers.len(), 1);

        let consumer = &config.consumers[0];
        if let mq_bridge::models::EndpointType::Kafka(k) = &consumer.endpoint.endpoint_type {
            assert_eq!(k.url, "kafka:9092");
            assert_eq!(k.topic.as_deref(), Some("in_topic"));
        }
        match &consumer.output {
            ConsumerOutputConfig::Publisher {
                publisher,
                publisher_id,
            } => {
                assert_eq!(publisher, "kafka_to_nats_publisher");
                assert_eq!(
                    publisher_id.as_deref(),
                    Some(config.publishers[0].id.as_str())
                );
            }
            other => panic!("expected publisher output, got {other:?}"),
        }
    }
    #[test]
    fn test_config_from_env_vars() {
        // Set environment variables
        // Clear the var first to avoid interference from other tests
        unsafe {
            std::env::remove_var("MQB__LOG_LEVEL");
            std::env::set_var("MQB__LOG_LEVEL", "trace");
            std::env::set_var("MQB__LOGGER", "json");

            // Route 0: Kafka to NATS
            std::env::set_var(
                "MQB__ROUTES__KAFKA_TO_NATS_FROM_ENV__INPUT__KAFKA__BROKERS",
                "env-kafka:9092",
            );
            // Source
            std::env::set_var(
                "MQB__ROUTES__KAFKA_TO_NATS_FROM_ENV__INPUT__KAFKA__GROUP_ID",
                "env-group",
            );
            std::env::set_var(
                "MQB__ROUTES__KAFKA_TO_NATS_FROM_ENV__INPUT__KAFKA__TOPIC",
                "env-in-topic",
            );
            // Sink
            std::env::set_var(
                "MQB__ROUTES__KAFKA_TO_NATS_FROM_ENV__OUTPUT__NATS__URL",
                "nats://env-nats:4222",
            );
            std::env::set_var(
                "MQB__ROUTES__KAFKA_TO_NATS_FROM_ENV__OUTPUT__NATS__SUBJECT",
                "env-out-subject",
            );

            std::env::set_var("CONFIG_FILE", "_"); // ignore existing config.yaml
        }
        // Load config
        let (config, _) = load_config(None, None, None, None).unwrap();

        // Assertions
        assert_eq!(config.log_level, "trace");
        assert_eq!(config.logger, "json");
        assert!(config.routes.is_empty());
        assert_eq!(config.publishers.len(), 1);
        assert_eq!(config.consumers.len(), 1);

        let consumer = &config.consumers[0];
        assert_eq!(consumer.name, "kafka_to_nats_from_env");
        if let mq_bridge::models::EndpointType::Kafka(k) = &consumer.endpoint.endpoint_type {
            assert_eq!(k.url, "env-kafka:9092"); // group_id is now optional
            assert_eq!(k.topic.as_deref(), Some("env-in-topic"));
        } else {
            panic!("Expected Kafka source endpoint");
        }
    }

    #[test]
    fn test_config_expands_placeholders_from_inline_env_vars() {
        let yaml_config = r#"
env_vars:
  baseUrl: "https://api.example.test"
publishers:
  - name: "orders list"
    endpoint:
      http:
        url: "${baseUrl}/orders"
"#;

        let (config, _) = load_config_internal(
            Some("_".to_string()),
            None,
            None,
            Some(yaml_config.to_string()),
            false,
            false,
        )
        .unwrap();

        match &config.publishers[0].endpoint.endpoint_type {
            mq_bridge::models::EndpointType::Http(http) => {
                assert_eq!(http.url, "https://api.example.test/orders");
            }
            other => panic!("expected http publisher, got {other:?}"),
        }
        assert_eq!(
            config.env_vars.get("baseUrl").map(String::as_str),
            Some("https://api.example.test")
        );
    }

    #[test]
    fn app_schema_uses_runtime_batch_size_default() {
        let schema = app_config_schema();
        for definition in ["RouteConfig", "ConsumerConfig"] {
            let batch_size = schema
                .pointer(&format!("/$defs/{definition}/properties/batch_size"))
                .unwrap();
            assert_eq!(
                batch_size["default"],
                mq_bridge::models::RouteOptions::default().batch_size
            );
            assert_eq!(batch_size["minimum"], 1);
        }
    }

    #[test]
    fn app_schema_includes_the_built_in_pulsar_endpoint() {
        let schema = app_config_schema();
        assert_eq!(
            schema.pointer("/$defs/PulsarConfig/required/0"),
            Some(&serde_json::json!("url"))
        );
        assert_eq!(
            schema.pointer("/$defs/PulsarConfig/additionalProperties"),
            Some(&serde_json::json!(false))
        );
        assert!(
            schema["$defs"]["Endpoint"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["required"] == serde_json::json!(["pulsar"]))
        );
    }

    #[test]
    fn mongodb_legacy_consume_modes_load_from_yaml() {
        fn mongo_config(extra: &str) -> std::result::Result<AppConfig, serde_yaml_ng::Error> {
            serde_yaml_ng::from_str(&format!(
                r#"
consumers:
  - name: mongo
    endpoint:
      mongodb:
        url: mongodb://localhost:27017
        database: app
        collection: orders
{extra}
"#
            ))
        }

        let error = mongo_config("        consume: subscriber").unwrap_err();
        assert!(error.to_string().contains("subscriber"), "{error}");

        let config = mongo_config("        change_stream: true").unwrap();
        let EndpointType::MongoDb(mongo) = &config.consumers[0].endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(
            mongo.resolved_consume(),
            mq_bridge::models::MongoConsume::CaptureNew
        );

        let config =
            mongo_config("        consume: snapshot\n        change_stream: true").unwrap();
        let EndpointType::MongoDb(mongo) = &config.consumers[0].endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(
            mongo.resolved_consume(),
            mq_bridge::models::MongoConsume::Snapshot
        );
    }

    // A config file that is nothing but one route: the `routes:` map and the
    // route's name are what a single-route file should not have to invent.
    #[test]
    fn a_bare_input_output_config_becomes_one_named_route() {
        let raw: serde_json::Value = serde_yaml_ng::from_str(
            r#"
log_level: debug
input:
  http: { url: "0.0.0.0:8443" }
output:
  http: { url: "https://upstream.internal/" }
batch_size: 8
"#,
        )
        .unwrap();

        let lifted = lift_bare_routes(raw).expect("a top-level input is a single-route config");
        let mut config: AppConfig = serde_json::from_value(lifted).unwrap();

        assert_eq!(config.log_level, "debug");
        let route = config
            .routes
            .get(SINGLE_ROUTE_NAME)
            .expect("the route is named");
        assert!(route.enabled);
        // Route options belong to the route, not to the application.
        assert_eq!(route.route.options.batch_size, 8);

        config.migrate_legacy_routes();
        assert_eq!(config.consumers.len(), 1);
        assert_eq!(config.consumers[0].name, SINGLE_ROUTE_NAME);
    }

    // `extract_secrets` is skipped when false, so it is absent from the serialized
    // default that seeds `app_level_fields` — it has to be named explicitly or a
    // bare route swallows it and route deserialization rejects the unknown key.
    #[test]
    fn a_bare_route_keeps_extract_secrets_at_application_level() {
        let raw: serde_json::Value = serde_yaml_ng::from_str(
            r#"
extract_secrets: true
input:
  memory: { topic: "in" }
output:
  memory: { topic: "out" }
"#,
        )
        .unwrap();

        let lifted = lift_bare_routes(raw).expect("a top-level input is a single-route config");
        let route = &lifted["routes"][SINGLE_ROUTE_NAME];
        assert!(route.get("extract_secrets").is_none());
        assert!(route.get("input").is_some());

        let mut config: AppConfig = serde_json::from_value(lifted).unwrap();
        assert!(config.extract_secrets);

        // And the legacy flag still migrates rather than being lost in the route.
        config.migrate_legacy_routes();
        assert_eq!(config.security_mode(), ConfigSecurityMode::Balanced);
    }

    #[test]
    fn a_config_with_a_routes_map_is_left_alone() {
        let raw: serde_json::Value = serde_yaml_ng::from_str(
            r#"
routes:
  named:
    input: { memory: { topic: "in" } }
"#,
        )
        .unwrap();
        assert!(lift_bare_routes(raw).is_none());

        let raw: serde_json::Value = serde_yaml_ng::from_str("consumers: []").unwrap();
        assert!(lift_bare_routes(raw).is_none());
    }

    // The shape the engine's own configuration guide is written in: route names
    // at the top level, no `routes:` wrapper.
    #[test]
    fn top_level_named_routes_are_lifted_and_others_left_alone() {
        let raw: serde_json::Value = serde_yaml_ng::from_str(
            r#"
ui_addr: "127.0.0.1:9091"
kafka_to_nats:
  input: { memory: { topic: "in" } }
  output: { memory: { topic: "out" } }
not_a_route:
  something: else
"#,
        )
        .unwrap();

        let lifted = lift_bare_routes(raw).expect("a top-level route map is liftable");
        assert!(
            lifted.get("not_a_route").is_some(),
            "a key that is not a route must stay where it is"
        );

        let config: AppConfig = serde_json::from_value(lifted).unwrap();
        assert_eq!(config.ui_addr, "127.0.0.1:9091");
        assert_eq!(config.routes.len(), 1);
        assert!(config.routes.contains_key("kafka_to_nats"));
    }

    // A key that is neither an app setting nor a route field must be named, not
    // silently dropped the way an unknown top-level key otherwise would be.
    #[test]
    fn a_misspelled_key_in_a_single_route_config_is_rejected() {
        let raw: serde_json::Value = serde_yaml_ng::from_str(
            r#"
input: { memory: { topic: "in" } }
batchsize: 8
"#,
        )
        .unwrap();
        let lifted = lift_bare_routes(raw).expect("a top-level input is a single-route config");
        let error = serde_json::from_value::<AppConfig>(lifted).unwrap_err();
        assert!(
            error.to_string().contains("batchsize"),
            "the error should name the key, got: {error}"
        );
    }

    #[test]
    fn test_config_deserializes_disabled_route() {
        let yaml_config = r#"
routes:
  paused_route:
    enabled: false
    input:
      memory:
        topic: "in_topic"
    output:
      memory:
        topic: "out_topic"
"#;

        let mut config: AppConfig = serde_yaml_ng::from_str(yaml_config).unwrap();
        config.migrate_legacy_routes();
        assert!(config.routes.is_empty());
        let consumer = &config.consumers[0];
        assert_eq!(consumer.name, "paused_route");
        // The flag has to survive the migration: without it the route would be
        // started at boot like any other consumer.
        assert!(!consumer.enabled);
        assert!(!consumer.message_capture.enabled);
        assert!(matches!(
            consumer.endpoint.endpoint_type,
            mq_bridge::models::EndpointType::Memory(_)
        ));
    }

    #[test]
    fn test_route_migration_reuses_matching_publishers() {
        let yaml_config = r#"
publishers:
  - name: "existing_pub"
    endpoint:
      memory:
        topic: "shared"
routes:
  route_alpha:
    input:
      memory:
        topic: "in"
    output:
      memory:
        topic: "shared"
"#;

        let mut config: AppConfig = serde_yaml_ng::from_str(yaml_config).unwrap();
        config.migrate_legacy_routes();
        assert!(config.routes.is_empty());
        assert_eq!(config.publishers.len(), 1);
        assert_eq!(config.consumers.len(), 1);
        match &config.consumers[0].output {
            ConsumerOutputConfig::Publisher { publisher, .. } => {
                assert_eq!(publisher, "existing_pub");
            }
            other => panic!("expected publisher output, got {other:?}"),
        }
    }

    #[test]
    fn test_consumer_output_deserializes_response_and_publisher_modes() {
        let yaml_config = r#"
consumers:
  - name: "reply_consumer"
    endpoint:
      http:
        url: "0.0.0.0:8080"
    message_capture:
      enabled: false
      keep_last: 25
    output:
      mode: response
      response:
        headers:
          content-type: "application/json"
        payload: "{\"ok\":true}"
  - name: "forward_consumer"
    endpoint:
      memory:
        topic: "orders"
    output:
      mode: publisher
      publisher: "orders_pub"
"#;

        let mut config: AppConfig = serde_yaml_ng::from_str(yaml_config).unwrap();
        config.migrate_legacy_routes();
        assert_eq!(config.consumers.len(), 2);

        match &config.consumers[0].output {
            ConsumerOutputConfig::Response { response } => {
                let response = response.clone().expect("response payload");
                assert_eq!(response.payload, "{\"ok\":true}");
                assert_eq!(
                    response.headers.get("content-type").map(String::as_str),
                    Some("application/json")
                );
            }
            other => panic!("expected response output, got {other:?}"),
        }
        assert!(!config.consumers[0].message_capture.enabled);
        assert_eq!(config.consumers[0].message_capture.keep_last, 25);

        match &config.consumers[1].output {
            ConsumerOutputConfig::Publisher { publisher, .. } => {
                assert_eq!(publisher, "orders_pub");
            }
            other => panic!("expected publisher output, got {other:?}"),
        }
        assert!(config.consumers[1].message_capture.enabled);
        assert_eq!(config.consumers[1].message_capture.keep_last, 100);
    }

    #[test]
    fn test_legacy_consumer_response_migrates_to_output() {
        let yaml_config = r#"
consumers:
  - name: "reply_consumer"
    endpoint:
      http:
        url: "0.0.0.0:8080"
    response:
      headers:
        content-type: "application/json"
      payload: "{\"ok\":true}"
"#;

        let mut config: AppConfig = serde_yaml_ng::from_str(yaml_config).unwrap();
        config.migrate_legacy_routes();

        assert_eq!(config.consumers.len(), 1);
        assert!(config.consumers[0].response.is_none());
        match &config.consumers[0].output {
            ConsumerOutputConfig::Response { response } => {
                let response = response.clone().expect("response payload");
                assert_eq!(response.payload, "{\"ok\":true}");
                assert_eq!(
                    response.headers.get("content-type").map(String::as_str),
                    Some("application/json")
                );
            }
            other => panic!("expected response output, got {other:?}"),
        }
    }

    #[test]
    fn test_legacy_extract_secrets_migrates_to_security_mode() {
        let yaml_config = r#"
extract_secrets: true
publishers: []
consumers: []
"#;

        let mut config: AppConfig = serde_yaml_ng::from_str(yaml_config).unwrap();
        config.migrate_legacy_routes();

        assert_eq!(config.security_mode(), ConfigSecurityMode::Balanced);
        assert_eq!(
            config
                .config_security
                .as_ref()
                .map(|security| security.mode),
            Some(ConfigSecurityMode::Balanced)
        );
        assert!(!config.extract_secrets);
    }

    #[test]
    fn test_save_unencrypted_keeps_inline_secrets() {
        let config = sample_security_config("unencrypted");
        let secret_store = RecordingSecretStore::default();
        let path = std::env::temp_dir().join("mqb-config-unencrypted.yml");

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("mode: unencrypted"));
        assert!(saved.contains("Bearer token"));
        assert!(secret_store.stored.lock().unwrap().is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_balanced_extracts_and_stores_secrets() {
        let config = sample_security_config("balanced");
        let secret_store = RecordingSecretStore::default();
        let path = std::env::temp_dir().join("mqb-config-balanced.yml");

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("mode: balanced"));
        assert!(!saved.contains("Bearer token"));
        assert!(!saved.contains("extract_secrets"));
        assert_eq!(secret_store.stored.lock().unwrap().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_env_temporary_messages_extracts_and_stores_secrets() {
        let config = sample_security_config("env_temporary_messages");
        let secret_store = RecordingSecretStore::default();
        let path = std::env::temp_dir().join("mqb-config-env-temporary-messages.yml");

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("mode: env_temporary_messages"));
        assert!(!saved.contains("Bearer token"));
        assert_eq!(secret_store.stored.lock().unwrap().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_temporary_messages_keeps_inline_secrets() {
        let config = sample_security_config("temporary_messages");
        let secret_store = RecordingSecretStore::default();
        let path = std::env::temp_dir().join("mqb-config-temporary-messages.yml");

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("mode: temporary_messages"));
        assert!(saved.contains("Bearer token"));
        assert!(secret_store.stored.lock().unwrap().is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_save_durable_encrypts_without_storing_secrets() {
        let config = sample_security_config("durable");
        let secret_store = RecordingSecretStore::default();
        let path = std::env::temp_dir().join("mqb-config-durable.yml");
        let _guard = crate::encrypted_config::test_config_master_key_lock().blocking_lock();
        unsafe {
            std::env::set_var(
                crate::encrypted_config::CONFIG_MASTER_KEY_ENV,
                "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
            );
        }

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("mode: durable"));
        assert!(saved.contains("encrypted_config"));
        assert!(!saved.contains("Bearer token"));
        assert!(secret_store.stored.lock().unwrap().is_empty());

        let _ = std::fs::remove_file(path);
        unsafe {
            std::env::remove_var(crate::encrypted_config::CONFIG_MASTER_KEY_ENV);
        }
    }
}
