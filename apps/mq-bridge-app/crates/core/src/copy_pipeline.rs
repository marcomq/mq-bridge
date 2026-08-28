//! Copy-command conveniences layered over mq-bridge's route abstractions.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use mq_bridge::ReceivedBatch;
use mq_bridge::models::{Endpoint, EndpointType, FileConsumerMode, Middleware, MongoConsume};
use mq_bridge::traits::{
    BoxFuture, ConsumerError, CustomMiddlewareFactory, EndpointStatus, MessageConsumer,
};
use serde_json::json;
use sha2::{Digest, Sha256};

const COUNTER_MIDDLEWARE_NAME: &str = "__mq_bridge_app_copy_counter_v1";
static COUNTER_FACTORY_REGISTRATION: OnceLock<Result<(), String>> = OnceLock::new();

/// Tallies handed out by [`configure_counter`], looked up by the token its
/// middleware config carries. A factory is built from JSON by name, so a token
/// is the only way to reach the `Arc` the caller is holding.
static COPY_COUNTERS: OnceLock<Mutex<HashMap<u64, Arc<AtomicU64>>>> = OnceLock::new();
static NEXT_COUNTER_TOKEN: AtomicU64 = AtomicU64::new(0);

fn copy_counters() -> &'static Mutex<HashMap<u64, Arc<AtomicU64>>> {
    COPY_COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The native mechanism used to resume a copy source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeCapability {
    Native,
    CursorBased,
    ExternalCheckpoint,
}

impl ResumeCapability {
    /// Names the mechanism in logs, so a resumed copy says what it resumed from.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::CursorBased => "cursor",
            Self::ExternalCheckpoint => "external-checkpoint",
        }
    }
}

/// Attaches mq-bridge's `filter` middleware to the source endpoint.
///
/// Consumer middlewares are applied in reverse, so the last entry is the
/// innermost wrapper: the filter runs immediately after the source and before
/// URI-configured transform or other processing middleware. The expression is
/// compiled when the route starts, which is also where an invalid one is
/// reported — before anything is copied.
pub fn configure_filter(input: &mut Endpoint, expression: &str) {
    insert_before_buffer(input, Middleware::Filter(expression.to_string()));
}

/// Attaches a row counter next to the source and returns the shared tally.
///
/// Innermost, so it counts everything the source produced, before any
/// middleware has had the chance to drop a message.
///
/// Counts messages as the route receives them. A source that redelivers after a
/// failed batch therefore counts those messages twice — the tally describes work
/// done, and only equals the destination's row count on a clean run.
pub fn configure_counter(input: &mut Endpoint) -> anyhow::Result<Arc<AtomicU64>> {
    let (entry, counter) = new_counter()?;
    insert_before_buffer(input, entry);
    Ok(counter)
}

/// Attaches a row counter *outside* every other source middleware and returns
/// the shared tally: what the whole chain let through, which is what "copied"
/// means to whoever ran the command.
///
/// Consumer middlewares are applied in reverse, so the front of the list is the
/// outermost wrapper and the last one to see a message before the route does.
/// Anything less than outermost undercounts a drop: a counter sitting inside a
/// URI-configured `transform` still tallies the rows that transform is about to
/// reject.
pub fn configure_delivered_counter(input: &mut Endpoint) -> anyhow::Result<Arc<AtomicU64>> {
    let (entry, counter) = new_counter()?;
    input.middlewares.insert(0, entry);
    Ok(counter)
}

/// Registers a tally and returns the middleware entry that feeds it.
fn new_counter() -> anyhow::Result<(Middleware, Arc<AtomicU64>)> {
    ensure_counter_factory()?;
    let counter = Arc::new(AtomicU64::new(0));
    let token = NEXT_COUNTER_TOKEN.fetch_add(1, Ordering::Relaxed);
    copy_counters()
        .lock()
        .map_err(|_| anyhow!("copy counter registry is poisoned"))?
        .insert(token, Arc::clone(&counter));
    Ok((
        Middleware::Custom {
            name: COUNTER_MIDDLEWARE_NAME.to_string(),
            config: json!({ "token": token }),
        },
        counter,
    ))
}

/// `buffer` is the one middleware a copy wrapper must not slip under: it
/// downcasts its inner consumer against a closed list of cancel-safe sources, so
/// anything between it and the source fails route startup. Sitting just outside
/// it keeps these wrappers ahead of every other middleware.
fn insert_before_buffer(input: &mut Endpoint, entry: Middleware) {
    match input
        .middlewares
        .iter()
        .rposition(|middleware| matches!(middleware, Middleware::Buffer(_)))
    {
        Some(index) => input.middlewares.insert(index, entry),
        None => input.middlewares.push(entry),
    }
}

/// Configures the source's existing durable resume mechanism and returns its kind.
///
/// The generated identity includes the credential-redacted source, destination,
/// and filter, so changing pipeline semantics starts a distinct checkpoint while
/// credential rotation alone does not.
pub fn configure_resume(
    input: &mut Endpoint,
    output: &Endpoint,
    filter: Option<&str>,
) -> anyhow::Result<ResumeCapability> {
    let state_id = checkpoint_identity(input, output, filter)?;

    match &mut input.endpoint_type {
        EndpointType::Kafka(config) => {
            config.group_id.get_or_insert_with(|| state_id.clone());
            Ok(ResumeCapability::Native)
        }
        EndpointType::MongoDb(config) => {
            let mode = config.consume.unwrap_or({
                if config.change_stream {
                    MongoConsume::CaptureNew
                } else {
                    MongoConsume::CaptureAll
                }
            });
            match mode {
                MongoConsume::CaptureNew | MongoConsume::CaptureAll => {
                    config.cursor_id.get_or_insert_with(|| state_id.clone());
                    Ok(ResumeCapability::CursorBased)
                }
                MongoConsume::Snapshot => bail!(
                    "source `mongodb` does not support resumable copy in snapshot mode; use capture_all or capture_new"
                ),
                MongoConsume::Consumer => unsupported("mongodb consumer mode"),
            }
        }
        EndpointType::PostgresCdc(config) => {
            if config.temporary_slot {
                bail!(
                    "source `postgres_cdc` cannot resume with temporary_slot=true; use a persistent replication slot"
                );
            }
            if config.slot_name == "mq_bridge_slot" {
                config.slot_name = postgres_slot_name(&state_id);
            }
            config.cursor_id.get_or_insert_with(|| state_id.clone());
            Ok(ResumeCapability::Native)
        }
        EndpointType::Sqlx(config) if config.publication.is_some() => {
            config.cursor_id.get_or_insert_with(|| state_id.clone());
            config
                .slot_name
                .get_or_insert_with(|| postgres_slot_name(&state_id));
            Ok(ResumeCapability::Native)
        }
        EndpointType::Sqlx(config) => {
            if config.cursor_column.is_none() {
                bail!(
                    "source `sql` needs a monotonic `cursor_column` for resumable copy; add `?cursor_column=<column>` to --from"
                );
            }
            config.cursor_id.get_or_insert_with(|| state_id.clone());
            Ok(ResumeCapability::CursorBased)
        }
        EndpointType::ClickHouse(config) => {
            if config.cursor_column.is_none() {
                bail!("source `clickhouse` needs a monotonic `cursor_column` for resumable copy");
            }
            if config.checkpoint_store.is_none() {
                bail!(
                    "source `clickhouse` needs an external `checkpoint_store` for resumable copy"
                );
            }
            config.cursor_id.get_or_insert_with(|| state_id.clone());
            Ok(ResumeCapability::ExternalCheckpoint)
        }
        EndpointType::ObjectStore(config) => {
            if config.checkpoint_store.is_none() {
                bail!(
                    "source `object_store` needs an external `checkpoint_store` for resumable copy"
                );
            }
            config.cursor_id.get_or_insert_with(|| state_id.clone());
            Ok(ResumeCapability::ExternalCheckpoint)
        }
        EndpointType::File(config) => match config.mode {
            Some(FileConsumerMode::GroupSubscribe { .. }) => bail!(
                "source `file` has offset state, but resumable copy is not enabled because its current batch commit is not safe across partial failures"
            ),
            _ => unsupported("file"),
        },
        EndpointType::Nats(_) => bail!(
            "source `nats` has durable JetStream consumers, but resumable copy is not enabled because the durable identity cannot include the destination and filter"
        ),
        EndpointType::Mqtt(_) => unsupported("mqtt"),
        endpoint_type => unsupported(endpoint_kind(endpoint_type)),
    }
}

fn unsupported(source: &str) -> anyhow::Result<ResumeCapability> {
    bail!("source `{source}` does not support resumable copy")
}

fn endpoint_kind(endpoint_type: &EndpointType) -> &'static str {
    match endpoint_type {
        EndpointType::Amqp(_) => "amqp",
        EndpointType::Aws(_) => "aws",
        EndpointType::File(_) => "file",
        EndpointType::Grpc(_) => "grpc",
        EndpointType::Http(_) => "http",
        EndpointType::IbmMq(_) => "ibm_mq",
        EndpointType::Kafka(_) => "kafka",
        EndpointType::Memory(_) => "memory",
        EndpointType::MongoDb(_) => "mongodb",
        EndpointType::Mqtt(_) => "mqtt",
        EndpointType::Nats(_) => "nats",
        EndpointType::Null => "null",
        EndpointType::ObjectStore(_) => "object_store",
        EndpointType::PostgresCdc(_) => "postgres_cdc",
        EndpointType::RedisStreams(_) => "redis_streams",
        EndpointType::Sqlx(_) => "sql",
        EndpointType::WebSocket(_) => "websocket",
        EndpointType::ZeroMq(_) => "zeromq",
        _ => "this endpoint",
    }
}

fn postgres_slot_name(state_id: &str) -> String {
    format!("mqb_{}", state_id.replace('-', "_"))
}

fn checkpoint_identity(
    input: &Endpoint,
    output: &Endpoint,
    filter: Option<&str>,
) -> anyhow::Result<String> {
    let mut definition = json!({
        "source": input,
        "destination": output,
        "filter": filter,
    });
    redact_credentials(&mut definition, None, None);
    strip_absent_fields(&mut definition, 0);
    let encoded = serde_json::to_vec(&definition)?;
    let digest = Sha256::digest(encoded);
    Ok(format!("copy-{}", hex::encode(&digest[..16])))
}

/// Drops absent config fields so the identity survives an mq-bridge upgrade.
///
/// Endpoint configs serialize every field, so a release that adds one optional
/// setting would otherwise change every existing checkpoint id and silently
/// restart each `--resume` from the beginning. An added `Option` arrives as
/// `null` and is removed here; a new field with a non-null default still moves
/// the identity.
///
/// Endpoint objects themselves are left alone. Their type is a flattened tag,
/// and a `null`-valued variant such as `null:` is the only thing naming it.
fn strip_absent_fields(value: &mut serde_json::Value, depth: usize) {
    match value {
        serde_json::Value::Object(map) => {
            if depth >= 2 && map.values().any(|value| !value.is_null()) {
                map.retain(|_, value| !value.is_null());
            }
            for value in map.values_mut() {
                strip_absent_fields(value, depth + 1);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                strip_absent_fields(value, depth);
            }
        }
        _ => {}
    }
}

/// Replaces credential values so rotating a password does not fork the checkpoint.
///
/// Not a disclosure control — the result is hashed and never shown. It only
/// decides which fields may change the identity, which makes over-redaction the
/// risk worth avoiding: blanking a *semantic* field would let two genuinely
/// different pipelines share one checkpoint. Names are matched narrowly, and
/// anything unrecognised keeps contributing to the hash.
fn redact_credentials(value: &mut serde_json::Value, key: Option<&str>, parent: Option<&str>) {
    match value {
        serde_json::Value::Object(map) => {
            for (child_key, child) in map {
                redact_credentials(child, Some(child_key), key);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_credentials(value, key, parent);
            }
        }
        serde_json::Value::String(text) => {
            if is_credential(key, parent) {
                *text = "[redacted]".to_string();
                return;
            }
            if let Ok(mut url) = url::Url::parse(text)
                && url.password().is_some()
            {
                let _ = url.set_password(None);
                *text = url.to_string();
            }
        }
        _ => {}
    }
}

/// Expands `${VAR}` and `$VAR` in a copy URI from the environment.
///
/// This is what keeps a password out of both the shell history and `argv`:
/// single-quoted, `postgres://${PGUSER}:${PGPASSWORD}@host/db` never carries the
/// secret on the command line, where any user on the box could read it from
/// `/proc/<pid>/cmdline`. Config files already expand the same way.
///
/// An undefined variable is an error rather than a literal — the config path
/// tolerates those, but here a typo would otherwise be sent as the password and
/// fail as an authentication error somewhere far from its cause. A literal `$`
/// in a URI must be written `%24`.
pub fn expand_uri_variables(uri: &str) -> anyhow::Result<String> {
    let mut missing = Vec::new();
    let expanded = shellexpand::env_with_context_no_errors(uri, |key| {
        std::env::var(key).ok().or_else(|| {
            missing.push(key.to_string());
            Some(String::new())
        })
    })
    .into_owned();

    if !missing.is_empty() {
        bail!(
            "undefined environment variable `{}` in endpoint URI (write a literal `$` as `%24`)",
            missing.join("`, `")
        );
    }
    Ok(expanded)
}

/// Replaces the credentials in a copy URI with `***` so it can be logged.
///
/// Unlike [`redact_credentials`], this one *is* a disclosure control: it feeds
/// the line naming the route, which lands in journald, Docker logs, CI output
/// and any terminal recording. Over-redaction is the safe failure here — the
/// opposite of the checkpoint-identity trade-off, where blanking a semantic
/// field would silently merge two pipelines.
///
/// Middleware segments carry secrets of their own (`|encryption?key=…`), so each
/// `|`-separated segment is redacted with its own name standing in as the parent
/// that decides whether a bare `key` is key material. A literal `|` inside a URI
/// has to be written `%7C`, so splitting on it is safe.
pub fn redact_uri(uri: &str) -> String {
    uri.split('|')
        .map(redact_uri_segment)
        .collect::<Vec<_>>()
        .join("|")
}

fn redact_uri_segment(segment: &str) -> String {
    let (head, query) = match segment.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (segment, None),
    };
    let mut redacted = redact_uri_password(head);
    if let Some(query) = query {
        // A segment with no scheme is a middleware, and its name is what tells a
        // bare `key` apart from a partition key.
        let parent = (!head.contains("://")).then_some(head);
        redacted.push('?');
        redacted.push_str(&redact_query_credentials(query, parent));
    }
    redacted
}

/// Blanks the password in a `scheme://user:password@host` authority.
fn redact_uri_password(head: &str) -> String {
    let Some(scheme_end) = head.find("://") else {
        return head.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = head[authority_start..]
        .find(['/', '#'])
        .map_or(head.len(), |offset| authority_start + offset);
    let authority = &head[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return head.to_string();
    };
    let Some((user, _password)) = authority[..at].split_once(':') else {
        return head.to_string();
    };
    format!(
        "{}{user}:***{}",
        &head[..authority_start],
        &head[authority_start + at..]
    )
}

fn redact_query_credentials(query: &str, parent: Option<&str>) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_credential(Some(key), parent) => format!("{key}=***"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether `key` names a credential rather than a pipeline setting.
///
/// `key` on its own is ambiguous — key material under `encryption`, a dedup key
/// *template* under `deduplication` — so the enclosing field decides. Names like
/// `partition_key`, `metadata_key` and `cookie_metadata_key` are deliberately
/// excluded: they name a field to read, not a secret.
fn is_credential(key: Option<&str>, parent: Option<&str>) -> bool {
    let Some(key) = key.map(str::to_ascii_lowercase) else {
        return false;
    };
    ["password", "passphrase", "secret", "token", "credential"]
        .iter()
        .any(|needle| key.contains(needle))
        || matches!(key.as_str(), "authorization" | "cookie" | "access_key")
        || (key == "key" && parent == Some("encryption"))
        || parent == Some("decrypt_keys")
}

fn ensure_counter_factory() -> anyhow::Result<()> {
    COUNTER_FACTORY_REGISTRATION
        .get_or_init(|| {
            mq_bridge::extensions::register_middleware_factory(
                COUNTER_MIDDLEWARE_NAME,
                Arc::new(CopyCounterFactory),
            )
            .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

#[derive(Debug)]
struct CopyCounterFactory;

#[async_trait]
impl CustomMiddlewareFactory for CopyCounterFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        _route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let token = config
            .get("token")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow!("copy counter requires a token"))?;
        let counter = copy_counters()
            .lock()
            .map_err(|_| anyhow!("copy counter registry is poisoned"))?
            .get(&token)
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("copy counter {token} is not registered"))?;
        Ok(Box::new(CountingConsumer {
            inner: consumer,
            counter,
        }))
    }
}

struct CountingConsumer {
    inner: Box<dyn MessageConsumer>,
    counter: Arc<AtomicU64>,
}

#[async_trait]
impl MessageConsumer for CountingConsumer {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let batch = self.inner.receive_batch(max_messages).await?;
        self.counter
            .fetch_add(batch.messages.len() as u64, Ordering::Relaxed);
        Ok(batch)
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.inner.close().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_bridge::models::{
        BufferMiddleware, KafkaConfig, MetricsMiddleware, MqttConfig, SqlxConfig,
    };

    fn endpoint(endpoint_type: EndpointType) -> Endpoint {
        Endpoint::new(endpoint_type)
    }

    // The route-start log reaches journald, Docker logs and CI output, so the
    // URI it names must not carry the password that was typed on the command line.
    #[test]
    fn logged_uris_drop_passwords_but_keep_the_pipeline_settings() {
        assert_eq!(
            redact_uri(
                "postgres://alice:hunter2@db.internal:5432/shop?table=orders&sslmode=require"
            ),
            "postgres://alice:***@db.internal:5432/shop?table=orders&sslmode=require"
        );

        // Middleware secrets travel in the query string, and `key` is only key
        // material because the segment it sits in is `encryption`.
        assert_eq!(
            redact_uri(
                "file:///tmp/out.messages?format=normal|encryption?cipher=aes256gcm&key=AAAA"
            ),
            "file:///tmp/out.messages?format=normal|encryption?cipher=aes256gcm&key=***"
        );

        // A partition key names a field to read, not a secret.
        assert_eq!(
            redact_uri("kafka://broker:9092?topic=orders&partition_key=id"),
            "kafka://broker:9092?topic=orders&partition_key=id"
        );

        // Nothing to redact must survive untouched, credential-free URIs included.
        assert_eq!(redact_uri("null:"), "null:");
        assert_eq!(
            redact_uri("nats://host:4222?subject=orders"),
            "nats://host:4222?subject=orders"
        );
    }

    // `buffer` refuses anything between itself and the source, so the filter has
    // to wrap it rather than land underneath it.
    #[test]
    fn the_filter_wraps_buffer_rather_than_splitting_it_from_the_source() {
        let mut input = endpoint(EndpointType::Kafka(KafkaConfig {
            url: "kafka://localhost:9092".to_string(),
            topic: Some("orders".to_string()),
            ..Default::default()
        }));
        input.middlewares.push(Middleware::Buffer(BufferMiddleware {
            max_messages: 100,
            max_delay_ms: 10,
        }));
        configure_filter(&mut input, "amount > 10");

        assert!(matches!(input.middlewares[0], Middleware::Filter(_)));
        assert!(matches!(input.middlewares[1], Middleware::Buffer(_)));
    }

    #[test]
    fn the_filter_is_innermost_without_a_buffer() {
        let mut input = endpoint(EndpointType::Kafka(KafkaConfig {
            url: "kafka://localhost:9092".to_string(),
            topic: Some("orders".to_string()),
            ..Default::default()
        }));
        input
            .middlewares
            .push(Middleware::Metrics(MetricsMiddleware {}));
        configure_filter(&mut input, "amount > 10");

        assert!(matches!(input.middlewares[1], Middleware::Filter(_)));
    }

    #[test]
    fn resume_configures_native_and_cursor_sources() {
        let output = endpoint(EndpointType::Null);
        let mut kafka = endpoint(EndpointType::Kafka(KafkaConfig {
            url: "kafka://localhost:9092".to_string(),
            topic: Some("orders".to_string()),
            ..Default::default()
        }));
        assert_eq!(
            configure_resume(&mut kafka, &output, Some("amount > 10")).unwrap(),
            ResumeCapability::Native
        );
        let EndpointType::Kafka(kafka) = kafka.endpoint_type else {
            unreachable!()
        };
        assert!(kafka.group_id.unwrap().starts_with("copy-"));

        let mut sql = endpoint(EndpointType::Sqlx(SqlxConfig {
            url: "postgres://alice:secret@localhost/app".to_string(),
            table: "orders".to_string(),
            cursor_column: Some("id".to_string()),
            ..Default::default()
        }));
        assert_eq!(
            configure_resume(&mut sql, &output, None).unwrap(),
            ResumeCapability::CursorBased
        );
        let EndpointType::Sqlx(sql) = sql.endpoint_type else {
            unreachable!()
        };
        assert!(sql.cursor_id.unwrap().starts_with("copy-"));
        assert!(sql.checkpoint_store.is_none());
    }

    #[test]
    fn resume_rejects_unsupported_sources_early() {
        let mut input = endpoint(EndpointType::Mqtt(MqttConfig::default()));
        let error = configure_resume(&mut input, &endpoint(EndpointType::Null), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "source `mqtt` does not support resumable copy"
        );
    }

    #[test]
    fn checkpoint_identity_changes_with_semantics_not_passwords() {
        let output = endpoint(EndpointType::Null);
        let source = |password: &str| {
            endpoint(EndpointType::Sqlx(SqlxConfig {
                url: format!("postgres://alice:{password}@localhost/app"),
                table: "orders".to_string(),
                cursor_column: Some("id".to_string()),
                ..Default::default()
            }))
        };
        let first = checkpoint_identity(&source("one"), &output, Some("amount > 10")).unwrap();
        let rotated = checkpoint_identity(&source("two"), &output, Some("amount > 10")).unwrap();
        let changed = checkpoint_identity(&source("two"), &output, Some("amount > 20")).unwrap();
        assert_eq!(first, rotated);
        assert_ne!(first, changed);
    }

    // `partition_key` reads like a credential to a `*_key` rule, but it is
    // routing: two destinations that differ in it are different pipelines and
    // must not share a checkpoint.
    #[test]
    fn key_named_settings_still_separate_pipelines() {
        let input = endpoint(EndpointType::Sqlx(SqlxConfig {
            url: "postgres://alice@localhost/app".to_string(),
            table: "orders".to_string(),
            cursor_column: Some("id".to_string()),
            ..Default::default()
        }));
        let sink = |partition_key: &str| {
            endpoint(EndpointType::Kafka(KafkaConfig {
                url: "kafka://localhost:9092".to_string(),
                topic: Some("orders".to_string()),
                partition_key: Some(partition_key.to_string()),
                ..Default::default()
            }))
        };
        assert_ne!(
            checkpoint_identity(&input, &sink("region"), None).unwrap(),
            checkpoint_identity(&input, &sink("customer"), None).unwrap()
        );
    }

    // An mq-bridge release that adds one optional setting must not silently
    // restart every resumable copy from the beginning.
    #[test]
    fn a_newly_added_absent_setting_keeps_the_identity_stable() {
        let mut definition = json!({
            "source": { "middlewares": [], "sql": { "url": "postgres://localhost/app" } },
            "destination": { "middlewares": [], "null": null },
            "filter": null,
        });
        let mut upgraded = json!({
            "source": {
                "middlewares": [],
                "sql": { "url": "postgres://localhost/app", "a_new_setting": null }
            },
            "destination": { "middlewares": [], "null": null },
            "filter": null,
        });
        strip_absent_fields(&mut definition, 0);
        strip_absent_fields(&mut upgraded, 0);
        assert_eq!(definition, upgraded);
        // The endpoint's own type tag is null-valued and must survive.
        assert!(definition["destination"].get("null").is_some());
    }
}
