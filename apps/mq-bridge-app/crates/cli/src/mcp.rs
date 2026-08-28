//  mq-bridge-app — MCP server mode (`mqb mcp`).
//
//  A lean, protocol-agnostic MCP server built directly on the `mq_bridge`
//  library. Unlike a fixed-config server, every tool takes the source/target
//  endpoint as JSON, so the model can publish to, and run routes between, any of
//  the supported connectors ad hoc.
//
//  Tools: `publish`, `start_route`, `list_routes`, `route_status`, `stop_route`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mq_bridge_app::mq_bridge::{
    CanonicalMessage, Handled, Publisher, Sent, SentBatch,
    models::{Endpoint, EndpointType, Route},
    route::{RouteHandle, RouteOutcome},
};
use mq_bridge_app::route_metrics::{
    CAPTURE_SOURCE_KEY, CAPTURE_TIME_KEY, MessageCapture, RouteMetrics, RouteTiming,
    format_capture_time, is_redelivery,
};
use mq_bridge_app::status_registry::{
    InstanceKind, StatusEntity, StatusLease, StatusRoute, StatusSnapshot, StatusSummary,
};
use mq_bridge_app::ui_app::{ConsumerStatusSnapshot, EndpointStatusSnapshot, RouteOutcomeSnapshot};
use rmcp::schemars;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

/// A single message to publish, mapped onto mq-bridge's `CanonicalMessage`
/// (a `payload`, a `message_id`, and string `metadata` headers).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InputMessage {
    /// The message content — a string (sent verbatim) or any JSON value
    /// (serialized to JSON bytes).
    payload: serde_json::Value,
    /// Optional string headers. Conventional keys: `kind` — the message type,
    /// used by mq-bridge's type-based routing/handlers; `correlation_id` and
    /// `reply_to` — request/reply correlation. Any other user headers are passed
    /// through. Keys starting with `mqb.src.` are reserved (source/provenance) and
    /// are stripped, so do not set them.
    #[serde(default)]
    metadata: HashMap<String, String>,
    /// Optional message id (string, integer, or MongoDB OID object). If omitted,
    /// a time-ordered UUIDv7 is generated.
    #[serde(default)]
    message_id: Option<serde_json::Value>,
}

impl InputMessage {
    fn into_canonical(self) -> Result<CanonicalMessage, McpError> {
        let mut msg = if let Some(s) = self.payload.as_str() {
            CanonicalMessage::from(s)
        } else {
            let bytes = serde_json::to_vec(&self.payload)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            CanonicalMessage::new(bytes, None)
        };
        if let Some(id) = self.message_id {
            // Reuse CanonicalMessage's own id parsing (handles string / int / OID).
            // A malformed id is reported rather than silently replaced by the
            // generated UUID, so the caller's id is never quietly dropped.
            let parsed = CanonicalMessage::from_json(serde_json::json!({ "message_id": id }))
                .map_err(|e| McpError::invalid_params(format!("invalid message_id: {e}"), None))?;
            msg.message_id = parsed.message_id;
        }
        for (k, v) in self.metadata {
            msg.metadata.insert(k, v);
        }
        Ok(msg)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PublishArgs {
    /// Target endpoint, keyed by connector type, e.g.
    /// `{"kafka": {"url": "localhost:9092", "topic": "orders"}}` or
    /// `{"file": {"path": "/tmp/out.jsonl"}}`.
    publisher: Endpoint,
    /// A single message to send.
    #[serde(default)]
    message: Option<InputMessage>,
    /// A batch of messages to send (alternative to `message`).
    #[serde(default)]
    messages: Option<Vec<InputMessage>>,
    /// Optional label for this target, used to group sends in status reporting.
    /// Defaults to the connector type.
    #[serde(default)]
    name: Option<String>,
}

/// The `route` argument, together with which tuning keys the caller actually
/// wrote inside it.
///
/// `RouteOptions::{concurrency,batch_size}` are plain `usize` with serde
/// defaults, so a deserialized `Route` cannot say whether the caller omitted
/// them or set them to the library's default of `1`. Going through the raw JSON
/// preserves that distinction, which is what lets `start_route` fill in the app
/// defaults *only* where the caller said nothing.
#[derive(Debug)]
struct RouteArg {
    route: Route,
    /// `concurrency` as written inside `route`, or `None` if the key was absent.
    concurrency: Option<usize>,
    /// `batch_size` as written inside `route`, or `None` if the key was absent.
    batch_size: Option<usize>,
}

impl<'de> Deserialize<'de> for RouteArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        // A key that is present but malformed reads as `None` here and is
        // reported by `Route`'s own deserializer below, so this never has to
        // raise an error of its own.
        let written = |key: &str| value.get(key).and_then(|v| v.as_u64()).map(|v| v as usize);
        let concurrency = written("concurrency");
        let batch_size = written("batch_size");
        let route = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            route,
            concurrency,
            batch_size,
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StartRouteArgs {
    /// Optional unique route name (auto-generated if omitted).
    #[serde(default)]
    name: Option<String>,
    /// The route to run: an `input` (source) endpoint and an `output` (sink)
    /// endpoint — each keyed by connector type, e.g.
    /// `{"input": {"nats": {"url": "localhost:4222", "subject": "orders"}},
    ///   "output": {"file": {"path": "/tmp/out.jsonl"}}}` — plus route options such
    /// as `exit_on_empty` (drain the source then exit), `concurrency` and
    /// `batch_size`. Use `{"null": null}` as the `output` to discard messages.
    ///
    /// A `file`/`object_store` source must repeat the `compression`/`encryption`
    /// its data was written with — neither is auto-detected, and a mismatch ends
    /// the route as `completed` having moved few or no messages rather than
    /// failing, so check the moved-message count.
    #[schemars(with = "Route")]
    route: RouteArg,
    /// Route concurrency. Overrides `concurrency` inside `route` if both are given;
    /// if neither is given the app default (4) applies, not the library's 1. Higher
    /// values parallelize sink writes; sources that read serially do not speed up.
    #[serde(default)]
    concurrency: Option<usize>,
    /// Batch size. Overrides `batch_size` inside `route` if both are given; if
    /// neither is given the app default (1024) applies, not the library's 512.
    #[serde(default)]
    batch_size: Option<usize>,
    /// Keep the last N messages that flow through this route, readable with
    /// `route_messages`. Omit (or 0) to capture nothing, which is the default —
    /// captured payloads are held in memory and returned to the model verbatim.
    #[serde(default)]
    capture_last: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GenerateCliCommandArgs {
    /// Optional route name in the generated configuration.
    #[serde(default)]
    name: Option<String>,
    /// The same route JSON accepted by `start_route`.
    #[schemars(with = "Route")]
    route: RouteArg,
    /// Route concurrency. Overrides a value inside `route`.
    #[serde(default)]
    concurrency: Option<usize>,
    /// Batch size. Overrides a value inside `route`.
    #[serde(default)]
    batch_size: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RouteNameArg {
    /// The route name.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitRouteArgs {
    /// The route name.
    name: String,
    /// How long to wait before giving up and returning `finished: false`, in
    /// milliseconds. Defaults to 60000; values above 18000000 are clamped. The
    /// route keeps running when the wait times out.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RouteStatusArgs {
    /// A specific route name; omit to report every running route.
    #[serde(default)]
    name: Option<String>,
}

/// The MCP server. Holds the routes it has started so it can report and stop them.
#[derive(Clone)]
pub struct BridgeMcp {
    routes: Arc<Mutex<HashMap<String, RunningRoute>>>,
    /// Names that are being started but have no handle yet. A name is reserved
    /// here before the route runs, so two concurrent starts can never both bring
    /// up a route that consumes the same source. Always locked *while holding
    /// the `routes` lock*, so the two together are a single decision.
    starting: Arc<Mutex<HashSet<String>>>,
    /// Publish targets used so far, so they can be reported to the UI's publisher
    /// bar. Holds names and connector types only.
    publishers: PublisherMap,
    /// Message counters and smoothed throughput for the routes and publishers
    /// above. This is the same machinery the web UI uses for its consumers, so
    /// `messages_per_second` means the same thing on both surfaces.
    metrics: RouteMetrics,
    // Required by the `#[tool_router]` / `#[tool_handler]` macro convention; the
    // generated handler builds the router, so the field itself isn't read directly.
    #[allow(dead_code)]
    tool_router: ToolRouter<BridgeMcp>,
}

/// Capture buffer topic for an MCP-started route.
///
/// Deliberately distinct from the UI's `ui_collector_` prefix: an MCP route is an
/// ad-hoc, unpersisted thing and must never be confused with a configured
/// consumer or publisher.
fn capture_topic(route_name: &str) -> String {
    format!("mcp_collector_{route_name}")
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn ok_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(pretty(&value))])
}

/// A tool result flagged as an error (`is_error = true`) so the client/LLM does
/// not mistake a (partial) failure for success.
fn err_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(pretty(&value))])
}

/// Whether a route's task has ended, or `None` when that cannot be determined.
///
/// `RouteHandle::outcome` is `Some` exactly when the task is done, so its
/// presence is the completion signal; [`route_outcome`] reports *how* it ended.
fn route_finished(handle: &RouteHandle) -> Option<bool> {
    Some(handle.outcome().is_some())
}

/// How a route terminated (`completed` / `stopped` / `failed`), or `None` while
/// it is still running.
///
/// `finished` alone cannot tell a drained `exit_on_empty` job from one that died
/// on a permanent error — both stop the task — so the outcome is reported
/// alongside it. On `failed`, the cause is in the entry's `status.error`.
fn route_outcome(handle: &RouteHandle) -> Option<RouteOutcome> {
    handle.outcome()
}

/// One entry describing a running route: its connection health plus the live
/// message counters the web UI reports for its own consumers.
///
/// `finished` says whether the route's task has ended; `outcome` says how
/// (`completed` / `stopped` / `failed`, `null` while running). A route started
/// with `exit_on_empty` reports `finished: true, outcome: "completed"` once it
/// has drained.
///
/// Two rates are reported, because they answer different questions.
/// `messages_per_second` is instantaneous and decays to ~0 within seconds of a
/// drain finishing, so it only describes a route that is running *now*.
/// `average_messages_per_second` over `elapsed_s` is the rate the route actually
/// achieved, and it stays put once the route goes idle — that is the one to quote
/// for a completed job. Both are absent until the sampler has observed the route.
fn route_entry_json(
    name: &str,
    handle: &RouteHandle,
    messages: u64,
    messages_per_second: f64,
    timing: Option<RouteTiming>,
) -> serde_json::Value {
    let (elapsed_s, average_messages_per_second) = elapsed_and_average(messages, timing);
    serde_json::json!({
        "name": name,
        "source": "mcp",
        "status": serde_json::to_value(handle.status()).unwrap_or_default(),
        "finished": route_finished(handle),
        "outcome": route_outcome(handle),
        "messages": messages,
        "messages_per_second": round2(messages_per_second),
        "elapsed_s": elapsed_s,
        // Averaged over the authoritative counter rather than `t.average_throughput`,
        // whose numerator is the sampler's own total. A route that finishes between
        // two sampler ticks leaves that total at 0 while `elapsed` is already set,
        // which reported 0.0 rows/s for a job that had just moved 100.
        "average_messages_per_second": average_messages_per_second,
    })
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn elapsed_and_average(messages: u64, timing: Option<RouteTiming>) -> (Option<f64>, Option<f64>) {
    let elapsed_s = timing.map(|timing| round2(timing.elapsed.as_secs_f64()));
    let average_messages_per_second = timing.map(|timing| {
        let seconds = timing.elapsed.as_secs_f64();
        round2(if seconds > 0.0 {
            messages as f64 / seconds
        } else {
            0.0
        })
    });
    (elapsed_s, average_messages_per_second)
}

/// Renders one captured message in the same shape the web UI's `/messages`
/// endpoint returns, so both surfaces describe a message identically.
fn captured_message_json(m: CanonicalMessage) -> serde_json::Value {
    let mut metadata = m.metadata.clone();
    metadata.remove(CAPTURE_SOURCE_KEY);
    let time = format_capture_time(metadata.remove(CAPTURE_TIME_KEY).as_deref());

    // Structured payloads are returned as JSON; anything else as a string.
    let payload: serde_json::Value = serde_json::from_slice(&m.payload)
        .unwrap_or_else(|_| serde_json::Value::String(m.get_payload_str().to_string()));

    serde_json::json!({
        "time": time,
        "metadata": metadata,
        "payload": payload,
    })
}

fn invalid(msg: impl Into<String>) -> McpError {
    McpError::invalid_params(msg.into(), None)
}

fn internal(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

fn describe_sent(sent: Sent) -> serde_json::Value {
    match sent {
        Sent::Ack => serde_json::json!({ "status": "Ack" }),
        Sent::Response(msg) => serde_json::json!({
            "status": "Response",
            "payload": msg.get_payload_str(),
            "metadata": msg.metadata,
        }),
    }
}

/// Describes a batch send outcome. Returns the JSON summary and whether any
/// message failed (so the caller can flag the tool result as an error).
fn describe_sent_batch(sent: SentBatch, count: usize) -> (serde_json::Value, bool) {
    match sent {
        SentBatch::Ack => (serde_json::json!({ "status": "Ack", "sent": count }), false),
        SentBatch::Partial { responses, failed } => {
            let failed_count = failed.len();
            let sent_ok = count.saturating_sub(failed_count);
            let value = serde_json::json!({
                "status": if failed_count == 0 { "Ack" } else { "Partial" },
                "sent": sent_ok,
                "failed": failed_count,
                "errors": failed.iter().map(|(_, e)| e.to_string()).collect::<Vec<_>>(),
                "responses": responses.map(|r| {
                    r.iter().map(|m| m.get_payload_str().to_string()).collect::<Vec<_>>()
                }),
            });
            (value, failed_count > 0)
        }
    }
}

/// A route started via this server. The connector types of its endpoints are
/// kept alongside the handle so status can label the rows it contributes; only
/// the type labels are kept, never the endpoint config.
pub(crate) struct RunningRoute {
    handle: RouteHandle,
    input: String,
    output: String,
}

/// Shared map of routes started via this server, keyed by route name.
type RouteMap = Arc<Mutex<HashMap<String, RunningRoute>>>;

/// Publish targets used via this server, keyed by name, holding the connector
/// type. Only the type is kept — never the endpoint config, which carries URLs
/// and credentials.
type PublisherMap = Arc<Mutex<HashMap<String, String>>>;

#[derive(Default)]
struct McpStatusSnapshot {
    routes: HashMap<String, McpRouteStatus>,
    publishers: HashMap<String, ConsumerStatusSnapshot>,
}

struct McpRouteStatus {
    snapshot: ConsumerStatusSnapshot,
    input: String,
    output: String,
}

/// The connector type of an endpoint, e.g. `"kafka"`, with no config attached.
///
/// `EndpointType` serializes as a single-key object keyed by connector name, so
/// the key alone is the label. Anything unexpected degrades to `"unknown"`
/// rather than risking a serialized config reaching the UI.
fn endpoint_type_label(endpoint_type: &EndpointType) -> String {
    match serde_json::to_value(endpoint_type) {
        Ok(serde_json::Value::Object(map)) => map
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()),
        Ok(serde_json::Value::String(s)) => s,
        _ => "unknown".to_string(),
    }
}

/// Builds the status snapshot reported to the UI for one route or publisher.
///
/// Deliberately constructs [`EndpointStatusSnapshot`] field by field instead of
/// converting from `EndpointStatus`: `target` is replaced with the local name and
/// `details` is dropped, because both are free-form and could otherwise carry a
/// connection string to the UI. `error` is kept — it is what makes a failing
/// route diagnosable — so it must stay a message, never a rendered config.
///
/// `capture_*` are always reported as off: the capture buffer lives in this
/// process's memory and the UI cannot read it, so advertising it would promise
/// messages the UI can never fetch.
fn report_snapshot(
    name: &str,
    healthy: bool,
    error: Option<String>,
    running: bool,
    outcome: Option<RouteOutcomeSnapshot>,
    messages: u64,
    throughput: f64,
) -> ConsumerStatusSnapshot {
    ConsumerStatusSnapshot {
        running,
        status: EndpointStatusSnapshot {
            healthy,
            target: name.to_string(),
            pending: None,
            capacity: None,
            error,
            details: serde_json::Value::Null,
        },
        throughput,
        message_sequence: messages,
        capture_enabled: false,
        capture_keep_last: 0,
        outcome,
    }
}

#[tool_router]
impl BridgeMcp {
    pub fn new() -> Self {
        Self::with_routes(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Builds a server that shares an existing route map. Used by the HTTP
    /// transport so every session sees (and can stop) the same routes.
    pub fn with_routes(routes: RouteMap) -> Self {
        let metrics = RouteMetrics::new();
        // Start sampling immediately so a route's rate is available as soon as it
        // runs, rather than only after the first status call.
        metrics.ensure_updater();
        Self {
            routes,
            starting: Arc::new(Mutex::new(HashSet::new())),
            publishers: Arc::new(Mutex::new(HashMap::new())),
            metrics,
            tool_router: Self::tool_router(),
        }
    }

    /// Metric keys are shared between routes and publishers, so publishers are
    /// namespaced to keep a publisher from colliding with a same-named route.
    fn publisher_metric_key(name: &str) -> String {
        format!("publisher:{name}")
    }

    /// The current status of every route and publisher, in the shape the UI
    /// expects. Used by the reporting task.
    async fn status_report(&self) -> McpStatusSnapshot {
        let mut routes = HashMap::new();
        for (name, route) in self.routes.lock().await.iter() {
            let handle = &route.handle;
            let status = handle.status();
            routes.insert(
                name.clone(),
                McpRouteStatus {
                    snapshot: report_snapshot(
                        name,
                        status.healthy,
                        status.error.clone(),
                        // Unknown completion reads as "still running": a route we
                        // still hold a handle for was never stopped from here.
                        !route_finished(handle).unwrap_or(false),
                        route_outcome(handle).map(RouteOutcomeSnapshot::from),
                        self.metrics.sequence(name).await,
                        self.metrics.throughput(name).await,
                    ),
                    input: route.input.clone(),
                    output: route.output.clone(),
                },
            );
        }

        let mut publishers = HashMap::new();
        for (name, endpoint_type) in self.publishers.lock().await.iter() {
            let key = Self::publisher_metric_key(name);
            publishers.insert(
                name.clone(),
                report_snapshot(
                    endpoint_type,
                    true,
                    None,
                    true,
                    // Publishers have no route task, so nothing ever ends.
                    None,
                    self.metrics.sequence(&key).await,
                    self.metrics.throughput(&key).await,
                ),
            );
        }

        McpStatusSnapshot { routes, publishers }
    }

    /// The same status in registry terms: each route contributes a consumer row
    /// for its input and a publisher row for its output.
    async fn status_snapshot(&self) -> StatusSnapshot {
        let report = self.status_report().await;
        let publisher_endpoints = self.publishers.lock().await.clone();
        let mut consumers: Vec<StatusEntity> = report
            .routes
            .iter()
            .map(|(name, route)| status_entity(name, &route.input, &route.snapshot))
            .collect();
        let mut publishers: Vec<StatusEntity> = report
            .publishers
            .iter()
            .filter_map(|(name, snapshot)| {
                publisher_endpoints
                    .get(name)
                    .map(|endpoint| status_entity(name, endpoint, snapshot))
            })
            .collect();
        let mut routes: Vec<StatusRoute> = report
            .routes
            .iter()
            .map(|(name, route)| StatusRoute {
                id: name.clone(),
                label: name.clone(),
                input: status_entity(&format!("{name}:input"), &route.input, &route.snapshot),
                output: status_entity(&format!("{name}:output"), &route.output, &route.snapshot),
                summary: StatusSummary::from(&route.snapshot),
            })
            .collect();
        // The maps above iterate in hash order, so sort: a peer's rows must not
        // reshuffle on every heartbeat.
        consumers.sort_by(|left, right| left.id.cmp(&right.id));
        publishers.sort_by(|left, right| left.id.cmp(&right.id));
        routes.sort_by(|left, right| left.id.cmp(&right.id));
        StatusSnapshot {
            consumers,
            publishers,
            routes,
        }
    }

    #[tool(
        description = "Publish one message or a batch of messages to any endpoint. \
            The target is supplied inline as endpoint JSON keyed by type (e.g. \
            {\"kafka\": {\"url\": \"...\", \"topic\": \"...\"}}); this is independent of routes. \
            Provide either `message` or `messages`; each message has a `payload` and optional \
            `metadata` headers (e.g. `kind` for the message type, `correlation_id`). Returns the \
            send outcome (an acknowledgement, or the endpoint's reply for request/reply endpoints)."
    )]
    async fn publish(
        &self,
        Parameters(args): Parameters<PublishArgs>,
    ) -> Result<CallToolResult, McpError> {
        let endpoint_type = endpoint_type_label(&args.publisher.endpoint_type);
        let target_name = args
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| endpoint_type.clone());

        let publisher = Publisher::new(args.publisher)
            .await
            .map_err(|e| invalid(format!("invalid publisher endpoint: {e}")))?;

        // Register the target and count what goes through it, so the UI's
        // publisher bar can show MCP publish activity the same way it shows
        // consumer throughput.
        self.publishers
            .lock()
            .await
            .entry(target_name.clone())
            .or_insert(endpoint_type);
        let counter = self
            .metrics
            .counter_for(&Self::publisher_metric_key(&target_name))
            .await;

        match (args.message, args.messages) {
            (None, None) => Err(invalid("provide either `message` or `messages`")),
            (Some(_), Some(_)) => Err(invalid("provide only one of `message` or `messages`")),
            (Some(message), None) => {
                let canonical = message.into_canonical()?;
                let sent = publisher
                    .send(canonical)
                    .await
                    .map_err(|e| internal(format!("publish failed: {e}")))?;
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(ok_json(describe_sent(sent)))
            }
            (None, Some(messages)) => {
                if messages.is_empty() {
                    return Err(invalid("`messages` must not be empty"));
                }
                let canonical: Vec<CanonicalMessage> = messages
                    .into_iter()
                    .map(InputMessage::into_canonical)
                    .collect::<Result<_, _>>()?;
                let count = canonical.len();
                let sent = publisher
                    .send_batch(canonical)
                    .await
                    .map_err(|e| internal(format!("batch publish failed: {e}")))?;
                let (summary, had_failures) = describe_sent_batch(sent, count);
                // Count what actually left, not what was offered.
                let delivered = summary
                    .get("sent")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(count as u64);
                counter.fetch_add(delivered, Ordering::Relaxed);
                Ok(if had_failures {
                    err_json(summary)
                } else {
                    ok_json(summary)
                })
            }
        }
    }

    #[tool(
        description = "Generate a copyable, headless `mqb` command for a route. Accepts the same \
            route JSON and tuning overrides as `start_route`, but does not start anything. The \
            command embeds canonical JSON with POSIX shell quoting so fields that have no compact \
            copy-URI representation are preserved. Credential values are replaced by environment \
            variable placeholders and the required variable names are returned separately.",
        annotations(read_only_hint = true)
    )]
    async fn generate_cli_command(
        &self,
        Parameters(args): Parameters<GenerateCliCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let name = args
            .name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "route".to_string());
        let RouteArg {
            mut route,
            concurrency: in_route_concurrency,
            batch_size: in_route_batch_size,
        } = args.route;
        route.options.concurrency = args
            .concurrency
            .or(in_route_concurrency)
            .unwrap_or(crate::DEFAULT_CONCURRENCY);
        route.options.batch_size = args
            .batch_size
            .or(in_route_batch_size)
            .unwrap_or(crate::DEFAULT_BATCH_SIZE);

        let mut route = serde_json::to_value(route)
            .map_err(|error| internal(format!("failed to serialize route: {error}")))?;
        route
            .as_object_mut()
            .ok_or_else(|| internal("serialized route is not an object"))?
            .insert("enabled".to_string(), serde_json::Value::Bool(true));
        let config: mq_bridge_app::config::AppConfig =
            serde_json::from_value(serde_json::json!({ "routes": { (name): route } }))
                .map_err(|error| internal(format!("failed to build route config: {error}")))?;
        let export = mq_bridge_app::cli_command::inline_config_command(&config)
            .map_err(|error| internal(error.to_string()))?;
        Ok(ok_json(serde_json::json!({
            "command": export.command,
            "required_env": export.required_env,
        })))
    }

    #[tool(
        description = "Create and run a route that moves messages from its `input` (source) \
            endpoint to its `output` (sink) endpoint, each keyed by connector type. Supports \
            route options like `concurrency`, `batch_size`, and `exit_on_empty` (drain the source \
            then exit; otherwise the route runs continuously until stopped). Either endpoint may \
            carry a `middlewares` array — `retry`, `dlq`, `deduplication`, `limiter`, `transform`, \
            `compression`, `encryption`, `buffer`, `delay`, `weak_join`, `cookie_jar`, `metrics` — \
            e.g. `{\"sqlx\": {...}, \"middlewares\": [{\"retry\": {\"max_attempts\": 3}}]}`. \
            `concurrency`/`batch_size` may be given inside `route` or as top-level arguments \
            — the top-level argument wins, and an unset value becomes the app default \
            (4 / 1024). Returns the route name for use with wait_route / route_status / \
            stop_route, plus the tuning that took effect.",
        annotations(destructive_hint = true, read_only_hint = false)
    )]
    async fn start_route(
        &self,
        Parameters(args): Parameters<StartRouteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let name = args
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(auto_route_name);
        let capture_last = args.capture_last.unwrap_or(0);

        // Both spellings are accepted, so resolve them in one place, most
        // specific first: the dedicated argument, else the same key written
        // inside `route`, else the app default. Only a value the caller never
        // wrote is replaced — an explicit `1` in either position survives, which
        // it could not if these were read off the deserialized `RouteOptions`
        // (see `RouteArg`).
        let RouteArg {
            mut route,
            concurrency: in_route_concurrency,
            batch_size: in_route_batch_size,
        } = args.route;
        let concurrency = args
            .concurrency
            .or(in_route_concurrency)
            .unwrap_or(crate::DEFAULT_CONCURRENCY);
        let batch_size = args
            .batch_size
            .or(in_route_batch_size)
            .unwrap_or(crate::DEFAULT_BATCH_SIZE);
        route.options.concurrency = concurrency;
        route.options.batch_size = batch_size;
        let exit_on_empty = route.options.exit_on_empty;

        // Claim the name before the potentially slow `run()`, then release the
        // locks so other route tools aren't blocked on startup. Reserving up
        // front means a concurrent start for the same name is rejected before it
        // can bring up a second route against the same source.
        {
            let routes = self.routes.lock().await;
            let mut starting = self.starting.lock().await;
            if routes.contains_key(&name) || !starting.insert(name.clone()) {
                return Err(invalid(format!("a route named '{name}' already exists")));
            }
        }

        // Wrap the route in the same counting/capturing handler the web UI puts on
        // its consumer routes, so `route_status` can report a live message rate.
        // The handler forwards every message unchanged unless the sink is `null`,
        // and counts each one once — a `retry` redelivery re-enters the handler
        // but is not a new message.
        let publishes = !matches!(route.output.endpoint_type, EndpointType::Null);
        let input_label = endpoint_type_label(&route.input.endpoint_type);
        let output_label = endpoint_type_label(&route.output.endpoint_type);
        let counter = self.metrics.counter_for(&name).await;
        let capture = (capture_last > 0)
            .then(|| MessageCapture::with_capacity(&capture_topic(&name), capture_last));
        let source_key = name.clone();

        let route = route.with_handler(move |msg: CanonicalMessage| {
            let counter = Arc::clone(&counter);
            let capture = capture.clone();
            let source_key = source_key.clone();
            async move {
                if !is_redelivery(&msg) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(capture) = capture {
                    capture.push(&source_key, msg.clone());
                }
                Ok(if publishes {
                    Handled::Publish(msg)
                } else {
                    Handled::Ack
                })
            }
        });

        let handle = match route.run(&name).await {
            Ok(handle) => handle,
            Err(e) => {
                // Startup failed, so release the name for a later retry — and with
                // it the counter created above. Leaving it behind would keep the
                // sampler working on a key no route will ever feed, and a retry
                // under the same name would inherit the failed attempt's start
                // time, so its `elapsed_s` and average rate would cover the
                // startup that never happened. Any capture buffer is dropped for
                // the same reason: it outlives the route in a process-global
                // registry keyed by topic.
                self.metrics.forget(&name).await;
                if capture_last > 0 {
                    MessageCapture::open(&capture_topic(&name)).drain();
                }
                // Release the name last: while it is held, no concurrent `start_route`
                // can create a capture buffer for this topic that the drain above
                // would then throw away.
                self.starting.lock().await.remove(&name);
                return Err(internal(format!("failed to start route '{name}': {e}")));
            }
        };

        // Replace the reservation with the running handle under both locks, so
        // the name is never momentarily free while the route is live.
        let mut routes = self.routes.lock().await;
        self.starting.lock().await.remove(&name);
        routes.insert(
            name.clone(),
            RunningRoute {
                handle,
                input: input_label,
                output: output_label,
            },
        );

        Ok(ok_json(serde_json::json!({
            "route": name,
            "exit_on_empty": exit_on_empty,
            "capture_last": capture_last,
            // Echo the resolved tuning, so the caller sees what actually took
            // effect rather than inferring it from the precedence rules.
            "concurrency": concurrency,
            "batch_size": batch_size,
        })))
    }

    #[tool(
        description = "List the routes started by this server, with their live connection health, \
            total messages moved, current message rate, and achieved average rate.",
        annotations(read_only_hint = true)
    )]
    async fn list_routes(&self) -> Result<CallToolResult, McpError> {
        Ok(ok_json(serde_json::Value::Array(
            self.all_routes_json().await,
        )))
    }

    #[tool(
        description = "Report this MCP server's build identity: crate `version`, `git_hash` (with a \
            `-dirty` suffix if the working tree had uncommitted changes at build time), build \
            `profile` (debug/release), and `build_time`. Use it to confirm which binary is actually \
            running before trusting throughput numbers.",
        annotations(read_only_hint = true)
    )]
    async fn server_info(&self) -> Result<CallToolResult, McpError> {
        Ok(ok_json(serde_json::json!({
            "name": "mq-bridge-app",
            "version": env!("CARGO_PKG_VERSION"),
            "git_hash": option_env!("MQB_GIT_HASH").unwrap_or("unknown"),
            "profile": option_env!("MQB_BUILD_PROFILE").unwrap_or("unknown"),
            "build_time": option_env!("MQB_BUILD_TIME").unwrap_or("unknown"),
        })))
    }

    #[tool(
        description = "Report the live status of one route (by `name`) or all routes: connection \
            health, whether it has finished, the total number of messages it has moved, its current \
            rate (`messages_per_second`), and the rate it achieved overall \
            (`average_messages_per_second` over `elapsed_s`). Quote the average for a finished job: \
            the current rate decays to ~0 once a route stops moving data.",
        annotations(read_only_hint = true)
    )]
    async fn route_status(
        &self,
        Parameters(args): Parameters<RouteStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        match args.name {
            Some(name) => {
                let routes = self.routes.lock().await;
                let Some(route) = routes.get(&name) else {
                    return Err(invalid(format!("no route named '{name}'")));
                };
                let entry = route_entry_json(
                    &name,
                    &route.handle,
                    self.metrics.sequence(&name).await,
                    self.metrics.throughput(&name).await,
                    self.metrics.timing(&name).await,
                );
                Ok(ok_json(entry))
            }
            None => Ok(ok_json(serde_json::Value::Array(
                self.all_routes_json().await,
            ))),
        }
    }

    #[tool(
        description = "Block until a route finishes, then report how it ended. Use this instead of \
            polling `route_status` for a job started with `exit_on_empty`: it costs one tool call \
            regardless of how long the job runs. Returns `finished` (false if `timeout_ms` elapsed \
            first — the route is left running), the `outcome`, the total messages moved and the \
            rate achieved. An `outcome` of `failed` carries the cause in `error`. Default timeout \
            60000 ms, maximum 18000000 ms.",
        annotations(read_only_hint = true)
    )]
    async fn wait_route(
        &self,
        Parameters(args): Parameters<WaitRouteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
                .min(MAX_WAIT_TIMEOUT_MS),
        );

        // The engine exposes completion as a poll (`RouteHandle::outcome`), not a
        // notification, so this polls too — but server-side, at a tick the client
        // never pays for. The route lock is held only for the check, so
        // `stop_route` and the other tools stay responsive while a wait is parked.
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        // The route's error is read under the same lock as its outcome: a bare
        // `outcome: "failed"` says nothing about *why*, and the caller would have
        // to follow up with `route_status` to find out. The CLI already prints
        // the cause on a permanent error, so dropping it here made the two
        // interfaces disagree about the same failure.
        let (outcome, error) = loop {
            {
                let routes = self.routes.lock().await;
                let Some(route) = routes.get(&args.name) else {
                    return Err(invalid(format!("no route named '{}'", args.name)));
                };
                if let Some(outcome) = route.handle.outcome() {
                    break (Some(outcome), route.handle.status().error);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break (None, None);
            }
            tokio::time::sleep(WAIT_POLL_INTERVAL.min(deadline - tokio::time::Instant::now()))
                .await;
        };

        let messages = self.metrics.sequence(&args.name).await;
        let timing = self.metrics.timing(&args.name).await;
        let (elapsed_s, average_messages_per_second) = elapsed_and_average(messages, timing);
        Ok(ok_json(serde_json::json!({
            "route": args.name,
            "finished": outcome.is_some(),
            "outcome": outcome,
            "error": error,
            "waited_ms": started.elapsed().as_millis() as u64,
            "messages": messages,
            "elapsed_s": elapsed_s,
            "average_messages_per_second": average_messages_per_second,
        })))
    }

    #[tool(
        description = "Return the most recent messages captured on a route, newest last. Only \
            routes started with `capture_last` capture anything. Reads are destructive: each \
            message is returned once, so repeated calls yield only what arrived since the last \
            call.",
        // Not read-only: draining consumes the buffer, so a client must not
        // auto-approve this the way it may auto-approve a true reader.
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn route_messages(
        &self,
        Parameters(args): Parameters<RouteNameArg>,
    ) -> Result<CallToolResult, McpError> {
        if !self.routes.lock().await.contains_key(&args.name) {
            return Err(invalid(format!("no route named '{}'", args.name)));
        }

        let messages: Vec<serde_json::Value> = MessageCapture::open(&capture_topic(&args.name))
            .drain()
            .into_iter()
            .map(captured_message_json)
            .collect();

        Ok(ok_json(serde_json::json!({
            "route": args.name,
            "count": messages.len(),
            "messages": messages,
        })))
    }

    #[tool(
        description = "Stop a running route by `name`. Returns the total messages it moved plus the \
            rate it achieved (`average_messages_per_second` over `elapsed_s`)."
    )]
    async fn stop_route(
        &self,
        Parameters(args): Parameters<RouteNameArg>,
    ) -> Result<CallToolResult, McpError> {
        let route = self.routes.lock().await.remove(&args.name);
        match route {
            Some(route) => {
                route.handle.stop().await;
                let messages = self.metrics.sequence(&args.name).await;
                // Read the timing before forgetting the route: this is the last
                // chance to report what it achieved.
                let timing = self.metrics.timing(&args.name).await;
                let (elapsed_s, average_messages_per_second) =
                    elapsed_and_average(messages, timing);
                self.metrics.forget(&args.name).await;
                // Capture buffers live in a process-global registry keyed by
                // topic and outlive the route, so anything still buffered would
                // otherwise resurface as the first `route_messages` result of the
                // next route that reuses this name.
                let dropped = MessageCapture::open(&capture_topic(&args.name))
                    .drain()
                    .len();
                Ok(ok_json(serde_json::json!({
                    "stopped": args.name,
                    "messages": messages,
                    "elapsed_s": elapsed_s,
                    "average_messages_per_second": average_messages_per_second,
                    "discarded_captured_messages": dropped,
                })))
            }
            None => Err(invalid(format!("no route named '{}'", args.name))),
        }
    }

    /// Every known route with its health and live counters, sorted by name so
    /// repeated calls are stable.
    async fn all_routes_json(&self) -> Vec<serde_json::Value> {
        let routes = self.routes.lock().await;
        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(routes.len());
        for (name, route) in routes.iter() {
            entries.push(route_entry_json(
                name,
                &route.handle,
                self.metrics.sequence(name).await,
                self.metrics.throughput(name).await,
                self.metrics.timing(name).await,
            ));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        entries
    }
}

impl Default for BridgeMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_handler]
impl ServerHandler for BridgeMcp {
    fn get_info(&self) -> ServerInfo {
        // `Implementation` is `#[non_exhaustive]`, so build via Default then set fields.
        let mut server_info = Implementation::default();
        server_info.name = "mq-bridge-app".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(server_info)
        .with_instructions(
            "mq-bridge: a universal, protocol-agnostic message and data bridge. Move data between \
             any of the supported endpoints (postgres, kafka, nats, mqtt, mongodb, redis, ibm-mq, \
             http, files, and more) ad hoc. `publish` and `start_route` take the endpoint(s) \
             inline as JSON keyed by type, e.g. {\"kafka\": {\"url\": \"...\", \"topic\": \
             \"...\"}}. Use `publish` to \
             send messages to a target; `start_route` to move messages from a source (`input`) to \
             a sink (`output`), optionally setting `exit_on_empty` to drain-then-exit; and \
             `list_routes` / `route_status` / `stop_route` to manage running routes. Prefer \
             `wait_route` over polling `route_status` for a drain-then-exit job: it is one call \
             however long the job runs. Either endpoint may carry a `middlewares` array (retry, \
             dlq, deduplication, limiter, transform, compression, encryption, ...) to add delivery \
             behaviour without changing the route. Apache Pulsar is available too, but is not one \
             of the endpoint variants in the tool schema: address it as {\"custom\": {\"name\": \
             \"pulsar\", \"config\": {\"url\": \"pulsar://host:6650\", \"topic\": \"...\", \
             \"subscription\": \"...\", \"initial_position\": \"earliest\"}}}. A source needs \
             `initial_position: earliest` to read a topic's existing backlog; the default \
             (`latest`) only sees messages published after the subscription is created.",
        )
    }
}

/// A process-unique name for an auto-named route. A monotonic counter avoids the
/// collisions a timestamp can produce for near-simultaneous starts.
fn auto_route_name() -> String {
    static ROUTE_SEQ: AtomicU64 = AtomicU64::new(1);
    format!("mcp-route-{}", ROUTE_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Default and ceiling for `wait_route`'s timeout. The ceiling keeps a client's
/// own request timeout, not the server, from being what ends a long wait.
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 60_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 18_000_000;

/// How often `wait_route` re-checks a route for completion. Matches the poll the
/// bench client used: fine enough not to distort a sub-second job, coarse enough
/// not to perturb the run it is observing.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Entry point for the `mcp` subcommand.
///
/// Status publication is on by default. The legacy flag is retained as a
/// disable switch for clients that need a quiet process.
pub async fn run(
    transport: String,
    bind: Option<String>,
    report_to_ui: bool,
    workspace_path: String,
) -> anyhow::Result<()> {
    let server = BridgeMcp::new();
    // Held for the lifetime of the transport; dropping it removes the lease,
    // including on the `bail!` below.
    let _lease = report_to_ui
        .then(|| {
            let server = server.clone();
            StatusLease::spawn(
                InstanceKind::Mcp,
                env!("CARGO_PKG_VERSION"),
                &workspace_path,
                move || {
                    let server = server.clone();
                    async move { server.status_snapshot().await }
                },
            )
        })
        .flatten();

    match transport.as_str() {
        "stdio" => run_stdio(server).await,
        "http" => run_http(server, bind.unwrap_or_else(|| "127.0.0.1:9092".to_string())).await,
        other => anyhow::bail!("unknown --transport '{other}' (expected 'stdio' or 'http')"),
    }
}

/// `endpoint` is the connector type, never a name: `sanitize_endpoint` would let
/// a route name through as if it were one. Callers with no type pass `"unknown"`.
fn status_entity(name: &str, endpoint: &str, snapshot: &ConsumerStatusSnapshot) -> StatusEntity {
    StatusEntity {
        id: name.to_string(),
        label: name.to_string(),
        endpoint: endpoint.to_string(),
        summary: StatusSummary::from(snapshot),
    }
}

async fn run_stdio(server: BridgeMcp) -> anyhow::Result<()> {
    info!("MCP server starting on stdio");
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn run_http(server: BridgeMcp, bind: String) -> anyhow::Result<()> {
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
        service::TowerToHyperService,
    };
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };

    // One shared server across all sessions, so routes, publishers and metrics
    // started in any session are visible to (and stoppable from) every session,
    // and the reporting task sees all of them.
    let service = TowerToHyperService::new(StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        Default::default(),
    ));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("MCP server listening on http://{bind}/ (streamable HTTP)");

    loop {
        let io = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            accept = listener.accept() => match accept {
                Ok((stream, _)) => TokioIo::new(stream),
                // A transient accept error must not tear down the whole server.
                Err(e) => {
                    tracing::warn!("MCP http accept error: {e}");
                    continue;
                }
            },
        };
        let service = service.clone();
        tokio::spawn(async move {
            let _ = Builder::new(TokioExecutor::default())
                .serve_connection(io, service)
                .await;
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY};

    fn parse(route: serde_json::Value) -> RouteArg {
        serde_json::from_value(route).expect("route parses")
    }

    #[test]
    fn elapsed_and_average_uses_the_authoritative_message_count() {
        let timing = RouteTiming {
            messages: 0,
            elapsed: Duration::from_secs_f64(2.005),
            average_throughput: 0.0,
        };

        assert_eq!(
            elapsed_and_average(5, Some(timing)),
            (Some(2.01), Some(2.49))
        );
        assert_eq!(
            elapsed_and_average(
                5,
                Some(RouteTiming {
                    elapsed: Duration::ZERO,
                    ..timing
                })
            ),
            (Some(0.0), Some(0.0))
        );
    }

    /// The two tuning knobs may be written inside `route` or as a top-level
    /// `start_route` argument. Presence, not value, decides: an explicit `1`
    /// must survive even though it equals the library's own default.
    #[test]
    fn route_arg_records_only_the_tuning_keys_that_were_written() {
        let bare = parse(serde_json::json!({ "input": { "null": null } }));
        assert_eq!(bare.concurrency, None);
        assert_eq!(bare.batch_size, None);

        let tuned = parse(serde_json::json!({
            "input": { "null": null },
            "concurrency": 1,
            "batch_size": 8,
        }));
        assert_eq!(tuned.concurrency, Some(1));
        assert_eq!(tuned.batch_size, Some(8));
        // The library fills its own defaults in, which is why presence has to be
        // read off the raw JSON rather than off `RouteOptions`.
        assert_eq!(bare.route.options.concurrency, 1);
    }

    #[test]
    fn tuning_precedence_is_argument_then_route_then_app_default() {
        let resolve = |arg: Option<usize>, in_route: Option<usize>, default: usize| {
            arg.or(in_route).unwrap_or(default)
        };
        assert_eq!(resolve(Some(8), Some(2), DEFAULT_CONCURRENCY), 8);
        assert_eq!(resolve(None, Some(2), DEFAULT_CONCURRENCY), 2);
        assert_eq!(resolve(None, Some(1), DEFAULT_CONCURRENCY), 1);
        assert_eq!(
            resolve(None, None, DEFAULT_CONCURRENCY),
            DEFAULT_CONCURRENCY
        );
        assert_eq!(resolve(None, None, DEFAULT_BATCH_SIZE), DEFAULT_BATCH_SIZE);
    }
}

/// Exercises the MCP tools the way a client does: real endpoints, real routes,
/// results read back out of the returned `CallToolResult` JSON.
#[cfg(test)]
mod tool_tests {
    use super::*;
    use serde_json::json;

    fn params<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Parameters<T> {
        Parameters(serde_json::from_value(value).expect("tool arguments parse"))
    }

    /// The JSON a tool returned. Every tool answers with one pretty-printed text block.
    fn result_json(result: &CallToolResult) -> serde_json::Value {
        let text = result
            .content
            .first()
            .and_then(|block| block.as_text())
            .expect("a text content block");
        serde_json::from_str(&text.text).expect("the tool result is JSON")
    }

    /// Memory topics live in a process-global registry, so every test needs its own.
    fn topic(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    fn memory_endpoint(topic: &str) -> serde_json::Value {
        json!({ "memory": { "topic": topic, "capacity": 1000 } })
    }

    async fn publish_n(server: &BridgeMcp, topic: &str, count: usize) {
        let messages: Vec<serde_json::Value> = (0..count)
            .map(|i| json!({ "payload": { "n": i }, "metadata": { "kind": "test" } }))
            .collect();
        let result = server
            .publish(params(json!({
                "publisher": memory_endpoint(topic),
                "messages": messages,
            })))
            .await
            .expect("publish succeeds");
        assert_ne!(result.is_error, Some(true), "{:?}", result.content);
    }

    /// Waits for a route to finish, returning `wait_route`'s report.
    async fn wait_for(server: &BridgeMcp, name: &str) -> serde_json::Value {
        let result = server
            .wait_route(params(json!({ "name": name, "timeout_ms": 10_000 })))
            .await
            .expect("wait_route succeeds");
        result_json(&result)
    }

    /// The whole client-visible lifecycle of a drain job: publish into a source,
    /// route it to a sink with capture on, wait for it, read the captured
    /// messages, then stop it.
    #[tokio::test]
    async fn a_captured_drain_route_reports_its_messages_and_outcome() {
        let server = BridgeMcp::new();
        let source = topic("mcp_src");
        let sink = topic("mcp_sink");
        publish_n(&server, &source, 3).await;

        let started = result_json(
            &server
                .start_route(params(json!({
                    "name": "drain-lifecycle",
                    "route": {
                        "input": memory_endpoint(&source),
                        "output": memory_endpoint(&sink),
                        "exit_on_empty": true,
                    },
                    "capture_last": 10,
                })))
                .await
                .expect("start_route succeeds"),
        );
        assert_eq!(started["route"], "drain-lifecycle");
        assert_eq!(started["exit_on_empty"], true);
        // Neither knob was written, so the app defaults apply, not the library's.
        assert_eq!(started["concurrency"], crate::DEFAULT_CONCURRENCY);
        assert_eq!(started["batch_size"], crate::DEFAULT_BATCH_SIZE);

        let waited = wait_for(&server, "drain-lifecycle").await;
        assert_eq!(waited["finished"], true, "{waited}");
        assert_eq!(waited["outcome"], "completed", "{waited}");
        assert_eq!(waited["messages"], 3, "{waited}");

        let captured = result_json(
            &server
                .route_messages(params(json!({ "name": "drain-lifecycle" })))
                .await
                .expect("route_messages succeeds"),
        );
        assert_eq!(captured["count"], 3, "{captured}");
        assert_eq!(captured["messages"][0]["payload"]["n"], 0);
        assert_eq!(captured["messages"][2]["payload"]["n"], 2);
        // The capture bookkeeping keys are internal and must not reach the model.
        let metadata = &captured["messages"][0]["metadata"];
        assert_eq!(metadata["kind"], "test");
        assert!(metadata.get(CAPTURE_SOURCE_KEY).is_none(), "{metadata}");
        assert!(metadata.get(CAPTURE_TIME_KEY).is_none(), "{metadata}");
        assert!(
            !captured["messages"][0]["time"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "the capture time must be rendered: {captured}"
        );

        // Reads are destructive, so the second call sees nothing.
        let again = result_json(
            &server
                .route_messages(params(json!({ "name": "drain-lifecycle" })))
                .await
                .expect("route_messages succeeds"),
        );
        assert_eq!(again["count"], 0, "{again}");

        let stopped = result_json(
            &server
                .stop_route(params(json!({ "name": "drain-lifecycle" })))
                .await
                .expect("stop_route succeeds"),
        );
        assert_eq!(stopped["messages"], 3, "{stopped}");
        assert_eq!(stopped["discarded_captured_messages"], 0, "{stopped}");

        // A stopped route is gone from every reporting surface.
        assert!(
            server
                .route_status(params(json!({ "name": "drain-lifecycle" })))
                .await
                .is_err()
        );
        let listed = result_json(&server.list_routes().await.expect("list_routes succeeds"));
        assert_eq!(listed.as_array().expect("an array").len(), 0, "{listed}");
    }

    /// A name is claimed before the (slow) route startup, so a second start for
    /// the same name must be rejected rather than bringing up a second consumer
    /// of the same source.
    #[tokio::test]
    async fn a_duplicate_route_name_is_rejected() {
        let server = BridgeMcp::new();
        let source = topic("mcp_dup_src");
        let route = json!({
            "name": "dup",
            "route": { "input": memory_endpoint(&source), "output": { "null": null } },
        });

        server
            .start_route(params(route.clone()))
            .await
            .expect("the first start succeeds");
        let err = server
            .start_route(params(route))
            .await
            .expect_err("the second start must be rejected");
        assert!(err.to_string().contains("already exists"), "{err}");

        server
            .stop_route(params(json!({ "name": "dup" })))
            .await
            .expect("stop_route succeeds");
    }

    /// Two concurrent starts race for the same name; exactly one may win.
    #[tokio::test]
    async fn concurrent_starts_of_one_name_leave_a_single_route() {
        let server = BridgeMcp::new();
        let source = topic("mcp_race_src");
        let route = json!({
            "name": "raced",
            "route": { "input": memory_endpoint(&source), "output": { "null": null } },
        });

        let (a, b) = tokio::join!(
            server.start_route(params(route.clone())),
            server.start_route(params(route)),
        );
        assert_eq!(
            a.is_ok() as u8 + b.is_ok() as u8,
            1,
            "exactly one concurrent start may win"
        );

        let listed = result_json(&server.list_routes().await.expect("list_routes succeeds"));
        assert_eq!(listed.as_array().expect("an array").len(), 1, "{listed}");
        server
            .stop_route(params(json!({ "name": "raced" })))
            .await
            .expect("stop_route succeeds");
    }

    /// `publish` takes exactly one of `message` / `messages`; the malformed
    /// spellings must be reported rather than silently sending nothing.
    #[tokio::test]
    async fn publish_rejects_malformed_message_arguments() {
        let server = BridgeMcp::new();
        let target = topic("mcp_pub_bad");

        let neither = server
            .publish(params(json!({ "publisher": memory_endpoint(&target) })))
            .await
            .expect_err("neither message nor messages");
        assert!(neither.to_string().contains("either"), "{neither}");

        let both = server
            .publish(params(json!({
                "publisher": memory_endpoint(&target),
                "message": { "payload": "a" },
                "messages": [{ "payload": "b" }],
            })))
            .await
            .expect_err("both message and messages");
        assert!(both.to_string().contains("only one"), "{both}");

        let empty = server
            .publish(params(json!({
                "publisher": memory_endpoint(&target),
                "messages": [],
            })))
            .await
            .expect_err("an empty batch");
        assert!(empty.to_string().contains("must not be empty"), "{empty}");
    }

    /// A caller-supplied id that cannot be parsed must be reported, never
    /// silently replaced by a generated one.
    #[tokio::test]
    async fn publish_rejects_an_unparseable_message_id() {
        let server = BridgeMcp::new();
        let err = server
            .publish(params(json!({
                "publisher": memory_endpoint(&topic("mcp_bad_id")),
                "message": { "payload": "x", "message_id": { "not": "an id" } },
            })))
            .await
            .expect_err("a malformed message_id must be reported");
        assert!(err.to_string().contains("invalid message_id"), "{err}");
    }

    /// Every route tool must reject a name it does not know rather than
    /// reporting an empty-but-successful result.
    #[tokio::test]
    async fn the_route_tools_reject_an_unknown_name() {
        let server = BridgeMcp::new();
        let name = json!({ "name": "never-started" });
        assert!(server.route_status(params(name.clone())).await.is_err());
        assert!(server.stop_route(params(name.clone())).await.is_err());
        assert!(server.route_messages(params(name.clone())).await.is_err());
        assert!(
            server
                .wait_route(params(json!({ "name": "never-started" })))
                .await
                .is_err()
        );
    }

    /// A route that discarded messages explains itself in `wait_route`, not only
    /// in `route_status`. A permanent sink rejection with no DLQ configured is
    /// dropped rather than retried forever, so the route still *finishes* — but
    /// it must not report that finish as clean, or a caller that only ever waits
    /// cannot tell a full delivery from a silently discarded one.
    #[tokio::test]
    async fn a_failed_route_reports_its_cause_to_wait_route() {
        let server = BridgeMcp::new();
        let source = topic("mcp_fail_src");
        server
            .publish(params(json!({
                "publisher": memory_endpoint(&source),
                "message": { "payload": "x" },
            })))
            .await
            .expect("publish succeeds");
        server
            .start_route(params(json!({
                "name": "doomed",
                "route": {
                    "input": memory_endpoint(&source),
                    // Fault injection is the only permanent failure reachable
                    // without an external endpoint to break.
                    "output": {
                        "memory": { "topic": topic("mcp_fail_sink"), "capacity": 1000 },
                        "middlewares": [{ "random_panic": { "mode": "json_format_error" } }],
                    },
                    "allow_fault_injection": true,
                    "exit_on_empty": true,
                },
            })))
            .await
            .expect("start_route succeeds");

        let waited = result_json(
            &server
                .wait_route(params(json!({ "name": "doomed", "timeout_ms": 10_000 })))
                .await
                .expect("wait_route returns"),
        );
        assert_eq!(waited["finished"], true, "{waited}");
        assert_eq!(waited["outcome"], "completed", "{waited}");
        let error = waited["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("JSON format error"),
            "the failure cause reaches the caller: {waited}"
        );
        assert!(
            error.contains("dropped"),
            "the caller learns messages were discarded, not just that a send failed: {waited}"
        );

        server
            .stop_route(params(json!({ "name": "doomed" })))
            .await
            .expect("stop_route succeeds");
    }

    /// A route that is still running has no failure to report.
    /// `wait_route` leaves a route that outlives the timeout running, and says so.
    #[tokio::test]
    async fn wait_route_times_out_without_stopping_the_route() {
        let server = BridgeMcp::new();
        let source = topic("mcp_wait_src");
        server
            .start_route(params(json!({
                "name": "long-runner",
                "route": { "input": memory_endpoint(&source), "output": { "null": null } },
            })))
            .await
            .expect("start_route succeeds");

        let waited = result_json(
            &server
                .wait_route(params(json!({ "name": "long-runner", "timeout_ms": 100 })))
                .await
                .expect("wait_route returns"),
        );
        assert_eq!(waited["finished"], false, "{waited}");
        assert!(waited["outcome"].is_null(), "{waited}");
        assert!(waited["error"].is_null(), "{waited}");

        // Still running, and still stoppable.
        let status = result_json(
            &server
                .route_status(params(json!({ "name": "long-runner" })))
                .await
                .expect("route_status succeeds"),
        );
        assert_eq!(status["finished"], false, "{status}");
        server
            .stop_route(params(json!({ "name": "long-runner" })))
            .await
            .expect("stop_route succeeds");
    }

    /// The `timeout_ms` ceiling the tool description promises the model.
    #[test]
    fn the_wait_timeout_ceiling_matches_the_documented_maximum() {
        assert_eq!(MAX_WAIT_TIMEOUT_MS, 18_000_000);
    }

    /// A route started without `capture_last` captures nothing, so
    /// `route_messages` must answer empty rather than failing.
    #[tokio::test]
    async fn an_uncaptured_route_reports_no_messages() {
        let server = BridgeMcp::new();
        let source = topic("mcp_nocap_src");
        publish_n(&server, &source, 2).await;
        server
            .start_route(params(json!({
                "name": "uncaptured",
                "route": {
                    "input": memory_endpoint(&source),
                    "output": { "null": null },
                    "exit_on_empty": true,
                },
            })))
            .await
            .expect("start_route succeeds");

        let waited = wait_for(&server, "uncaptured").await;
        assert_eq!(waited["messages"], 2, "{waited}");

        let captured = result_json(
            &server
                .route_messages(params(json!({ "name": "uncaptured" })))
                .await
                .expect("route_messages succeeds"),
        );
        assert_eq!(captured["count"], 0, "{captured}");
    }

    /// The status report the UI receives must carry the route's liveness and
    /// counters, and must never carry the endpoint config.
    #[tokio::test]
    async fn the_ui_status_report_covers_routes_and_publishers_without_config() {
        let server = BridgeMcp::new();
        let source = topic("mcp_report_src");
        publish_n(&server, &source, 2).await;

        server
            .start_route(params(json!({
                "name": "reported",
                "route": {
                    "input": memory_endpoint(&source),
                    "output": { "null": null },
                    "exit_on_empty": true,
                },
            })))
            .await
            .expect("start_route succeeds");
        wait_for(&server, "reported").await;

        let report = server.status_report().await;
        let route = report
            .routes
            .get("reported")
            .expect("the route is reported");
        assert!(!route.snapshot.running, "a drained route is not running");
        assert_eq!(
            route.snapshot.outcome,
            Some(RouteOutcomeSnapshot::Completed)
        );
        assert_eq!(route.snapshot.message_sequence, 2);
        assert_eq!(route.snapshot.status.target, "reported");
        assert!(
            route.snapshot.status.details.is_null(),
            "config must not be reported"
        );

        // The publish above registered its target under the connector type.
        let publisher = report
            .publishers
            .get("memory")
            .expect("the publish target is reported");
        assert_eq!(publisher.message_sequence, 2);
        assert!(publisher.status.details.is_null());
    }
}

/// What a route start that fails, and a route that cannot make progress, leave
/// behind. Both are ordinary outcomes of a model handing over an endpoint that
/// does not resolve, so neither may corrupt what the next call reports.
#[cfg(test)]
mod start_failure_tests {
    use super::*;
    use serde_json::json;

    fn params<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Parameters<T> {
        Parameters(serde_json::from_value(value).expect("tool arguments parse"))
    }

    /// An `ipc://` address under a directory that does not exist cannot be bound,
    /// so `run()` fails rather than the route failing later.
    fn unstartable_input() -> serde_json::Value {
        json!({ "memory": { "url": "ipc:///nonexistent-dir-xyz/mqb-test.sock" } })
    }

    async fn start(
        server: &BridgeMcp,
        name: &str,
        input: serde_json::Value,
        capture_last: usize,
    ) -> Result<CallToolResult, McpError> {
        server
            .start_route(params(json!({
                "name": name,
                "route": { "input": input, "output": { "null": null }, "exit_on_empty": true },
                "capture_last": capture_last,
            })))
            .await
    }

    /// A failed start released the route name but kept the counter it had already
    /// created, so the sampler kept working on a dead key and a retry under the
    /// same name inherited the failed attempt's start time — reporting an
    /// `elapsed_s` that covered a startup which never happened.
    #[tokio::test]
    async fn a_failed_start_leaves_no_metrics_behind() {
        let server = BridgeMcp::new();
        let err = start(&server, "unstartable", unstartable_input(), 0)
            .await
            .expect_err("the route must fail to start");
        assert!(err.to_string().contains("failed to start route"), "{err}");

        assert!(
            !server.metrics.sequences().await.contains_key("unstartable"),
            "a failed start must not leave a counter: {:?}",
            server.metrics.sequences().await
        );
        assert!(server.metrics.timing("unstartable").await.is_none());
        // The name is free again, and nothing is listed as running.
        assert!(!server.routes.lock().await.contains_key("unstartable"));
        assert!(!server.starting.lock().await.contains("unstartable"));
    }

    /// The capture buffer is created before the route runs and lives in a
    /// process-global registry keyed by topic, so a failed start must empty it —
    /// otherwise the next route to claim the name would inherit its contents.
    #[tokio::test]
    async fn a_failed_start_does_not_poison_the_next_routes_capture() {
        let server = BridgeMcp::new();
        let name = format!("recapture_{}", uuid::Uuid::new_v4().simple());

        // Push something into the buffer the failed start will create, the way a
        // route that briefly ran would have.
        MessageCapture::with_capacity(&capture_topic(&name), 10)
            .push(&name, CanonicalMessage::from("stale"));
        start(&server, &name, unstartable_input(), 10)
            .await
            .expect_err("the route must fail to start");

        assert!(
            MessageCapture::open(&capture_topic(&name))
                .drain()
                .is_empty(),
            "a failed start must not leave captured messages for the next route"
        );
    }

    /// A source that never yields and never errors — a file path that does not
    /// exist — must not be reported as having moved anything, and must stay
    /// stoppable.
    #[tokio::test]
    async fn a_source_that_never_yields_stays_stoppable() {
        let server = BridgeMcp::new();
        start(
            &server,
            "stalled",
            json!({ "file": { "path": "/nonexistent-dir-xyz/in.jsonl" } }),
            0,
        )
        .await
        .expect("the route starts");

        let waited = server
            .wait_route(params(json!({ "name": "stalled", "timeout_ms": 200 })))
            .await
            .expect("wait_route returns");
        let waited = serde_json::from_str::<serde_json::Value>(
            &waited.content[0].as_text().expect("text").text,
        )
        .expect("json");
        assert_eq!(waited["messages"], 0, "{waited}");

        server
            .stop_route(params(json!({ "name": "stalled" })))
            .await
            .expect("a stalled route must still be stoppable");
        assert!(!server.metrics.sequences().await.contains_key("stalled"));
    }
}
