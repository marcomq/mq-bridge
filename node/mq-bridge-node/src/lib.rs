#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use ::mq_bridge as core;
use anyhow::Context;
use async_trait::async_trait;
use core::models::Endpoint;
use core::traits::{BatchCommitFunc, Handler, MessageConsumer, MessageDisposition};
use core::type_handler::TypeHandler;
use core::{
    CanonicalMessage, Handled, HandlerError, Publisher as CorePublisher, Route as CoreRoute, Sent,
    SentBatch,
};
use mq_bridge_bindings_common as common;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;
use serde_json::Value as JsonValue;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[napi(js_name = "version")]
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often a blocking `join()` checks whether the route ended on its own.
const ROUTE_END_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// JSON Schema for the route/config mapping, generated on demand from the
/// compiled Rust models (no checked-in copy, so it cannot drift).
#[cfg(feature = "schema")]
#[napi(js_name = "configSchema")]
pub fn config_schema() -> Result<JsonValue> {
    let schema = schemars::schema_for!(core::models::Config);
    serde_json::to_value(schema).map_err(to_napi_error)
}

/// One library log event, delivered to the `initLogging` callback.
#[napi(object)]
pub struct LogRecord {
    /// `error` / `warn` / `info` / `debug` / `trace`.
    pub level: String,
    /// Emitting module, e.g. `mq_bridge::route`.
    pub target: String,
    pub message: String,
}

/// A `tracing` layer that hands each event to the JS logging callback.
struct NodeLogLayer {
    callback: ThreadsafeFunction<LogRecord, (), LogRecord, Status, false, true>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for NodeLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let record = common::logging::record_from_event(event);
        let entry = LogRecord {
            level: record.level_str().to_string(),
            target: record.target,
            message: record.message,
        };
        let _ = self.callback.call(
            entry,
            napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

/// Route the library's internal `tracing` events into a JS callback so the host
/// logger (console, pino, winston, …) owns output. Call once at startup. `level`
/// seeds the Rust-side filter (default `"warn"`); `MQ_BRIDGE_LOG` / `RUST_LOG`
/// override it. Filtering happens in Rust, so suppressed events never reach JS.
/// The callback is held with a weak reference, so it will not keep the process
/// alive. Throws if logging was already initialized.
#[napi(js_name = "initLogging")]
pub fn init_logging(
    #[napi(ts_arg_type = "(record: LogRecord) => void")] callback: ThreadsafeFunction<
        LogRecord,
        (),
        LogRecord,
        Status,
        false,
        true,
    >,
    level: Option<String>,
) -> Result<()> {
    let filter = common::logging::env_filter(level.as_deref());
    tracing_subscriber::registry()
        .with(filter)
        .with(NodeLogLayer { callback })
        .try_init()
        .map_err(|err| {
            Error::from_reason(format!("mq_bridge logging is already initialized: {err}"))
        })?;
    Ok(())
}

/// Request a graceful shutdown of every route: running ones stop, and ones
/// started afterwards stop immediately. Wire it to the host's signal handling.
/// Returns `true` only for the first request. It cannot be undone.
#[napi(js_name = "requestShutdown")]
pub fn request_shutdown() -> bool {
    core::shutdown::request_shutdown()
}

#[napi(js_name = "isShutdownRequested")]
pub fn is_shutdown_requested() -> bool {
    core::shutdown::is_shutdown_requested()
}

#[napi(object)]
pub struct NativeMessage {
    pub payload: Buffer,
    pub metadata: Option<HashMap<String, String>>,
    pub id: Option<String>,
}

/// Result of `pollBatch()`: the messages plus the token used to `ack`/`nack` them.
#[napi(object)]
pub struct PollBatch {
    pub messages: Vec<NativeMessage>,
    /// `null` when the poll timed out or the source is exhausted.
    pub token: Option<u32>,
}

impl NativeMessage {
    fn from_canonical(message: &CanonicalMessage) -> Self {
        Self {
            payload: message.payload.to_vec().into(),
            metadata: Some(message.metadata.clone()),
            id: Some(core::canonical_message::format_message_id(
                message.message_id,
            )),
        }
    }

    fn into_canonical(self) -> Result<CanonicalMessage> {
        build_message(self.payload.to_vec(), self.metadata, self.id.as_deref())
    }
}

#[napi]
pub fn create_message(
    payload: Buffer,
    metadata: Option<HashMap<String, String>>,
    id: Option<String>,
) -> Result<NativeMessage> {
    id.as_deref().map(parse_message_id).transpose()?;
    Ok(NativeMessage {
        payload,
        metadata,
        id,
    })
}

#[napi]
pub fn message_from_json(
    data: JsonValue,
    metadata: Option<HashMap<String, String>>,
    id: Option<String>,
) -> Result<NativeMessage> {
    id.as_deref().map(parse_message_id).transpose()?;
    Ok(NativeMessage {
        payload: serde_json::to_vec(&data).map_err(to_napi_error)?.into(),
        metadata,
        id,
    })
}

#[napi]
pub fn message_json(message: NativeMessage) -> Result<JsonValue> {
    serde_json::from_slice(&message.payload).map_err(to_napi_error)
}

#[napi]
pub fn message_text(message: NativeMessage) -> Result<String> {
    std::str::from_utf8(&message.payload)
        .map(str::to_string)
        .map_err(to_napi_error)
}

struct JsMessageHandler {
    label: String,
    callback: ThreadsafeFunction<
        NativeMessage,
        Promise<Option<NativeMessage>>,
        NativeMessage,
        Status,
        true,
        true,
    >,
}

struct JsJsonHandler {
    label: String,
    callback: ThreadsafeFunction<
        JsonValue,
        Promise<Option<NativeMessage>>,
        JsonValue,
        Status,
        true,
        true,
    >,
}

#[async_trait]
impl Handler for JsMessageHandler {
    async fn handle(&self, msg: CanonicalMessage) -> std::result::Result<Handled, HandlerError> {
        let message = NativeMessage::from_canonical(&msg);
        let result = self
            .callback
            .call_async(Ok(message))
            .await
            .map_err(|err| handler_error(&self.label, err))?
            .await
            .map_err(|err| handler_error(&self.label, err))?;
        message_result_to_handled(result)
    }
}

#[async_trait]
impl Handler for JsJsonHandler {
    async fn handle(&self, msg: CanonicalMessage) -> std::result::Result<Handled, HandlerError> {
        let data = msg.parse::<JsonValue>().map_err(|err| {
            HandlerError::NonRetryable(anyhow::anyhow!("JSON handler parse failed: {err}"))
        })?;
        let result = self
            .callback
            .call_async(Ok(data))
            .await
            .map_err(|err| handler_error(&self.label, err))?
            .await
            .map_err(|err| handler_error(&self.label, err))?;
        message_result_to_handled(result)
    }
}

fn message_result_to_handled(
    result: Option<NativeMessage>,
) -> std::result::Result<Handled, HandlerError> {
    match result {
        Some(message) => message
            .into_canonical()
            .map(Handled::Publish)
            .map_err(|err| HandlerError::NonRetryable(anyhow::anyhow!(err.to_string()))),
        None => Ok(Handled::Ack),
    }
}

fn handler_error(label: &str, err: napi::Error) -> HandlerError {
    HandlerError::NonRetryable(anyhow::anyhow!("Node handler '{label}' failed: {err}"))
}

// --- Custom endpoints implemented in JavaScript -----------------------------

/// One operation on a JS endpoint instance. A single dispatch callback per
/// registered name carries all of them: JS objects are not `Send`, so the
/// instance table lives on the JS side and Rust refers to instances by id.
#[napi(object)]
pub struct EndpointCall {
    /// `create` | `receive` | `commit` | `send` | `close`.
    pub op: String,
    /// Identifies the endpoint instance the op applies to.
    pub instance: u32,
    pub route_name: String,
    /// The endpoint's config block. `create` only.
    pub config: Option<JsonValue>,
    /// `receive` only.
    pub max_messages: Option<u32>,
    /// `send` only.
    pub messages: Option<Vec<NativeMessage>>,
    /// `commit` only: one `"ack"`/`"nack"` per message in the batch.
    pub dispositions: Option<Vec<String>>,
}

/// What the JS dispatcher reports back. Failures come back as a populated
/// `error` rather than a rejected promise, so the retryable flag survives.
#[napi(object)]
pub struct EndpointReply {
    /// `receive` only. Absent or empty means "nothing right now".
    pub messages: Option<Vec<NativeMessage>>,
    /// `receive` only: the source is finished, so the route can stop.
    pub end_of_stream: Option<bool>,
    /// `create` only: whether the instance implements `receiveBatch`.
    pub consumer: Option<bool>,
    /// `create` only: whether the instance implements `sendBatch`.
    pub publisher: Option<bool>,
    /// `create` only: whether the instance implements `onReceive`.
    pub on_receive: Option<bool>,
    /// `create` only: whether the instance implements `onSend`.
    pub on_send: Option<bool>,
    /// `onReceive`/`onSend` only: one slot per input message, `null` for dropped.
    pub filtered: Option<Vec<Option<NativeMessage>>>,
    pub error: Option<String>,
    /// Whether `error` should be retried. When absent, consumer errors are retried
    /// as connection failures while publisher errors are non-retryable.
    pub retryable: Option<bool>,
}

/// Strongly referenced (`Weak = false`) on purpose: a weak dispatch does not keep
/// the event loop referenced, so on an otherwise idle loop the endpoint's queued
/// calls are never drained and the route silently stalls.
type EndpointDispatch =
    ThreadsafeFunction<EndpointCall, Promise<EndpointReply>, EndpointCall, Status, true, false>;

static NEXT_ENDPOINT_INSTANCE: AtomicU32 = AtomicU32::new(1);

struct JsEndpointFactory {
    name: String,
    dispatch: Arc<EndpointDispatch>,
}

impl std::fmt::Debug for JsEndpointFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsEndpointFactory")
            .field("name", &self.name)
            .finish()
    }
}

/// Runs one op and turns a rejected promise, a dispatcher failure, or a reported
/// `error` into a single uniform failure.
async fn call_js_endpoint(
    dispatch: &EndpointDispatch,
    name: &str,
    call: EndpointCall,
) -> std::result::Result<EndpointReply, (String, Option<bool>)> {
    let op = call.op.clone();
    let reply = dispatch
        .call_async(Ok(call))
        .await
        .map_err(|err| (format!("Node endpoint '{name}' {op} failed: {err}"), None))?
        .await
        .map_err(|err| (format!("Node endpoint '{name}' {op} failed: {err}"), None))?;
    if let Some(error) = reply.error {
        return Err((
            format!("Node endpoint '{name}' {op} failed: {error}"),
            reply.retryable,
        ));
    }
    Ok(reply)
}

/// Which side of the endpoint a handle drives, so the lazy `create` can check
/// that the host object actually implements it.
#[derive(Clone, Copy, Eq, PartialEq)]
enum JsEndpointRole {
    Consumer,
    Publisher,
    Middleware(JsMiddlewareSide),
}

#[async_trait]
impl core::traits::CustomEndpointFactory for JsEndpointFactory {
    async fn create_consumer(
        &self,
        route_name: &str,
        config: &JsonValue,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(JsEndpointConsumer {
            inner: Arc::new(self.instance(route_name, config, JsEndpointRole::Consumer)),
        }))
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &JsonValue,
    ) -> anyhow::Result<Box<dyn core::traits::MessagePublisher>> {
        Ok(Box::new(JsEndpointPublisher {
            inner: Arc::new(self.instance(route_name, config, JsEndpointRole::Publisher)),
        }))
    }
}

impl JsEndpointFactory {
    fn instance(
        &self,
        route_name: &str,
        config: &JsonValue,
        role: JsEndpointRole,
    ) -> JsEndpointInstance {
        JsEndpointInstance {
            name: self.name.clone(),
            route_name: route_name.to_string(),
            config: config.clone(),
            role,
            instance: tokio::sync::OnceCell::new(),
            dispatch: Arc::clone(&self.dispatch),
        }
    }
}

/// A handle to one JS endpoint instance, shared by the endpoint itself and the
/// commit closures it hands to the route.
///
/// The host object is built on first use rather than at route startup: `start()`
/// blocks the JS thread while it deploys the route, so a `create` dispatched
/// from there could never be serviced by the event loop.
struct JsEndpointInstance {
    name: String,
    route_name: String,
    config: JsonValue,
    role: JsEndpointRole,
    /// `Some(id)` once the host object exists; `Some(None)` when it exists but
    /// does not implement this middleware side, which means "pass through".
    instance: tokio::sync::OnceCell<Option<u32>>,
    dispatch: Arc<EndpointDispatch>,
}

impl JsEndpointInstance {
    /// Builds the host object once. A failure leaves the cell empty, so a
    /// genuinely transient connect failure is retried on the next call.
    async fn instance_id(&self) -> std::result::Result<Option<u32>, (String, Option<bool>)> {
        self.instance
            .get_or_try_init(|| async {
                let id = NEXT_ENDPOINT_INSTANCE.fetch_add(1, Ordering::SeqCst);
                let reply = match call_js_endpoint(
                    &self.dispatch,
                    &self.name,
                    EndpointCall {
                        op: "create".to_string(),
                        instance: id,
                        route_name: self.route_name.clone(),
                        config: Some(self.config.clone()),
                        max_messages: None,
                        messages: None,
                        dispositions: None,
                    },
                )
                .await
                {
                    Ok(reply) => reply,
                    Err(err) => {
                        self.close_instance(id);
                        return Err(err);
                    }
                };
                // JS has already put the instance in its table, so every path
                // that does not keep the id has to close it or the host object
                // (and whatever connection it opened) leaks for the process.
                // A missing method is a config error, not a transport blip:
                // flag it non-retryable so the route stops instead of looping.
                let outcome = match self.role {
                    JsEndpointRole::Consumer if reply.consumer != Some(true) => Err((
                        format!(
                            "Node endpoint '{}' has no receiveBatch(maxMessages) method, so it cannot be used as an input",
                            self.name
                        ),
                        Some(false),
                    )),
                    JsEndpointRole::Publisher if reply.publisher != Some(true) => Err((
                        format!(
                            "Node endpoint '{}' has no sendBatch(messages) method, so it cannot be used as an output",
                            self.name
                        ),
                        Some(false),
                    )),
                    // A missing middleware hook is not an error — the caller
                    // reads `None` as "pass through".
                    JsEndpointRole::Middleware(JsMiddlewareSide::Receive)
                        if reply.on_receive != Some(true) =>
                    {
                        Ok(None)
                    }
                    JsEndpointRole::Middleware(JsMiddlewareSide::Send)
                        if reply.on_send != Some(true) =>
                    {
                        Ok(None)
                    }
                    _ => Ok(Some(id)),
                };
                if !matches!(outcome, Ok(Some(_))) {
                    self.close_instance(id);
                }
                outcome
            })
            .await
            .copied()
    }

    /// Like `instance_id`, for the endpoint roles where "no instance" cannot
    /// happen — a missing method is reported as an error there instead.
    async fn endpoint_instance_id(&self) -> std::result::Result<u32, (String, Option<bool>)> {
        self.instance_id().await?.ok_or_else(|| {
            (
                format!("Node endpoint '{}' was not created", self.name),
                Some(false),
            )
        })
    }

    /// Drops the JS-side instance-table entry and lets the host object release
    /// whatever it opened. Fire-and-forget: nothing here can await a reply.
    fn close_instance(&self, instance: u32) {
        let _ = self.dispatch.call(
            Ok(self.call("close", instance)),
            napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
        );
    }

    fn call(&self, op: &str, instance: u32) -> EndpointCall {
        EndpointCall {
            op: op.to_string(),
            instance,
            route_name: self.route_name.clone(),
            config: None,
            max_messages: None,
            messages: None,
            dispositions: None,
        }
    }

    async fn run(
        &self,
        call: EndpointCall,
    ) -> std::result::Result<EndpointReply, (String, Option<bool>)> {
        call_js_endpoint(&self.dispatch, &self.name, call).await
    }
}

impl Drop for JsEndpointInstance {
    /// Let JS drop its instance-table entry, and give a host object holding a
    /// real connection the chance to close it. Nothing to do if it was never
    /// built.
    fn drop(&mut self) {
        // Only the id we kept: the paths that discarded one closed it already.
        let Some(&Some(instance)) = self.instance.get() else {
            return;
        };
        self.close_instance(instance);
    }
}

struct JsEndpointConsumer {
    inner: Arc<JsEndpointInstance>,
}

#[async_trait]
impl MessageConsumer for JsEndpointConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> std::result::Result<core::ReceivedBatch, core::errors::ConsumerError> {
        let instance = self
            .inner
            .endpoint_instance_id()
            .await
            .map_err(js_consumer_error)?;
        let mut call = self.inner.call("receive", instance);
        call.max_messages = Some(max_messages.min(u32::MAX as usize) as u32);
        let reply = self.inner.run(call).await.map_err(js_consumer_error)?;
        if reply.end_of_stream == Some(true) {
            return Err(core::errors::ConsumerError::EndOfStream);
        }
        let messages = reply
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(NativeMessage::into_canonical)
            .collect::<Result<Vec<_>>>()
            .map_err(|err| {
                core::errors::ConsumerError::Permanent(anyhow::anyhow!(err.to_string()))
            })?;
        if messages.is_empty() {
            return Ok(core::ReceivedBatch::empty());
        }
        let inner = Arc::clone(&self.inner);
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            let names = dispositions.iter().map(disposition_name).collect();
            Box::pin(async move {
                let instance = inner
                    .endpoint_instance_id()
                    .await
                    .map_err(|(message, _)| anyhow::anyhow!(message))?;
                let mut call = inner.call("commit", instance);
                call.dispositions = Some(names);
                inner
                    .run(call)
                    .await
                    .map(|_| ())
                    .map_err(|(message, _)| anyhow::anyhow!(message))
            })
        });
        Ok(core::ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct JsEndpointPublisher {
    inner: Arc<JsEndpointInstance>,
}

#[async_trait]
impl core::traits::MessagePublisher for JsEndpointPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> std::result::Result<SentBatch, core::errors::PublisherError> {
        let instance = self
            .inner
            .endpoint_instance_id()
            .await
            .map_err(js_publisher_error)?;
        let mut call = self.inner.call("send", instance);
        call.messages = Some(messages.iter().map(NativeMessage::from_canonical).collect());
        self.inner
            .run(call)
            .await
            .map(|_| SentBatch::Ack)
            .map_err(js_publisher_error)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A failed read is a transport problem unless the host explicitly said the
/// error cannot heal, so the route reconnects rather than dropping the source.
fn js_consumer_error((message, retryable): (String, Option<bool>)) -> core::errors::ConsumerError {
    match retryable {
        Some(false) => core::errors::ConsumerError::Permanent(anyhow::anyhow!(message)),
        _ => core::errors::ConsumerError::Connection(anyhow::anyhow!(message)),
    }
}

/// A failed write is non-retryable unless the host flagged it, matching Node
/// handlers, so `dlq` catches it and `retry` does not spin on it.
fn js_publisher_error(
    (message, retryable): (String, Option<bool>),
) -> core::errors::PublisherError {
    match retryable {
        Some(true) => core::errors::PublisherError::Retryable(anyhow::anyhow!(message)),
        _ => core::errors::PublisherError::NonRetryable(anyhow::anyhow!(message)),
    }
}

/// The string a `MessageDisposition` is reported as to a JS `commit`.
/// A `Reply` payload is not delivered to host endpoints yet; it acks.
fn disposition_name(disposition: &MessageDisposition) -> String {
    match disposition {
        MessageDisposition::Ack | MessageDisposition::Reply(_) => "ack".to_string(),
        MessageDisposition::Nack => "nack".to_string(),
    }
}

/// Which hook a middleware handle drives.
#[derive(Clone, Copy, Eq, PartialEq)]
enum JsMiddlewareSide {
    Receive,
    Send,
}

struct JsMiddlewareFactory {
    name: String,
    dispatch: Arc<EndpointDispatch>,
}

impl std::fmt::Debug for JsMiddlewareFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsMiddlewareFactory")
            .field("name", &self.name)
            .finish()
    }
}

impl JsMiddlewareFactory {
    /// Wraps unconditionally and decides pass-through on first use: `apply_*`
    /// runs while `start()` blocks the JS thread, so nothing may dispatch to JS
    /// here — the event loop could not service it.
    fn handle(
        &self,
        route_name: &str,
        config: &JsonValue,
        side: JsMiddlewareSide,
    ) -> Arc<JsEndpointInstance> {
        Arc::new(JsEndpointInstance {
            name: self.name.clone(),
            route_name: route_name.to_string(),
            config: config.clone(),
            role: JsEndpointRole::Middleware(side),
            instance: tokio::sync::OnceCell::new(),
            dispatch: Arc::clone(&self.dispatch),
        })
    }
}

#[async_trait]
impl core::traits::CustomMiddlewareFactory for JsMiddlewareFactory {
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        route_name: &str,
        config: &JsonValue,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(JsMiddlewareConsumer {
            inner: consumer,
            middleware: self.handle(route_name, config, JsMiddlewareSide::Receive),
        }))
    }

    async fn apply_publisher(
        &self,
        publisher: Box<dyn core::traits::MessagePublisher>,
        route_name: &str,
        config: &JsonValue,
    ) -> anyhow::Result<Box<dyn core::traits::MessagePublisher>> {
        Ok(Box::new(JsMiddlewarePublisher {
            inner: publisher,
            middleware: self.handle(route_name, config, JsMiddlewareSide::Send),
        }))
    }
}

/// Runs one middleware hook, returning one slot per input message. The caller
/// resolves `instance` first so a pass-through side never has to copy the batch.
async fn run_js_filter(
    handle: &JsEndpointInstance,
    op: &str,
    instance: u32,
    messages: &[CanonicalMessage],
) -> std::result::Result<Vec<Option<CanonicalMessage>>, (String, Option<bool>)> {
    let expected = messages.len();
    let mut call = handle.call(op, instance);
    call.messages = Some(messages.iter().map(NativeMessage::from_canonical).collect());
    let reply = handle.run(call).await?;
    let filtered = reply.filtered.unwrap_or_default();
    if filtered.len() != expected {
        return Err((
            format!(
                "Node middleware '{}' {op} returned {} items for a batch of {expected}; return one item per message (null to drop it)",
                handle.name,
                filtered.len()
            ),
            Some(false),
        ));
    }
    filtered
        .into_iter()
        .map(|slot| slot.map(NativeMessage::into_canonical).transpose())
        .collect::<Result<Vec<_>>>()
        .map_err(|err| (err.to_string(), Some(false)))
}

struct JsMiddlewareConsumer {
    inner: Box<dyn MessageConsumer>,
    middleware: Arc<JsEndpointInstance>,
}

#[async_trait]
impl MessageConsumer for JsMiddlewareConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> std::result::Result<core::ReceivedBatch, core::errors::ConsumerError> {
        // Keep pulling until something survives the filter. Returning an empty
        // batch here would be read as "the source is drained" and would end an
        // `exit_on_empty` route early; only the inner consumer may say that.
        loop {
            let batch = self.inner.receive_batch(max_messages).await?;
            if batch.messages.is_empty() {
                return Ok(batch);
            }
            let Some(instance) = self
                .middleware
                .instance_id()
                .await
                .map_err(js_consumer_error)?
            else {
                return Ok(batch);
            };
            let core::ReceivedBatch { messages, commit } = batch;
            let results = run_js_filter(&self.middleware, "onReceive", instance, &messages)
                .await
                .map_err(js_consumer_error)?;

            let mut kept = Vec::with_capacity(results.len());
            let mut keep_flags = Vec::with_capacity(results.len());
            for result in results {
                keep_flags.push(result.is_some());
                if let Some(message) = result {
                    kept.push(message);
                }
            }

            let inner_commit = commit;
            if kept.is_empty() {
                // The route will never commit a batch it never sees, so ack the
                // dropped messages here or the source redelivers them forever.
                inner_commit(vec![MessageDisposition::Ack; keep_flags.len()])
                    .await
                    .map_err(core::errors::ConsumerError::Connection)?;
                continue;
            }

            // The route only sees the kept messages, so expand its dispositions
            // back to one per source message, acking the ones we dropped.
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                let mut kept_dispositions = dispositions.into_iter();
                let expanded: Vec<MessageDisposition> = keep_flags
                    .iter()
                    .map(|keep| {
                        if *keep {
                            kept_dispositions.next().unwrap_or(MessageDisposition::Ack)
                        } else {
                            MessageDisposition::Ack
                        }
                    })
                    .collect();
                inner_commit(expanded)
            });
            return Ok(core::ReceivedBatch {
                messages: kept,
                commit,
            });
        }
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.inner.close().await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct JsMiddlewarePublisher {
    inner: Box<dyn core::traits::MessagePublisher>,
    middleware: Arc<JsEndpointInstance>,
}

#[async_trait]
impl core::traits::MessagePublisher for JsMiddlewarePublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> std::result::Result<SentBatch, core::errors::PublisherError> {
        let Some(instance) = self
            .middleware
            .instance_id()
            .await
            .map_err(js_publisher_error)?
        else {
            return self.inner.send_batch(messages).await;
        };
        let results = run_js_filter(&self.middleware, "onSend", instance, &messages)
            .await
            .map_err(js_publisher_error)?;
        let kept: Vec<CanonicalMessage> = results.into_iter().flatten().collect();
        if kept.is_empty() {
            return Ok(SentBatch::Ack);
        }
        self.inner.send_batch(kept).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Register a JS middleware dispatcher under `name`. Called by the
/// `registerMiddleware` wrapper in `index.js` — do not call directly.
#[napi(js_name = "registerMiddlewareDispatch")]
pub fn register_middleware_dispatch(
    name: String,
    dispatch: ThreadsafeFunction<
        EndpointCall,
        Promise<EndpointReply>,
        EndpointCall,
        Status,
        true,
        false,
    >,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::from_reason("middleware name must not be empty"));
    }
    core::extensions::register_middleware_factory(
        &name,
        Arc::new(JsMiddlewareFactory {
            name: name.clone(),
            dispatch: Arc::new(dispatch),
        }),
    )
    .map_err(|err| Error::from_reason(format!("{err:#}")))?;
    Ok(())
}

/// Unregister a JS middleware dispatcher after all routes using it have stopped.
#[napi(js_name = "unregisterMiddlewareDispatch")]
pub fn unregister_middleware_dispatch(name: String) -> bool {
    core::extensions::unregister_middleware_factory(&name)
}

/// Load a native endpoint plugin and register the endpoint it provides.
///
/// `path` is the compiled plugin library shipped by an endpoint package (for
/// example `mq-bridge-pulsar`); those packages expose a `register()` helper that
/// resolves the bundled file and calls this. Returns the registered endpoint
/// name, usable as a route's endpoint type.
///
/// Call once, before starting routes. Loading the same file again is a no-op.
/// A plugin is native code with the same privileges as the Node process.
#[napi(js_name = "loadEndpointPlugin")]
pub fn load_endpoint_plugin(path: String) -> Result<String> {
    #[cfg(feature = "plugin")]
    {
        core::plugin::load_endpoint_plugin(&path)
            .map(|info| info.name)
            .map_err(|err| Error::from_reason(format!("{err:#}")))
    }
    #[cfg(not(feature = "plugin"))]
    {
        let _ = path;
        Err(Error::from_reason(
            "this mq-bridge build was compiled without native plugin support",
        ))
    }
}

/// Register a JS endpoint dispatcher under `name`. Called by the `registerEndpoint`
/// wrapper in `index.js`, which owns the instance table — do not call directly.
#[napi(js_name = "registerEndpointDispatch")]
pub fn register_endpoint_dispatch(
    name: String,
    dispatch: ThreadsafeFunction<
        EndpointCall,
        Promise<EndpointReply>,
        EndpointCall,
        Status,
        true,
        false,
    >,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::from_reason("endpoint name must not be empty"));
    }
    core::extensions::register_endpoint_factory(
        &name,
        Arc::new(JsEndpointFactory {
            name: name.clone(),
            dispatch: Arc::new(dispatch),
        }),
    )
    .map_err(|err| Error::from_reason(format!("{err:#}")))?;
    Ok(())
}

/// Unregister a JS endpoint dispatcher after all routes using it have stopped.
#[napi(js_name = "unregisterEndpointDispatch")]
pub fn unregister_endpoint_dispatch(name: String) -> bool {
    core::extensions::unregister_endpoint_factory(&name)
}

#[napi]
pub struct Route {
    runtime: Arc<Runtime>,
    route: Arc<Mutex<CoreRoute>>,
    name: String,
    run_state: Arc<Mutex<RouteRunState>>,
}

#[derive(Default)]
struct RouteRunState {
    running: bool,
    stop_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
    /// Set when a route started with `start()` ended on a permanent failure, so
    /// `join()` can report the cause instead of returning as if it had stopped
    /// cleanly. Taken by `join()`.
    failure: Option<String>,
}

#[napi]
impl Route {
    /// Build a route from a YAML or JSON config file. Accepts a `routes:`
    /// document, a bare `{name: route}` map, or a single route body. Omit
    /// `name` (or pass `""`) when the file is a single bare route body.
    #[napi(factory)]
    pub fn from_file(path: String, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref()).map(str::to_string);
        let route =
            common::load_named_route(Path::new(&path), name.as_deref()).map_err(to_napi_error)?;
        Self::build(route, name.unwrap_or_else(common::default_route_name))
    }

    /// Deprecated alias for `from_file`.
    #[napi(factory)]
    pub fn from_yaml(path: String, name: Option<String>) -> Result<Self> {
        Self::from_file(path, name)
    }

    /// Build a route from an in-memory YAML or JSON string. Accepts the same
    /// shapes as `from_file`. Omit `name` (or pass `""`) when the string is a
    /// single bare route body.
    #[napi(factory)]
    pub fn from_str(text: String, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref()).map(str::to_string);
        let value = serde_yaml_ng::from_str(&text)
            .context("failed to parse YAML config")
            .map_err(to_napi_error)?;
        let route =
            common::named_route_from_value(value, name.as_deref()).map_err(to_napi_error)?;
        Self::build(route, name.unwrap_or_else(common::default_route_name))
    }

    /// Deprecated alias for `from_str`.
    #[napi(factory)]
    pub fn from_yaml_str(text: String, name: Option<String>) -> Result<Self> {
        Self::from_str(text, name)
    }

    /// Build a route from an in-memory mapping (e.g. a JS object). Accepts a
    /// `routes:` document, a bare `{name: route}` map, or a single route body.
    /// Omit `name` (or pass `""`) to treat the mapping as a single bare route
    /// body, in which case a name is generated automatically.
    #[napi(factory)]
    pub fn from_config(config: JsonValue, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref()).map(str::to_string);
        let value = serde_yaml_ng::to_value(config)
            .context("failed to convert config mapping")
            .map_err(to_napi_error)?;
        let route =
            common::named_route_from_value(value, name.as_deref()).map_err(to_napi_error)?;
        Self::build(route, name.unwrap_or_else(common::default_route_name))
    }

    #[napi]
    pub fn with_handler(
        &self,
        callback: ThreadsafeFunction<
            NativeMessage,
            Promise<Option<NativeMessage>>,
            NativeMessage,
            Status,
            true,
            true,
        >,
    ) -> Result<()> {
        self.ensure_not_running()?;
        let mut route = self.lock_route()?;
        let updated = route.clone().with_handler(JsMessageHandler {
            label: self.name.clone(),
            callback,
        });
        *route = updated;
        Ok(())
    }

    #[napi]
    pub fn add_handler(
        &self,
        kind: String,
        callback: ThreadsafeFunction<
            JsonValue,
            Promise<Option<NativeMessage>>,
            JsonValue,
            Status,
            true,
            true,
        >,
    ) -> Result<()> {
        self.ensure_not_running()?;
        let mut route = self.lock_route()?;
        let prev_handler = route.output.handler.take();
        let js_handler: Arc<dyn Handler> = Arc::new(JsJsonHandler {
            label: format!("{}:{kind}", self.name),
            callback,
        });

        let new_handler = if let Some(existing) = prev_handler {
            if let Some(extended) = existing.register_handler(&kind, Arc::clone(&js_handler)) {
                extended
            } else {
                Arc::new(
                    TypeHandler::new()
                        .with_fallback(existing)
                        .add_handler(&kind, js_handler),
                )
            }
        } else {
            Arc::new(TypeHandler::new().add_handler(&kind, js_handler))
        };

        route.output.handler = Some(new_handler);
        Ok(())
    }

    #[napi]
    pub fn start(&self) -> Result<()> {
        let route = self.lock_route()?.clone();
        let stop_rx = self.begin_run()?;
        let name = self.name.clone();
        let runtime = Arc::clone(&self.runtime);
        let run_state = Arc::clone(&self.run_state);

        let deploy_name = name.clone();
        let deploy_runtime = Arc::clone(&runtime);
        if let Err(err) = deploy_runtime.block_on(async move { route.deploy(&deploy_name).await }) {
            finish_run(&run_state, &name);
            return Err(to_napi_error(err));
        }

        let wait_name = name.clone();
        let wait_run_state = Arc::clone(&run_state);
        let handle = match thread::Builder::new()
            .name(format!("mqb-node-route-{name}"))
            .spawn(move || {
                let stop_name = wait_name.clone();
                let result = runtime.block_on(async move {
                    let outcome = wait_for_stop_or_end(&stop_name, stop_rx).await;
                    let result = outcome_to_result(&stop_name, outcome);
                    core::Route::stop(&stop_name).await;
                    result
                });
                if let Err(err) = result {
                    if let Ok(mut state) = wait_run_state.lock() {
                        state.failure = Some(err.to_string());
                    }
                }
                finish_run(&wait_run_state, &wait_name);
            }) {
            Ok(handle) => handle,
            Err(err) => {
                // Spawn failed after the route was deployed; stop it and clear
                // run state so the name isn't orphaned and can be started again.
                deploy_runtime.block_on(async { core::Route::stop(&name).await });
                finish_run(&run_state, &name);
                return Err(to_napi_error(err));
            }
        };

        self.lock_run_state()?.join_handle = Some(handle);
        Ok(())
    }

    #[napi]
    pub fn stop(&self) -> Result<()> {
        let stop_tx = self.lock_run_state()?.stop_tx.take();
        if let Some(stop_tx) = stop_tx {
            let _ = stop_tx.send(());
        }
        Ok(())
    }

    /// Block until the route has fully stopped — either by `stop()` or by ending
    /// on its own (a drained source under `exit_on_empty`, an exhausted stream).
    /// Throws if the route ended on a permanent error.
    #[napi]
    pub fn join(&self) -> Result<()> {
        let handle = self.lock_run_state()?.join_handle.take();
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| Error::from_reason("Route background thread panicked"))?;
        }
        if let Some(failure) = self.lock_run_state()?.failure.take() {
            return Err(Error::from_reason(failure));
        }
        Ok(())
    }

    /// Like `join()`, but resolves asynchronously so the event loop, and with it
    /// `process.on('SIGINT')` handlers, keeps running while the route does.
    #[napi]
    pub async fn wait(&self) -> Result<()> {
        let handle = self.lock_run_state()?.join_handle.take();
        if let Some(handle) = handle {
            tokio::task::spawn_blocking(move || handle.join())
                .await
                .map_err(to_napi_error)?
                .map_err(|_| Error::from_reason("Route background thread panicked"))?;
        }
        if let Some(failure) = self.lock_run_state()?.failure.take() {
            return Err(Error::from_reason(failure));
        }
        Ok(())
    }
}

impl Route {
    fn build(route: CoreRoute, name: String) -> Result<Self> {
        Ok(Self {
            runtime: Arc::new(common::build_runtime().map_err(to_napi_error)?),
            route: Arc::new(Mutex::new(route)),
            name,
            run_state: Arc::new(Mutex::new(RouteRunState::default())),
        })
    }

    fn begin_run(&self) -> Result<oneshot::Receiver<()>> {
        let mut state = self.lock_run_state()?;
        if state.running {
            return Err(Error::from_reason("Route is already running"));
        }
        let mut active_route_names = lock_active_route_names()?;
        if active_route_names.contains(&self.name) || core::Route::get(&self.name).is_some() {
            return Err(Error::from_reason(format!(
                "A route named '{}' is already running",
                self.name
            )));
        }
        active_route_names.insert(self.name.clone());
        let (stop_tx, stop_rx) = oneshot::channel();
        state.running = true;
        state.stop_tx = Some(stop_tx);
        // Drop any failure left unclaimed by a previous run, so `join()` cannot
        // report a stale error for this one.
        state.failure = None;
        Ok(stop_rx)
    }

    fn lock_route(&self) -> Result<std::sync::MutexGuard<'_, CoreRoute>> {
        self.route
            .lock()
            .map_err(|_| Error::from_reason("Route lock poisoned"))
    }

    fn lock_run_state(&self) -> Result<std::sync::MutexGuard<'_, RouteRunState>> {
        self.run_state
            .lock()
            .map_err(|_| Error::from_reason("Route state lock poisoned"))
    }

    fn ensure_not_running(&self) -> Result<()> {
        if self.lock_run_state()?.running {
            Err(Error::from_reason(
                "Route handlers cannot be modified while the route is running",
            ))
        } else {
            Ok(())
        }
    }
}

#[napi]
pub struct Publisher {
    runtime: Arc<Runtime>,
    publisher: CorePublisher,
}

#[napi]
impl Publisher {
    /// Build a publisher endpoint from a YAML or JSON config file. Omit `name`
    /// (or pass `""`) when the file is a single bare endpoint body.
    #[napi(factory)]
    pub fn from_file(path: String, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref());
        Self::build(common::load_named_publisher(Path::new(&path), name).map_err(to_napi_error)?)
    }

    /// Deprecated alias for `from_file`.
    #[napi(factory)]
    pub fn from_yaml(path: String, name: Option<String>) -> Result<Self> {
        Self::from_file(path, name)
    }

    /// Build a publisher endpoint from an in-memory YAML or JSON string. Omit
    /// `name` (or pass `""`) when the string is a single bare endpoint body.
    #[napi(factory)]
    pub fn from_str(text: String, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref());
        let value = serde_yaml_ng::from_str(&text)
            .context("failed to parse YAML config")
            .map_err(to_napi_error)?;
        Self::build(common::named_publisher_from_value(value, name).map_err(to_napi_error)?)
    }

    /// Deprecated alias for `from_str`.
    #[napi(factory)]
    pub fn from_yaml_str(text: String, name: Option<String>) -> Result<Self> {
        Self::from_str(text, name)
    }

    /// Build a publisher endpoint from an in-memory mapping (e.g. a JS object).
    /// Omit `name` (or pass `""`) to treat the mapping as a single bare endpoint
    /// body.
    #[napi(factory)]
    pub fn from_config(config: JsonValue, name: Option<String>) -> Result<Self> {
        let name = common::normalize_name(name.as_deref());
        let value = serde_yaml_ng::to_value(config)
            .context("failed to convert config mapping")
            .map_err(to_napi_error)?;
        Self::build(common::named_publisher_from_value(value, name).map_err(to_napi_error)?)
    }

    #[napi]
    pub async fn send(&self, message: NativeMessage) -> Result<()> {
        self.send_on_runtime(message.into_canonical()?).await
    }

    #[napi]
    pub async fn send_batch(&self, messages: Vec<NativeMessage>) -> Result<()> {
        let batch = messages
            .into_iter()
            .map(NativeMessage::into_canonical)
            .collect::<Result<Vec<_>>>()?;
        self.send_batch_on_runtime(batch).await
    }

    #[napi]
    pub async fn request(&self, message: NativeMessage) -> Result<NativeMessage> {
        self.request_on_runtime(message.into_canonical()?).await
    }

    #[napi]
    pub async fn send_json(
        &self,
        data: JsonValue,
        metadata: Option<HashMap<String, String>>,
        id: Option<String>,
    ) -> Result<()> {
        self.send_on_runtime(json_input_to_canonical(data, metadata, id.as_deref())?)
            .await
    }

    #[napi]
    pub async fn request_json(
        &self,
        data: JsonValue,
        metadata: Option<HashMap<String, String>>,
        id: Option<String>,
    ) -> Result<NativeMessage> {
        self.request_on_runtime(json_input_to_canonical(data, metadata, id.as_deref())?)
            .await
    }
}

impl Publisher {
    fn build(endpoint: Endpoint) -> Result<Self> {
        let runtime = Arc::new(common::build_runtime().map_err(to_napi_error)?);
        let publisher = runtime
            .block_on(CorePublisher::new(endpoint))
            .map_err(to_napi_error)?;
        Ok(Self { runtime, publisher })
    }

    // Run the work on this publisher's own runtime (where its transport is
    // bound) and await the result without blocking the JS thread. Awaiting the
    // JoinHandle is runtime-agnostic, so napi's executor only parks on it.
    async fn send_on_runtime(&self, message: CanonicalMessage) -> Result<()> {
        let publisher = self.publisher.clone();
        let handle = self
            .runtime
            .spawn(async move { publisher.send(message).await });
        match handle
            .await
            .map_err(to_napi_error)?
            .map_err(to_napi_error)?
        {
            Sent::Ack | Sent::Response(_) => Ok(()),
        }
    }

    async fn send_batch_on_runtime(&self, messages: Vec<CanonicalMessage>) -> Result<()> {
        let publisher = self.publisher.clone();
        let count = messages.len();
        let handle = self
            .runtime
            .spawn(async move { publisher.send_batch(messages).await });
        match handle
            .await
            .map_err(to_napi_error)?
            .map_err(to_napi_error)?
        {
            SentBatch::Ack => Ok(()),
            // A `Partial` with no failures (e.g. request-reply responses) is a full success.
            SentBatch::Partial { failed, .. } if failed.is_empty() => Ok(()),
            SentBatch::Partial { failed, .. } => Err(to_napi_error(format!(
                "sendBatch: {} of {count} message(s) failed to publish. First error: {}",
                failed.len(),
                failed[0].1
            ))),
        }
    }

    async fn request_on_runtime(&self, message: CanonicalMessage) -> Result<NativeMessage> {
        let publisher = self.publisher.clone();
        let handle = self
            .runtime
            .spawn(async move { publisher.request(message).await });
        let response = handle
            .await
            .map_err(to_napi_error)?
            .map_err(to_napi_error)?;
        Ok(NativeMessage::from_canonical(&response))
    }
}

/// A pull-based consumer over any mq-bridge input endpoint.
///
/// `poll()` receives a batch of messages but does **not** acknowledge them;
/// call `commit()` once they have been durably handled to ack every batch
/// returned since the previous commit (advancing offsets / removing them from
/// the source). This manual-commit model gives at-least-once delivery across a
/// failed downstream load. The endpoint config decides durability (consumer vs
/// subscriber mode), exactly as it does for a route input.
///
/// Relationship to the Rust core: this is a boundary-friendly projection of the
/// core `MessageConsumer::receive_batch`, which is the low-level primitive. Two
/// differences are deliberate, both because a Rust commit closure cannot be
/// handed across the FFI boundary for JS to call later:
///   - `receive_batch` returns the messages *and* their commit closure together;
///     `poll()` returns only the messages and keeps the closure on the Rust side,
///     so committing becomes the separate `commit()` call.
///   - that closure accepts a per-message disposition vector (ack/nack/reject);
///     `poll()` + `commit()` only ack the whole batch.
/// `poll()` additionally layers a `timeoutMs` over `receive_batch` (which has no
/// timeout of its own). Native Rust code should use `receive_batch` directly — it
/// is strictly more expressive and needs no deferred-commit state.
#[napi]
pub struct Consumer {
    runtime: Arc<Runtime>,
    // `None` once `close()` has dropped the underlying consumer.
    consumer: Arc<tokio::sync::Mutex<Option<Box<dyn MessageConsumer>>>>,
    // Polled-but-uncommitted batches, keyed by a monotonic token. Ordered so
    // `commit()` acks them oldest-first; `ack(token)`/`nack(token)` address one.
    pending: Arc<Mutex<BTreeMap<u32, (BatchCommitFunc, usize)>>>,
    next_token: Arc<AtomicU32>,
    exhausted: Arc<AtomicBool>,
    // `true` for cumulative-ack transports (Kafka, …) where acking a later batch
    // implicitly acks earlier ones, so token acks must stay oldest-first.
    requires_order: bool,
}

#[napi]
impl Consumer {
    /// Build a consumer from a YAML or JSON config file. Accepts a `consumers:`
    /// document entry (with `name`) or a single bare endpoint body.
    #[napi(factory)]
    pub fn from_file(path: String, name: Option<String>) -> Result<Self> {
        let resolved = common::normalize_name(name.as_deref()).map(str::to_string);
        let endpoint = common::load_named_consumer(Path::new(&path), resolved.as_deref())
            .map_err(to_napi_error)?;
        Self::build(
            resolved.unwrap_or_else(common::default_route_name),
            endpoint,
        )
    }

    /// Build a consumer from an in-memory YAML or JSON string.
    #[napi(factory)]
    pub fn from_str(text: String, name: Option<String>) -> Result<Self> {
        let resolved = common::normalize_name(name.as_deref()).map(str::to_string);
        let value = serde_yaml_ng::from_str(&text)
            .context("failed to parse YAML config")
            .map_err(to_napi_error)?;
        let endpoint =
            common::named_consumer_from_value(value, resolved.as_deref()).map_err(to_napi_error)?;
        Self::build(
            resolved.unwrap_or_else(common::default_route_name),
            endpoint,
        )
    }

    /// Build a consumer from an in-memory mapping (e.g. a JS object). Omit `name`
    /// to treat the mapping as a single bare endpoint body.
    #[napi(factory)]
    pub fn from_config(config: JsonValue, name: Option<String>) -> Result<Self> {
        let resolved = common::normalize_name(name.as_deref()).map(str::to_string);
        let value = serde_yaml_ng::to_value(config)
            .context("failed to convert config mapping")
            .map_err(to_napi_error)?;
        let endpoint =
            common::named_consumer_from_value(value, resolved.as_deref()).map_err(to_napi_error)?;
        Self::build(
            resolved.unwrap_or_else(common::default_route_name),
            endpoint,
        )
    }

    /// Receive up to `max` messages without acking. Resolves to an empty array
    /// if `timeoutMs` milliseconds elapse with nothing received, or the source is
    /// exhausted (see `exhausted`). Omit `timeoutMs` to block until a message
    /// arrives. Acked by the next `commit()`.
    #[napi]
    pub async fn poll(
        &self,
        max: Option<u32>,
        timeout_ms: Option<u32>,
    ) -> Result<Vec<NativeMessage>> {
        match self.receive(max, timeout_ms).await? {
            Some((messages, _token)) => Ok(messages),
            None => Ok(Vec::new()),
        }
    }

    /// Like `poll()`, but also return the batch's token so it can be acked or
    /// nacked individually with `ack(token)` / `nack(token)` — the shape a `dlt`
    /// resource wants (`poll → yield → commit load package → ack(token)`).
    /// Resolves to `{ messages, token }`, with `token === null` on timeout or
    /// end-of-stream. Tokens stay outstanding until acked/nacked; `commit()`
    /// still acks every outstanding batch at once, so don't mix the two styles
    /// on one consumer.
    #[napi]
    pub async fn poll_batch(&self, max: Option<u32>, timeout_ms: Option<u32>) -> Result<PollBatch> {
        match self.receive(max, timeout_ms).await? {
            Some((messages, token)) => Ok(PollBatch {
                messages,
                token: Some(token),
            }),
            None => Ok(PollBatch {
                messages: Vec::new(),
                token: None,
            }),
        }
    }

    /// Acknowledge a single batch by the token from `pollBatch()`, advancing the
    /// consumer offset for just that batch. Rejects if the token is unknown
    /// (already acked/nacked, or never polled).
    #[napi]
    pub async fn ack(&self, token: u32) -> Result<()> {
        self.commit_one(token, MessageDisposition::Ack).await
    }

    /// Negatively acknowledge so the broker can redeliver. With a `token`, nacks
    /// just that batch; without one, nacks every outstanding batch (oldest
    /// first). On Kafka there is no per-message nack — this leaves the offset
    /// unadvanced, so redelivery happens on the next run/rebalance, not at once.
    #[napi]
    pub async fn nack(&self, token: Option<u32>) -> Result<()> {
        if let Some(token) = token {
            return self.commit_one(token, MessageDisposition::Nack).await;
        }
        // Nack all outstanding batches, oldest first.
        let pending: Vec<(u32, (BatchCommitFunc, usize))> =
            std::mem::take(&mut *self.lock_pending()?)
                .into_iter()
                .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let handle = self.runtime.spawn(async move {
            let mut iter = pending.into_iter();
            while let Some((_token, (commit, len))) = iter.next() {
                if let Err(err) = commit(vec![MessageDisposition::Nack; len]).await {
                    return (Some(err), iter.collect::<Vec<_>>());
                }
            }
            (None, Vec::new())
        });
        let (err, tail) = handle.await.map_err(to_napi_error)?;
        if !tail.is_empty() {
            let mut pending = self.lock_pending()?;
            for (token, entry) in tail {
                pending.insert(token, entry);
            }
        }
        match err {
            Some(err) => Err(to_napi_error(err)),
            None => Ok(()),
        }
    }

    /// Acknowledge every batch returned by `poll()` since the last `commit()`,
    /// advancing the consumer offset.
    ///
    /// Calling this is required, not optional. Without it the offset never
    /// advances (messages are re-delivered on the next run), most brokers stall
    /// once their unacknowledged/prefetch window fills, and uncommitted batches
    /// are held in memory so the process grows unbounded. To retry a failed
    /// batch, simply don't commit it — it will be redelivered.
    #[napi]
    pub async fn commit(&self) -> Result<()> {
        // Drain oldest-first; `BTreeMap` iterates in token order.
        let commits: Vec<(u32, (BatchCommitFunc, usize))> =
            std::mem::take(&mut *self.lock_pending()?)
                .into_iter()
                .collect();
        if commits.is_empty() {
            return Ok(());
        }
        let handle = self.runtime.spawn(async move {
            let mut iter = commits.into_iter();
            while let Some((_token, (commit, len))) = iter.next() {
                if let Err(err) = commit(vec![MessageDisposition::Ack; len]).await {
                    // Hand back the batches we never attempted so they can be retried.
                    return (Some(err), iter.collect::<Vec<_>>());
                }
            }
            (None, Vec::new())
        });
        let (err, tail) = handle.await.map_err(to_napi_error)?;
        if !tail.is_empty() {
            // Re-insert the un-attempted batches under their original tokens.
            let mut pending = self.lock_pending()?;
            for (token, entry) in tail {
                pending.insert(token, entry);
            }
        }
        match err {
            Some(err) => Err(to_napi_error(err)),
            None => Ok(()),
        }
    }

    /// `true` once the source has signalled end-of-stream (e.g. a fully drained
    /// file). Streaming brokers never set this.
    #[napi(getter)]
    pub fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::SeqCst)
    }

    /// Resolve to a status snapshot for the underlying endpoint: `healthy`,
    /// `target`, optional `pending` (broker backlog/lag where the transport
    /// reports it — Kafka offset lag, AMQP queue depth, NATS JetStream
    /// `numPending`), optional `capacity`/`error`, and `details`. `pending === 0`
    /// is a precise "caught up" signal on those transports; it is absent where
    /// the broker exposes no backlog (core NATS, MQTT). Point-in-time snapshot.
    #[napi]
    pub async fn status(&self) -> Result<JsonValue> {
        let consumer = Arc::clone(&self.consumer);
        let handle = self.runtime.spawn(async move {
            let guard = consumer.lock().await;
            let consumer = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("consumer is closed"))?;
            serde_json::to_value(consumer.status().await).map_err(anyhow::Error::from)
        });
        handle.await.map_err(to_napi_error)?.map_err(to_napi_error)
    }

    /// Release the underlying consumer connection. Idempotent. After this,
    /// `poll()` and `status()` reject. GC'd JS has no deterministic drop, so
    /// closing explicitly is how the broker connection is freed promptly.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let consumer = Arc::clone(&self.consumer);
        let handle = self.runtime.spawn(async move {
            let taken = consumer.lock().await.take();
            if let Some(mut consumer) = taken {
                consumer.close().await?;
            }
            Ok::<(), anyhow::Error>(())
        });
        handle.await.map_err(to_napi_error)?.map_err(to_napi_error)
    }
}

impl Consumer {
    fn build(name: String, endpoint: Endpoint) -> Result<Self> {
        let runtime = Arc::new(common::build_runtime().map_err(to_napi_error)?);
        let consumer = runtime
            .block_on(core::endpoints::create_consumer_from_route(
                &name, &endpoint,
            ))
            .map_err(to_napi_error)?;
        let requires_order = consumer.commit_requires_order();
        Ok(Self {
            runtime,
            consumer: Arc::new(tokio::sync::Mutex::new(Some(consumer))),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            next_token: Arc::new(AtomicU32::new(0)),
            exhausted: Arc::new(AtomicBool::new(false)),
            requires_order,
        })
    }

    fn lock_pending(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<u32, (BatchCommitFunc, usize)>>> {
        self.pending
            .lock()
            .map_err(|_| Error::from_reason("Consumer commit lock poisoned"))
    }

    /// Receive up to `max` messages, registering the batch's commit closure under
    /// a fresh token. Resolves to `None` on timeout or end-of-stream, otherwise
    /// the messages and their token. Shared by `poll()` and `pollBatch()`.
    async fn receive(
        &self,
        max: Option<u32>,
        timeout_ms: Option<u32>,
    ) -> Result<Option<(Vec<NativeMessage>, u32)>> {
        let consumer = Arc::clone(&self.consumer);
        let exhausted = Arc::clone(&self.exhausted);
        let pending = Arc::clone(&self.pending);
        let next_token = Arc::clone(&self.next_token);
        let max = max.unwrap_or(256).max(1) as usize;
        // The token is allocated and the batch registered while still holding the
        // consumer lock, so token order matches receive order even when several
        // polls are in flight concurrently.
        let handle = self.runtime.spawn(async move {
            let mut guard = consumer.lock().await;
            let consumer = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("consumer is closed"))?;
            let recv = consumer.receive_batch(max);
            let batch = if let Some(timeout_ms) = timeout_ms {
                match tokio::time::timeout(Duration::from_millis(timeout_ms as u64), recv).await {
                    Ok(result) => result,
                    Err(_) => return Ok(None),
                }
            } else {
                recv.await
            };
            match batch {
                Ok(batch) => {
                    let count = batch.messages.len();
                    if count == 0 {
                        return Ok(None);
                    }
                    let token = next_token.fetch_add(1, Ordering::SeqCst);
                    let mut pending = pending
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Consumer commit lock poisoned"))?;
                    // `u32` tokens stay JS-number friendly; fail fast if the
                    // counter wraps onto a batch that is still outstanding rather
                    // than silently overwriting its commit closure.
                    if pending.contains_key(&token) {
                        return Err(anyhow::anyhow!(
                            "batch token space exhausted (u32 token counter wrapped with batches still outstanding)"
                        ));
                    }
                    pending.insert(token, (batch.commit, count));
                    drop(pending);
                    Ok(Some((batch.messages, token)))
                }
                Err(core::errors::ConsumerError::EndOfStream) => {
                    exhausted.store(true, Ordering::SeqCst);
                    Ok(None)
                }
                Err(err) => Err(anyhow::Error::from(err)),
            }
        });
        let outcome = handle
            .await
            .map_err(to_napi_error)?
            .map_err(to_napi_error)?;
        let Some((messages, token)) = outcome else {
            return Ok(None);
        };
        let messages: Vec<NativeMessage> =
            messages.iter().map(NativeMessage::from_canonical).collect();
        Ok(Some((messages, token)))
    }

    /// Run one batch's commit closure with a uniform disposition, removing it from
    /// `pending`. Used by `ack(token)` and `nack(token)`.
    async fn commit_one(&self, token: u32, disposition: MessageDisposition) -> Result<()> {
        let entry = {
            let mut pending = self.lock_pending()?;
            // On cumulative-ack transports, acking a later batch implicitly acks
            // the earlier ones, so an out-of-order ack would silently drop them.
            // Reject it (the token stays outstanding) instead of committing.
            if matches!(disposition, MessageDisposition::Ack) && self.requires_order {
                if let Some((&oldest, _)) = pending.iter().next() {
                    if token != oldest && pending.contains_key(&token) {
                        return Err(Error::from_reason(format!(
                            "cannot ack batch token {token} before older outstanding token {oldest}: this transport commits cumulatively, so acks must follow receive order (ack older batches first, or use commit())"
                        )));
                    }
                }
            }
            pending.remove(&token)
        };
        let Some((commit, len)) = entry else {
            return Err(Error::from_reason(format!(
                "unknown batch token {token} (already committed, or never polled)"
            )));
        };
        let handle = self
            .runtime
            .spawn(async move { commit(vec![disposition; len]).await });
        // On failure the closure consumed the batch; it cannot be retried by token.
        handle.await.map_err(to_napi_error)?.map_err(to_napi_error)
    }
}

fn build_message(
    payload: Vec<u8>,
    metadata: Option<HashMap<String, String>>,
    id: Option<&str>,
) -> Result<CanonicalMessage> {
    let message_id = id.map(parse_message_id).transpose()?;
    let mut message = CanonicalMessage::new(payload, message_id);
    if let Some(metadata) = metadata {
        message.metadata.extend(metadata);
    }
    Ok(message)
}

fn json_input_to_canonical(
    data: JsonValue,
    metadata: Option<HashMap<String, String>>,
    id: Option<&str>,
) -> Result<CanonicalMessage> {
    build_message(
        serde_json::to_vec(&data).map_err(to_napi_error)?,
        metadata,
        id,
    )
}

/// Resolve a user-supplied id string. A UUID, `0x` hex literal or decimal integer parses
/// directly; anything else is hashed to a stable id, so this no longer throws for arbitrary
/// strings. The `Result` is kept for the API's shape.
fn parse_message_id(id: &str) -> Result<u128> {
    core::canonical_message::message_id_from_str(id).map_err(Error::from_reason)
}

fn active_route_names() -> &'static Mutex<HashSet<String>> {
    static ACTIVE_ROUTE_NAMES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE_ROUTE_NAMES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lock_active_route_names() -> Result<std::sync::MutexGuard<'static, HashSet<String>>> {
    active_route_names()
        .lock()
        .map_err(|_| Error::from_reason("Active route registry lock poisoned"))
}

/// Wait for an explicit `stop()` or for the route to end on its own — a drained
/// source under `exit_on_empty`, an exhausted stream, or a permanent failure.
///
/// Returns the terminal outcome when the route ended by itself, `None` when a
/// stop or a process-wide shutdown was requested. Without this a drain-then-exit
/// route would leave `join()` waiting forever on a stop signal that never arrives.
async fn wait_for_stop_or_end(
    name: &str,
    stop_rx: oneshot::Receiver<()>,
) -> Option<core::RouteOutcome> {
    tokio::pin!(stop_rx);
    loop {
        tokio::select! {
            _ = &mut stop_rx => return None,
            _ = core::shutdown::shutdown_requested() => return None,
            _ = tokio::time::sleep(ROUTE_END_POLL_INTERVAL) => {
                if let Some(outcome) = core::route_outcome(name) {
                    return Some(outcome);
                }
            }
        }
    }
}

/// Turn a self-terminated route's outcome into the result `join()` reports. A
/// clean drain is success; a permanent failure carries the cause from the
/// route's status.
fn outcome_to_result(name: &str, outcome: Option<core::RouteOutcome>) -> Result<()> {
    if outcome == Some(core::RouteOutcome::Failed) {
        let cause = core::route_status(name)
            .and_then(|status| status.error)
            .unwrap_or_else(|| "permanent error".to_string());
        return Err(Error::from_reason(format!(
            "Route '{name}' failed: {cause}"
        )));
    }
    Ok(())
}

fn finish_run(run_state: &Arc<Mutex<RouteRunState>>, name: &str) {
    if let Ok(mut active) = active_route_names().lock() {
        active.remove(name);
    }
    if let Ok(mut state) = run_state.lock() {
        state.running = false;
        state.stop_tx = None;
    }
}

fn to_napi_error(err: impl std::fmt::Display) -> Error {
    Error::from_reason(err.to_string())
}
