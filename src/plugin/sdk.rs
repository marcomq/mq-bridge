//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Author an endpoint plugin with normal Rust traits instead of raw FFI.
//!
//! An endpoint author implements the usual mq-bridge contracts —
//! [`CustomEndpointFactory`], [`MessageConsumer`]
//! and [`MessagePublisher`] — and exports them through the stable C ABI with one
//! macro:
//!
//! ```ignore
//! #[derive(Debug, Default)]
//! struct PulsarFactory;
//!
//! impl mq_bridge::traits::CustomEndpointFactory for PulsarFactory { /* ... */ }
//!
//! mq_bridge::export_endpoint_plugin! {
//!     name: "pulsar",
//!     factory: PulsarFactory,
//! }
//! ```
//!
//! Depend on mq-bridge with the `plugin-sdk` feature and build the crate as both
//! a normal library and a plugin:
//!
//! ```toml
//! [lib]
//! crate-type = ["rlib", "cdylib"]
//! ```
//!
//! The `rlib` lets Rust users link the endpoint directly (and lets you test it
//! as plain Rust); the `cdylib` is what [`load_endpoint_plugin`](super::load_endpoint_plugin)
//! opens, and what the Python and Node.js packages ship.
//!
//! This module takes care of everything the ABI requires: panic containment,
//! buffer allocation and release, handle lifetimes, error translation (retryable
//! / permanent / connection / end-of-stream), and the plugin's own async
//! runtime, since futures cannot cross the boundary. Acknowledgement timing is
//! passed through unchanged: the host's batch commit reaches your
//! [`ReceivedBatch`](crate::ReceivedBatch) commit function, so nothing is acked
//! before the route says so.
//!
//! The factory type must implement `Default` — the ABI constructs it with no
//! arguments — and be `Send + Sync`. Configure endpoints through the route's
//! `config` object, not through factory state.
//!
//! ABI v1 limits: a batch is *reported* all-or-nothing (no per-message publish
//! responses, so no request/reply), and `MessageDisposition::Reply`
//! acknowledges the source message.
//!
//! All-or-nothing is about the status, not about what reached the sink. A
//! [`SentBatch::Partial`] may already have delivered some of its messages, yet
//! only the first failure's class crosses the ABI — and if that class is
//! retryable, the host retries the *whole* batch and the delivered messages are
//! duplicated. Under ABI v1, publish into an idempotent sink or do not return
//! partial results.
//!
//! See [`conformance`](super::conformance) for the suite to run against your
//! endpoint both linked directly and loaded as a plugin.

use std::ffi::c_void;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::Context;
use futures::FutureExt;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use crate::errors::{ConsumerError, ProcessingError};
use crate::plugin::message::{from_abi, AbiMessages};
use crate::support::plugin_abi::{
    MqbBatchHandle, MqbBuffer, MqbConsumerHandle, MqbFactoryHandle, MqbFilterHandle, MqbMessage,
    MqbMiddlewareHandle, MqbPluginVTable, MqbPublisherHandle, MqbSlice, MqbStatus,
    MQB_CAP_CONSUMER, MQB_CAP_MIDDLEWARE, MQB_CAP_PUBLISHER, MQB_DISPOSITION_NACK,
    MQB_END_OF_STREAM, MQB_ERR_CONNECTION, MQB_ERR_INVALID_CONFIG, MQB_ERR_PANIC,
    MQB_ERR_PERMANENT, MQB_ERR_RETRYABLE, MQB_MIDDLEWARE_RECEIVE, MQB_OK, MQB_PLUGIN_ABI_MAJOR,
    MQB_PLUGIN_ABI_MINOR,
};
use crate::traits::{
    BatchCommitFunc, CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher,
};
use crate::{CanonicalMessage, SentBatch};

/// Builds the runtime that drives every endpoint this plugin creates.
fn build_runtime() -> anyhow::Result<Arc<Runtime>> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().thread_name("mqb-plugin");
    if let Some(threads) = std::env::var("MQB_PLUGIN_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
    {
        builder.worker_threads(threads);
    }
    builder
        .build()
        .map(Arc::new)
        .context("failed to start the plugin's async runtime")
}

/// Why an ABI call never got a result from the plugin's runtime.
///
/// Distinguished from an ordinary endpoint error because the two map to
/// different ABI statuses: a panic is a bug and must not be retried, while a
/// shut-down runtime is a connection-level fault.
enum TaskFailure {
    Panicked(String),
    Lost,
    TimedOut(std::time::Duration),
}

impl TaskFailure {
    fn status(&self) -> MqbStatus {
        match self {
            TaskFailure::Panicked(_) => MQB_ERR_PANIC,
            TaskFailure::Lost | TaskFailure::TimedOut(_) => MQB_ERR_CONNECTION,
        }
    }
}

impl std::fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskFailure::Panicked(text) => write!(f, "plugin panicked: {text}"),
            TaskFailure::Lost => f.write_str("plugin task did not complete: its runtime shut down"),
            TaskFailure::TimedOut(limit) => write!(
                f,
                "plugin task did not complete within {limit:?} \
                 (MQB_PLUGIN_CALL_TIMEOUT_SECS); it is still running"
            ),
        }
    }
}

/// Optional bound on how long an ABI call waits for the plugin's runtime.
///
/// Unset by default: a `receive_batch` may legitimately park until a message
/// arrives, so a blanket timeout would turn a healthy idle source into an error.
fn call_timeout() -> Option<std::time::Duration> {
    static TIMEOUT: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        std::env::var("MQB_PLUGIN_CALL_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .map(std::time::Duration::from_secs)
    })
}

/// Runs `future` on the plugin's runtime and blocks until it completes.
///
/// Deliberately spawns instead of calling `Runtime::block_on`: the caller is one
/// of the host's blocking threads, which still carries the host runtime's
/// context, and `block_on` panics there. A panic inside `future` is caught here
/// so it cannot unwind past the ABI boundary either.
///
/// The wait is unbounded unless `MQB_PLUGIN_CALL_TIMEOUT_SECS` is set. On a
/// timeout the call returns [`MQB_ERR_CONNECTION`] but the spawned future keeps
/// running: it is never cancelled, so anything it does must be cancel-safe or
/// self-bounded (its own read/connect timeouts), or the operation is simply
/// reported as failed while still in flight.
fn block_on<T: Send + 'static>(
    runtime: &Runtime,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, TaskFailure> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let result = AssertUnwindSafe(future).catch_unwind().await;
        let _ = tx.send(result.map_err(|payload| panic_text(payload.as_ref())));
    });
    let received = match call_timeout() {
        Some(limit) => rx.recv_timeout(limit).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => TaskFailure::TimedOut(limit),
            std::sync::mpsc::RecvTimeoutError::Disconnected => TaskFailure::Lost,
        }),
        None => rx.recv().map_err(|_| TaskFailure::Lost),
    };
    match received {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(text)) => Err(TaskFailure::Panicked(text)),
        Err(failure) => Err(failure),
    }
}

// ---------------------------------------------------------------- plugin state

struct FactoryState {
    factory: Arc<dyn CustomEndpointFactory>,
    runtime: Arc<Runtime>,
}

struct ConsumerState {
    consumer: Arc<Mutex<Box<dyn MessageConsumer>>>,
    runtime: Arc<Runtime>,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Read once at creation: querying it later could deadlock against an
    /// in-flight `receive_batch` that holds the consumer lock.
    commit_requires_order: bool,
}

struct PublisherState {
    publisher: Arc<dyn MessagePublisher>,
    runtime: Arc<Runtime>,
}

struct BatchState {
    /// Owns the memory the handed-out ABI array points into.
    messages: AbiMessages,
    commit: Option<BatchCommitFunc>,
    runtime: Arc<Runtime>,
}

// ------------------------------------------------------------------- utilities

fn into_handle<T>(value: T) -> *mut c_void {
    Box::into_raw(Box::new(value)).cast()
}

/// # Safety
/// `handle` must be a live handle previously produced by [`into_handle`] for `T`.
unsafe fn borrow<'a, T>(handle: *mut c_void) -> Option<&'a T> {
    (!handle.is_null()).then(|| unsafe { &*handle.cast::<T>() })
}

/// # Safety
/// `handle` must be a live handle for `T`, and must not be used afterwards.
unsafe fn reclaim<T>(handle: *mut c_void) -> Option<Box<T>> {
    (!handle.is_null()).then(|| unsafe { Box::from_raw(handle.cast::<T>()) })
}

fn buffer_from(message: impl AsRef<str>) -> MqbBuffer {
    let mut bytes = message.as_ref().as_bytes().to_vec();
    let buffer = MqbBuffer {
        ptr: bytes.as_mut_ptr(),
        len: bytes.len(),
        cap: bytes.capacity(),
    };
    std::mem::forget(bytes);
    buffer
}

/// Writes error text for the host to read and release. Never overwrites an
/// already-populated slot, and tolerates a null out-parameter.
unsafe fn set_error(out: *mut MqbBuffer, message: impl AsRef<str>) {
    if out.is_null() {
        return;
    }
    if !unsafe { (*out).is_empty() } {
        unsafe { buffer_free(*out) };
    }
    unsafe { *out = buffer_from(message) };
}

/// Runs an ABI call body, turning any panic into [`MQB_ERR_PANIC`] so unwinding
/// never crosses the shared-library boundary.
fn guarded(err: *mut MqbBuffer, body: impl FnOnce() -> MqbStatus) -> MqbStatus {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
            unsafe {
                set_error(
                    err,
                    format!("plugin panicked: {}", panic_text(payload.as_ref())),
                )
            };
            MQB_ERR_PANIC
        }
    }
}

/// Same as [`guarded`] for functions that cannot report an error.
fn guarded_unit(body: impl FnOnce()) {
    if std::panic::catch_unwind(AssertUnwindSafe(body)).is_err() {
        tracing::error!("panic caught at the mq-bridge plugin ABI boundary; ignoring");
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// # Safety
/// `slice` must satisfy the ABI's borrow rules for the current call.
unsafe fn read_str(slice: MqbSlice, field: &str) -> Result<String, String> {
    std::str::from_utf8(unsafe { slice.as_bytes() })
        .map(str::to_owned)
        .map_err(|_| format!("`{field}` passed to the plugin is not valid UTF-8"))
}

/// # Safety
/// As [`read_str`]. An empty slice is read as an empty JSON object.
unsafe fn read_config(slice: MqbSlice) -> Result<serde_json::Value, String> {
    if slice.len == 0 {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    serde_json::from_slice(unsafe { slice.as_bytes() })
        .map_err(|err| format!("endpoint configuration is not valid JSON: {err}"))
}

/// Reports a runtime-level failure (a panic, or a runtime that went away) with
/// the status matching its cause.
unsafe fn task_failed(err: *mut MqbBuffer, failure: TaskFailure) -> MqbStatus {
    unsafe { set_error(err, failure.to_string()) };
    failure.status()
}

fn consumer_status(err: &ConsumerError) -> MqbStatus {
    match err {
        ConsumerError::EndOfStream => MQB_END_OF_STREAM,
        ConsumerError::Permanent(_) => MQB_ERR_PERMANENT,
        ConsumerError::Gap { .. } => MQB_ERR_PERMANENT,
        ConsumerError::Connection(_) => MQB_ERR_CONNECTION,
    }
}

fn processing_status(err: &ProcessingError) -> MqbStatus {
    match err {
        ProcessingError::Retryable(_) => MQB_ERR_RETRYABLE,
        ProcessingError::NonRetryable(_) => MQB_ERR_PERMANENT,
        ProcessingError::Connection(_) => MQB_ERR_CONNECTION,
    }
}

// ------------------------------------------------------------------- factory

/// Creates the factory. Generic so the export macro can name the author's type;
/// every other ABI function works through trait objects.
unsafe extern "C" fn factory_create<F>(out: *mut MqbFactoryHandle, err: *mut MqbBuffer) -> MqbStatus
where
    F: CustomEndpointFactory + Default + 'static,
{
    guarded(err, || {
        let runtime = match build_runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                unsafe { set_error(err, format!("{error:#}")) };
                return MQB_ERR_PERMANENT;
            }
        };
        let state = FactoryState {
            factory: Arc::new(F::default()),
            runtime,
        };
        unsafe { *out = MqbFactoryHandle(into_handle(state)) };
        MQB_OK
    })
}

unsafe extern "C" fn factory_free(factory: MqbFactoryHandle) {
    guarded_unit(|| drop(unsafe { reclaim::<FactoryState>(factory.0) }));
}

unsafe extern "C" fn buffer_free(buffer: MqbBuffer) {
    guarded_unit(|| {
        if !buffer.ptr.is_null() {
            drop(unsafe { Vec::from_raw_parts(buffer.ptr, buffer.len, buffer.cap) });
        }
    });
}

// ------------------------------------------------------------------ consumer

unsafe extern "C" fn consumer_create(
    factory: MqbFactoryHandle,
    route_name: MqbSlice,
    config_json: MqbSlice,
    out: *mut MqbConsumerHandle,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<FactoryState>(factory.0) }) else {
            unsafe { set_error(err, "consumer_create called with a null factory handle") };
            return MQB_ERR_PERMANENT;
        };
        let (route, config) =
            match unsafe { (read_str(route_name, "route_name"), read_config(config_json)) } {
                (Ok(route), Ok(config)) => (route, config),
                (Err(message), _) | (_, Err(message)) => {
                    unsafe { set_error(err, message) };
                    return MQB_ERR_INVALID_CONFIG;
                }
            };

        let factory = Arc::clone(&state.factory);
        let runtime = Arc::clone(&state.runtime);
        let created = block_on(&runtime, async move {
            // Creation and connection fail for different reasons, so they are
            // reported separately: a bad config is never worth reconnecting.
            let consumer = factory
                .create_consumer(&route, &config)
                .await
                .map_err(|error| (MQB_ERR_INVALID_CONFIG, error))?;
            // The host route awaits this hook for endpoints it builds itself;
            // behind the ABI only the plugin can run it.
            if let Some(hook) = consumer.on_connect_hook() {
                hook.await.map_err(|error| (MQB_ERR_CONNECTION, error))?;
            }
            let commit_requires_order = consumer.commit_requires_order();
            Ok::<_, (MqbStatus, anyhow::Error)>((consumer, commit_requires_order))
        });
        let (consumer, commit_requires_order) = match created {
            Ok(Ok(created)) => created,
            Ok(Err((status, error))) => {
                unsafe { set_error(err, format!("{error:#}")) };
                return status;
            }
            Err(failure) => return unsafe { task_failed(err, failure) },
        };

        let (shutdown, _) = tokio::sync::watch::channel(false);
        unsafe {
            *out = MqbConsumerHandle(into_handle(ConsumerState {
                consumer: Arc::new(Mutex::new(consumer)),
                runtime,
                shutdown,
                commit_requires_order,
            }))
        };
        MQB_OK
    })
}

unsafe extern "C" fn consumer_receive_batch(
    consumer: MqbConsumerHandle,
    max_messages: usize,
    out_batch: *mut MqbBatchHandle,
    out_messages: *mut *const MqbMessage,
    out_len: *mut usize,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<ConsumerState>(consumer.0) }) else {
            unsafe { set_error(err, "consumer_receive_batch called with a null handle") };
            return MQB_ERR_PERMANENT;
        };
        let shared = Arc::clone(&state.consumer);
        let mut shutdown = state.shutdown.subscribe();
        let received = block_on(&state.runtime, async move {
            tokio::select! {
                batch = async { shared.lock().await.receive_batch(max_messages).await } => Some(batch),
                _ = shutdown.wait_for(|closed| *closed) => None,
            }
        });
        let batch = match received {
            Ok(Some(Ok(batch))) => batch,
            Ok(Some(Err(error))) => {
                let status = consumer_status(&error);
                unsafe { set_error(err, format!("{error:#}")) };
                return status;
            }
            Ok(None) => return MQB_END_OF_STREAM,
            Err(failure) => return unsafe { task_failed(err, failure) },
        };

        let state = Box::new(BatchState {
            messages: AbiMessages::new(batch.messages),
            commit: Some(batch.commit),
            runtime: Arc::clone(&state.runtime),
        });
        unsafe {
            *out_messages = state.messages.as_ptr();
            *out_len = state.messages.len();
            *out_batch = MqbBatchHandle(Box::into_raw(state).cast());
        }
        MQB_OK
    })
}

unsafe extern "C" fn consumer_commit_requires_order(consumer: MqbConsumerHandle) -> u8 {
    // Defaults to the safe answer (ordered) if the handle is unusable.
    unsafe { borrow::<ConsumerState>(consumer.0) }
        .map_or(1, |state| u8::from(state.commit_requires_order))
}

unsafe extern "C" fn consumer_set_exit_on_empty(consumer: MqbConsumerHandle, exit_on_empty: u8) {
    guarded_unit(|| {
        let Some(state) = (unsafe { borrow::<ConsumerState>(consumer.0) }) else {
            return;
        };
        let shared = Arc::clone(&state.consumer);
        let _ = block_on(&state.runtime, async move {
            shared.lock().await.set_exit_on_empty(exit_on_empty != 0);
        });
    })
}

unsafe extern "C" fn consumer_close(consumer: MqbConsumerHandle, err: *mut MqbBuffer) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<ConsumerState>(consumer.0) }) else {
            return MQB_OK;
        };
        let _ = state.shutdown.send(true);
        let shared = Arc::clone(&state.consumer);
        let closed = block_on(
            &state.runtime,
            async move { shared.lock().await.close().await },
        );
        match closed {
            Ok(Ok(())) => MQB_OK,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                MQB_ERR_RETRYABLE
            }
            Err(failure) => unsafe { task_failed(err, failure) },
        }
    })
}

unsafe extern "C" fn consumer_free(consumer: MqbConsumerHandle) {
    guarded_unit(|| drop(unsafe { reclaim::<ConsumerState>(consumer.0) }));
}

// --------------------------------------------------------------------- batch

unsafe extern "C" fn batch_commit(
    batch: MqbBatchHandle,
    dispositions: *const u8,
    len: usize,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(mut state) = (unsafe { reclaim::<BatchState>(batch.0) }) else {
            unsafe { set_error(err, "batch_commit called with a null handle") };
            return MQB_ERR_PERMANENT;
        };
        let Some(commit) = state.commit.take() else {
            unsafe { set_error(err, "batch_commit called twice for the same batch") };
            return MQB_ERR_PERMANENT;
        };
        // Same check the middleware side makes: a mismatched count would
        // silently ack or drop messages instead of failing loudly.
        let expected = state.messages.len();
        if len != expected {
            unsafe {
                set_error(
                    err,
                    format!(
                        "batch_commit got {len} dispositions for a batch of {expected} messages"
                    ),
                )
            };
            return MQB_ERR_PERMANENT;
        }
        if dispositions.is_null() && len != 0 {
            unsafe { set_error(err, "batch_commit got a null disposition array") };
            return MQB_ERR_PERMANENT;
        }
        let codes: &[u8] = if len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(dispositions, len) }
        };
        let dispositions: Vec<MessageDisposition> = codes
            .iter()
            .map(|code| {
                if *code == MQB_DISPOSITION_NACK {
                    MessageDisposition::Nack
                } else {
                    MessageDisposition::Ack
                }
            })
            .collect();

        match block_on(&state.runtime, commit(dispositions)) {
            Ok(Ok(())) => MQB_OK,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                MQB_ERR_RETRYABLE
            }
            Err(failure) => unsafe { task_failed(err, failure) },
        }
    })
}

unsafe extern "C" fn batch_free(batch: MqbBatchHandle) {
    // Dropping the commit closure without calling it acknowledges nothing,
    // which is what an uncommitted batch must do.
    guarded_unit(|| drop(unsafe { reclaim::<BatchState>(batch.0) }));
}

// ----------------------------------------------------------------- publisher

unsafe extern "C" fn publisher_create(
    factory: MqbFactoryHandle,
    route_name: MqbSlice,
    config_json: MqbSlice,
    out: *mut MqbPublisherHandle,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<FactoryState>(factory.0) }) else {
            unsafe { set_error(err, "publisher_create called with a null factory handle") };
            return MQB_ERR_PERMANENT;
        };
        let (route, config) =
            match unsafe { (read_str(route_name, "route_name"), read_config(config_json)) } {
                (Ok(route), Ok(config)) => (route, config),
                (Err(message), _) | (_, Err(message)) => {
                    unsafe { set_error(err, message) };
                    return MQB_ERR_INVALID_CONFIG;
                }
            };

        let factory = Arc::clone(&state.factory);
        let runtime = Arc::clone(&state.runtime);
        let created = block_on(&runtime, async move {
            let publisher = factory
                .create_publisher(&route, &config)
                .await
                .map_err(|error| (MQB_ERR_INVALID_CONFIG, error))?;
            if let Some(hook) = publisher.on_connect_hook() {
                hook.await.map_err(|error| (MQB_ERR_CONNECTION, error))?;
            }
            Ok::<_, (MqbStatus, anyhow::Error)>(publisher)
        });
        let publisher = match created {
            Ok(Ok(publisher)) => publisher,
            Ok(Err((status, error))) => {
                unsafe { set_error(err, format!("{error:#}")) };
                return status;
            }
            Err(failure) => return unsafe { task_failed(err, failure) },
        };

        unsafe {
            *out = MqbPublisherHandle(into_handle(PublisherState {
                publisher: Arc::from(publisher),
                runtime,
            }))
        };
        MQB_OK
    })
}

unsafe extern "C" fn publisher_send_batch(
    publisher: MqbPublisherHandle,
    messages: *const MqbMessage,
    len: usize,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<PublisherState>(publisher.0) }) else {
            unsafe { set_error(err, "publisher_send_batch called with a null handle") };
            return MQB_ERR_PERMANENT;
        };
        let messages: Vec<CanonicalMessage> = unsafe { from_abi(messages, len) };
        let shared = Arc::clone(&state.publisher);
        let sent = block_on(
            &state.runtime,
            async move { shared.send_batch(messages).await },
        );
        match sent {
            Ok(Ok(SentBatch::Ack)) => MQB_OK,
            // The v1 ABI is all-or-nothing per batch: report the first failure's
            // class so the host keeps the retry decision it would have made.
            Ok(Ok(SentBatch::Partial { failed, .. })) => match failed.into_iter().next() {
                None => MQB_OK,
                Some((_, error)) => {
                    let status = processing_status(&error);
                    unsafe { set_error(err, format!("{error:#}")) };
                    status
                }
            },
            Ok(Err(error)) => {
                let status = processing_status(&error);
                unsafe { set_error(err, format!("{error:#}")) };
                status
            }
            Err(failure) => unsafe { task_failed(err, failure) },
        }
    })
}

unsafe extern "C" fn publisher_flush(
    publisher: MqbPublisherHandle,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<PublisherState>(publisher.0) }) else {
            return MQB_OK;
        };
        let shared = Arc::clone(&state.publisher);
        match block_on(&state.runtime, async move { shared.flush().await }) {
            Ok(Ok(())) => MQB_OK,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                MQB_ERR_RETRYABLE
            }
            Err(failure) => unsafe { task_failed(err, failure) },
        }
    })
}

unsafe extern "C" fn publisher_close(
    publisher: MqbPublisherHandle,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<PublisherState>(publisher.0) }) else {
            return MQB_OK;
        };
        let shared = Arc::clone(&state.publisher);
        let closed = block_on(&state.runtime, async move {
            shared.flush().await?;
            if let Some(hook) = shared.on_disconnect_hook() {
                hook.await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        match closed {
            Ok(Ok(())) => MQB_OK,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                MQB_ERR_RETRYABLE
            }
            Err(failure) => unsafe { task_failed(err, failure) },
        }
    })
}

unsafe extern "C" fn publisher_free(publisher: MqbPublisherHandle) {
    guarded_unit(|| drop(unsafe { reclaim::<PublisherState>(publisher.0) }));
}

// ---------------------------------------------------------------- middleware

/// A middleware, as a plugin sees it: a batch goes in, the same batch comes
/// back with entries rewritten or dropped.
///
/// A middleware never touches the endpoint it wraps — the host keeps that
/// wrapper — so only messages cross the ABI. Return **exactly one entry per
/// input message, in order**: `Some(message)` keeps it (rewritten or not),
/// `None` drops it. The host acknowledges dropped messages on your behalf, so a
/// dropped message is not redelivered.
#[async_trait::async_trait]
pub trait BatchFilter: Send + Sync + 'static {
    /// Applied on an input endpoint, after the source produced the batch.
    async fn on_receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        Ok(messages.into_iter().map(Some).collect())
    }

    /// Applied on an output endpoint, before the sink receives the batch.
    async fn on_send(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        Ok(messages.into_iter().map(Some).collect())
    }
}

/// Creates one [`BatchFilter`] per route and side.
#[async_trait::async_trait]
pub trait MiddlewareFactory: Default + Send + Sync + 'static {
    async fn create(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BatchFilter>>;
}

/// Stand-in for a plugin that exports no middleware.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct NoMiddleware;

#[async_trait::async_trait]
impl MiddlewareFactory for NoMiddleware {
    async fn create(&self, _: &str, _: &serde_json::Value) -> anyhow::Result<Box<dyn BatchFilter>> {
        Err(anyhow::anyhow!("this plugin exports no middleware"))
    }
}

/// Stand-in for a plugin that exports no endpoint.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct NoEndpoint;

impl CustomEndpointFactory for NoEndpoint {}

struct MiddlewareState {
    filter: Arc<dyn BatchFilter>,
    /// `MQB_MIDDLEWARE_RECEIVE` or `MQB_MIDDLEWARE_SEND`, fixed at creation.
    side: u8,
    runtime: Arc<Runtime>,
}

struct FilterResultState {
    /// Parallel to the input: dropped entries hold an empty placeholder the
    /// host never reads.
    messages: AbiMessages,
    kept: Vec<u8>,
}

unsafe extern "C" fn middleware_create<M>(
    factory: MqbFactoryHandle,
    route_name: MqbSlice,
    config_json: MqbSlice,
    side: u8,
    out: *mut MqbMiddlewareHandle,
    err: *mut MqbBuffer,
) -> MqbStatus
where
    M: MiddlewareFactory,
{
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<FactoryState>(factory.0) }) else {
            unsafe { set_error(err, "middleware_create called with a null factory handle") };
            return MQB_ERR_PERMANENT;
        };
        let (route, config) =
            match unsafe { (read_str(route_name, "route_name"), read_config(config_json)) } {
                (Ok(route), Ok(config)) => (route, config),
                (Err(message), _) | (_, Err(message)) => {
                    unsafe { set_error(err, message) };
                    return MQB_ERR_INVALID_CONFIG;
                }
            };

        let runtime = Arc::clone(&state.runtime);
        let created = block_on(&runtime, async move {
            M::default().create(&route, &config).await
        });
        let filter = match created {
            Ok(Ok(filter)) => filter,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                return MQB_ERR_INVALID_CONFIG;
            }
            Err(failure) => return unsafe { task_failed(err, failure) },
        };

        unsafe {
            *out = MqbMiddlewareHandle(into_handle(MiddlewareState {
                filter: Arc::from(filter),
                side,
                runtime,
            }))
        };
        MQB_OK
    })
}

unsafe extern "C" fn middleware_apply(
    middleware: MqbMiddlewareHandle,
    messages: *const MqbMessage,
    len: usize,
    out_result: *mut MqbFilterHandle,
    out_messages: *mut *const MqbMessage,
    out_kept: *mut *const u8,
    err: *mut MqbBuffer,
) -> MqbStatus {
    guarded(err, || {
        let Some(state) = (unsafe { borrow::<MiddlewareState>(middleware.0) }) else {
            unsafe { set_error(err, "middleware_apply called with a null handle") };
            return MQB_ERR_PERMANENT;
        };
        let messages = unsafe { from_abi(messages, len) };
        let filter = Arc::clone(&state.filter);
        let receive = state.side == MQB_MIDDLEWARE_RECEIVE;
        let filtered = block_on(&state.runtime, async move {
            if receive {
                filter.on_receive(messages).await
            } else {
                filter.on_send(messages).await
            }
        });
        let filtered = match filtered {
            Ok(Ok(filtered)) => filtered,
            Ok(Err(error)) => {
                unsafe { set_error(err, format!("{error:#}")) };
                return MQB_ERR_RETRYABLE;
            }
            Err(failure) => return unsafe { task_failed(err, failure) },
        };
        // Enforced here so a mistake surfaces as a clear error rather than as a
        // mismatched array the host would read out of bounds.
        if filtered.len() != len {
            unsafe {
                set_error(
                    err,
                    format!(
                        "middleware returned {} entries for a batch of {len}; it must return \
                         exactly one entry per message, in order",
                        filtered.len()
                    ),
                )
            };
            return MQB_ERR_PERMANENT;
        }

        let mut kept = Vec::with_capacity(len);
        let messages = filtered
            .into_iter()
            .map(|message| {
                kept.push(u8::from(message.is_some()));
                // Keeps the arrays parallel; the host skips dropped entries.
                message.unwrap_or_else(|| CanonicalMessage::new(Vec::new(), None))
            })
            .collect();

        let state = Box::new(FilterResultState {
            messages: AbiMessages::new(messages),
            kept,
        });
        unsafe {
            *out_messages = state.messages.as_ptr();
            *out_kept = state.kept.as_ptr();
            *out_result = MqbFilterHandle(Box::into_raw(state).cast());
        }
        MQB_OK
    })
}

unsafe extern "C" fn middleware_result_free(result: MqbFilterHandle) {
    guarded_unit(|| drop(unsafe { reclaim::<FilterResultState>(result.0) }));
}

unsafe extern "C" fn middleware_free(middleware: MqbMiddlewareHandle) {
    guarded_unit(|| drop(unsafe { reclaim::<MiddlewareState>(middleware.0) }));
}

/// A function table wrapped so it can live in a `static`.
///
/// The table holds raw pointers (to its own name and to code), which makes it
/// `!Sync` by default. It is written once at compile time and never mutated.
#[doc(hidden)]
pub struct ExportedVTable(pub MqbPluginVTable);

unsafe impl Sync for ExportedVTable {}

impl ExportedVTable {
    pub const fn as_ptr(&self) -> *const MqbPluginVTable {
        &self.0
    }
}

/// Both consumers and publishers, the usual case.
pub const CAPABILITIES_INPUT_AND_OUTPUT: u64 = MQB_CAP_CONSUMER | MQB_CAP_PUBLISHER;
/// Input endpoints only.
pub const CAPABILITIES_INPUT_ONLY: u64 = MQB_CAP_CONSUMER;
/// Output endpoints only.
pub const CAPABILITIES_OUTPUT_ONLY: u64 = MQB_CAP_PUBLISHER;
/// A middleware, no endpoint.
pub const CAPABILITIES_MIDDLEWARE_ONLY: u64 = MQB_CAP_MIDDLEWARE;

/// Builds the function table for endpoint factory `F` and middleware factory
/// `M`. Use [`NoEndpoint`] or [`NoMiddleware`] for whichever half the plugin
/// does not provide; `capabilities` is what actually tells the host.
///
/// Called by [`export_endpoint_plugin!`]; there is no reason to call it
/// directly unless you export the discovery symbol yourself.
#[doc(hidden)]
pub const fn build_vtable<F, M>(
    name: &'static str,
    version: &'static str,
    capabilities: u64,
) -> ExportedVTable
where
    F: CustomEndpointFactory + Default + 'static,
    M: MiddlewareFactory,
{
    ExportedVTable(MqbPluginVTable {
        // The table this plugin was actually compiled with, which is what tells
        // a host which appended fields exist.
        struct_size: std::mem::size_of::<MqbPluginVTable>(),
        abi_major: MQB_PLUGIN_ABI_MAJOR,
        abi_minor: MQB_PLUGIN_ABI_MINOR,
        capabilities,
        name: MqbSlice::from_str(name),
        version: MqbSlice::from_str(version),
        factory_create: factory_create::<F>,
        factory_free,
        buffer_free,
        consumer_create,
        consumer_receive_batch,
        consumer_commit_requires_order,
        consumer_set_exit_on_empty,
        consumer_close,
        consumer_free,
        batch_commit,
        batch_free,
        publisher_create,
        publisher_send_batch,
        publisher_flush,
        publisher_close,
        publisher_free,
        middleware_create: middleware_create::<M>,
        middleware_apply,
        middleware_result_free,
        middleware_free,
    })
}

/// Exports an endpoint factory as a loadable mq-bridge plugin.
///
/// ```ignore
/// mq_bridge::export_endpoint_plugin! {
///     name: "pulsar",
///     factory: PulsarFactory,
/// }
/// ```
///
/// The plugin reports the exporting crate's version. Add a middleware under the
/// same name — routes then use it as `middlewares: [{ custom: { name: "pulsar" } }]`:
///
/// ```ignore
/// mq_bridge::export_endpoint_plugin! {
///     name: "pulsar",
///     factory: PulsarFactory,
///     middleware: PulsarMiddleware,
/// }
/// ```
///
/// `capabilities` defaults to [`CAPABILITIES_INPUT_AND_OUTPUT`] (plus
/// [`MQB_CAP_MIDDLEWARE`] when a middleware is given); set it explicitly for a
/// one-directional endpoint:
///
/// ```ignore
/// mq_bridge::export_endpoint_plugin! {
///     name: "metrics-sink",
///     factory: SinkFactory,
///     capabilities: mq_bridge::plugin::sdk::CAPABILITIES_OUTPUT_ONLY,
/// }
/// ```
///
/// The macro defines the `mq_bridge_plugin_v1` symbol, so a crate may export at
/// most one plugin — and two plugins cannot be statically linked into the same
/// binary. Gate the macro behind a feature if your crate is also linked
/// directly alongside others.
#[macro_export]
macro_rules! export_endpoint_plugin {
    (name: $name:expr, factory: $factory:ty $(,)?) => {
        $crate::export_endpoint_plugin!(
            name: $name,
            factory: $factory,
            middleware: $crate::plugin::sdk::NoMiddleware,
            capabilities: $crate::plugin::sdk::CAPABILITIES_INPUT_AND_OUTPUT,
        );
    };
    (name: $name:expr, factory: $factory:ty, capabilities: $capabilities:expr $(,)?) => {
        $crate::export_endpoint_plugin!(
            name: $name,
            factory: $factory,
            middleware: $crate::plugin::sdk::NoMiddleware,
            capabilities: $capabilities,
        );
    };
    (name: $name:expr, factory: $factory:ty, middleware: $middleware:ty $(,)?) => {
        $crate::export_endpoint_plugin!(
            name: $name,
            factory: $factory,
            middleware: $middleware,
            capabilities: $crate::plugin::sdk::CAPABILITIES_INPUT_AND_OUTPUT
                | $crate::plugin::sdk::CAPABILITIES_MIDDLEWARE_ONLY,
        );
    };
    (name: $name:expr, factory: $factory:ty, middleware: $middleware:ty, capabilities: $capabilities:expr $(,)?) => {
        const _: () = {
            static MQ_BRIDGE_PLUGIN_VTABLE: $crate::plugin::sdk::ExportedVTable =
                $crate::plugin::sdk::build_vtable::<$factory, $middleware>(
                    $name,
                    env!("CARGO_PKG_VERSION"),
                    $capabilities,
                );

            /// Discovery symbol read by `mq_bridge::plugin::load_endpoint_plugin`.
            #[no_mangle]
            pub extern "C" fn mq_bridge_plugin_v1() -> *const $crate::support::plugin_abi::MqbPluginVTable {
                MQ_BRIDGE_PLUGIN_VTABLE.as_ptr()
            }
        };
    };
}

/// Exports a middleware as a loadable mq-bridge plugin, with no endpoint.
///
/// ```ignore
/// mq_bridge::export_middleware_plugin! {
///     name: "redact",
///     middleware: RedactFactory,
/// }
/// ```
///
/// Routes then name it like any custom middleware:
///
/// ```yaml
/// input:
///   kafka: { topic: orders }
///   middlewares:
///     - custom:
///         name: redact
///         config: { fields: ["ssn"] }
/// ```
#[macro_export]
macro_rules! export_middleware_plugin {
    (name: $name:expr, middleware: $middleware:ty $(,)?) => {
        $crate::export_endpoint_plugin!(
            name: $name,
            factory: $crate::plugin::sdk::NoEndpoint,
            middleware: $middleware,
            capabilities: $crate::plugin::sdk::CAPABILITIES_MIDDLEWARE_ONLY,
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_buffers_round_trip_through_the_host() {
        let mut slot = MqbBuffer::EMPTY;
        unsafe { set_error(&mut slot, "boom") };
        assert_eq!(unsafe { slot.as_bytes() }, b"boom");
        unsafe { buffer_free(slot) };
    }

    #[test]
    fn a_null_error_slot_is_tolerated() {
        unsafe { set_error(std::ptr::null_mut(), "dropped") };
    }

    #[test]
    fn panics_become_a_status_instead_of_unwinding() {
        let mut slot = MqbBuffer::EMPTY;
        let status = guarded(&mut slot, || panic!("kaboom"));
        assert_eq!(status, MQB_ERR_PANIC);
        let text = String::from_utf8_lossy(unsafe { slot.as_bytes() }).into_owned();
        assert!(text.contains("kaboom"), "{text}");
        unsafe { buffer_free(slot) };
    }

    #[test]
    fn error_classes_map_onto_abi_status_codes() {
        assert_eq!(
            consumer_status(&ConsumerError::EndOfStream),
            MQB_END_OF_STREAM
        );
        assert_eq!(
            consumer_status(&ConsumerError::Permanent(anyhow::anyhow!("x"))),
            MQB_ERR_PERMANENT
        );
        assert_eq!(
            consumer_status(&ConsumerError::Connection(anyhow::anyhow!("x"))),
            MQB_ERR_CONNECTION
        );
        assert_eq!(
            processing_status(&ProcessingError::Retryable(anyhow::anyhow!("x"))),
            MQB_ERR_RETRYABLE
        );
        assert_eq!(
            processing_status(&ProcessingError::NonRetryable(anyhow::anyhow!("x"))),
            MQB_ERR_PERMANENT
        );
    }

    #[test]
    fn config_parsing_accepts_an_empty_slice() {
        let value = unsafe { read_config(MqbSlice::EMPTY) }.unwrap();
        assert!(value.as_object().is_some_and(serde_json::Map::is_empty));
        assert!(unsafe { read_config(MqbSlice::from_str("{oops")) }.is_err());
    }
}
