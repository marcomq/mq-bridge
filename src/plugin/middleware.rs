//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! A loaded plugin's middleware, presented as a [`CustomMiddlewareFactory`].
//!
//! A map/drop middleware only sees the messages: the wrapper around the inner consumer
//! or publisher stays here on the host side. A wrapping middleware (ABI 1.5) gets the
//! inner endpoint itself, lent through the host's own endpoint table.

use std::any::Any;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Bytes;
use futures::task::AtomicWaker;

use super::endpoint::{
    consumer_error_from, join_error, plugin_consumer, plugin_publisher, publisher_error_from,
    AssertSend,
};
use super::message::{read_message, AbiMessages};
use super::LoadedPlugin;
use crate::errors::{ConsumerError, PublisherError};
use crate::middleware::deferred_commit::{run_all, DeferredCommits};
use crate::middleware::lookup::lookup_through;
use crate::support::plugin_abi::{
    MqbBuffer, MqbCompletion, MqbConsumerHandle, MqbFilterHandle, MqbMessage,
    MqbMiddlewareApplyAsync, MqbMiddlewareHandle, MqbPublisherHandle, MqbSlice, MqbStatus,
    MQB_END_OF_STREAM, MQB_ERR_CONNECTION, MQB_ERR_INVALID_CONFIG, MQB_ERR_PANIC,
    MQB_ERR_PERMANENT, MQB_ERR_UNSUPPORTED, MQB_MESSAGE_DROPPED, MQB_MESSAGE_KEPT,
    MQB_MESSAGE_REPLACED, MQB_MIDDLEWARE_RECEIVE, MQB_MIDDLEWARE_SEND, MQB_OK, MQB_PENDING,
};
use crate::traits::{
    BatchCommitFunc, BoxFuture, CustomMiddlewareFactory, EndpointStatus, MessageConsumer,
    MessageDisposition, MessagePublisher,
};
use crate::{CanonicalMessage, ReceivedBatch, SentBatch};

/// A [`CustomMiddlewareFactory`] backed by a loaded plugin.
pub struct PluginMiddlewareFactory {
    plugin: Arc<LoadedPlugin>,
}

impl PluginMiddlewareFactory {
    pub(crate) fn new(plugin: Arc<LoadedPlugin>) -> Self {
        Self { plugin }
    }

    async fn open(
        &self,
        route_name: &str,
        config: &serde_json::Value,
        side: u8,
    ) -> anyhow::Result<PluginMiddleware> {
        let plugin = Arc::clone(&self.plugin);
        let route = route_name.to_owned();
        let config = serde_json::to_vec(config)?;
        let handle = tokio::task::spawn_blocking(move || {
            let mut out = MqbMiddlewareHandle::NULL;
            let mut err = MqbBuffer::EMPTY;
            let status = unsafe {
                (plugin.table().middleware_create)(
                    plugin.factory(),
                    MqbSlice::from_str(&route),
                    MqbSlice::from_bytes(&config),
                    side,
                    &mut out,
                    &mut err,
                )
            };
            if status == MQB_OK {
                Ok(AssertSend(out))
            } else {
                Err(anyhow!(
                    "middleware plugin `{}` could not open for route `{route}`: {}",
                    plugin.name(),
                    plugin.take_error(err)
                ))
            }
        })
        .await
        .map_err(join_error)??;

        Ok(PluginMiddleware {
            plugin: Arc::clone(&self.plugin),
            handle: handle.0,
            blocking: AtomicBool::new(false),
            spare: Mutex::default(),
        })
    }
}

impl std::fmt::Debug for PluginMiddlewareFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginMiddlewareFactory")
            .field("name", &self.plugin.name())
            .field("path", &self.plugin.info.path)
            .finish()
    }
}

#[async_trait]
impl CustomMiddlewareFactory for PluginMiddlewareFactory {
    fn config_schema(&self) -> Option<serde_json::Value> {
        self.plugin.info.middleware_schema()
    }

    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let middleware = self
            .open(route_name, config, MQB_MIDDLEWARE_RECEIVE)
            .await?;
        if let Some((wrap, _)) = self.plugin.table().wrap_hooks() {
            let inner = AssertSend(super::sdk::lend_consumer(consumer).await?);
            let route = route_name.to_owned();
            let handle = tokio::task::spawn_blocking(move || {
                let (middleware, inner) = (middleware, inner);
                let mut out = MqbConsumerHandle::NULL;
                let mut err = MqbBuffer::EMPTY;
                let host = super::sdk::host_table();
                let status = unsafe { wrap(middleware.handle, host, inner.0, &mut out, &mut err) };
                if status == MQB_OK {
                    return Ok(AssertSend(out));
                }
                let plugin = &middleware.plugin;
                let cause = anyhow!(
                    "middleware plugin `{}` could not wrap the input of route `{route}`: {}",
                    plugin.name(),
                    plugin.take_error(err)
                );
                Err(anyhow::Error::new(consumer_error_from(status, cause)))
            })
            .await
            .map_err(join_error)??;
            return Ok(plugin_consumer(Arc::clone(&self.plugin), handle.0));
        }
        Ok(Box::new(PluginMiddlewareConsumer {
            inner: consumer,
            scratch: Scratch::new(&middleware.plugin),
            middleware: Arc::new(middleware),
            deferred: DeferredCommits::new(),
        }))
    }

    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let middleware = self.open(route_name, config, MQB_MIDDLEWARE_SEND).await?;
        if let Some((_, wrap)) = self.plugin.table().wrap_hooks() {
            let inner = AssertSend(super::sdk::lend_publisher(publisher).await?);
            let route = route_name.to_owned();
            let handle = tokio::task::spawn_blocking(move || {
                let (middleware, inner) = (middleware, inner);
                let mut out = MqbPublisherHandle::NULL;
                let mut err = MqbBuffer::EMPTY;
                let host = super::sdk::host_table();
                let status = unsafe { wrap(middleware.handle, host, inner.0, &mut out, &mut err) };
                if status == MQB_OK {
                    return Ok(AssertSend(out));
                }
                let plugin = &middleware.plugin;
                let cause = anyhow!(
                    "middleware plugin `{}` could not wrap the output of route `{route}`: {}",
                    plugin.name(),
                    plugin.take_error(err)
                );
                Err(anyhow::Error::new(publisher_error_from(status, cause)))
            })
            .await
            .map_err(join_error)??;
            return Ok(plugin_publisher(Arc::clone(&self.plugin), handle.0));
        }
        Ok(Box::new(PluginMiddlewarePublisher {
            inner: publisher,
            middleware: Arc::new(middleware),
        }))
    }
}

/// One middleware instance inside the plugin.
struct PluginMiddleware {
    plugin: Arc<LoadedPlugin>,
    handle: MqbMiddlewareHandle,
    /// Set once the plugin refuses the non-blocking apply.
    blocking: AtomicBool,
    /// Call buffers for the send side, whose batches may overlap.
    spare: Mutex<Vec<Arc<Scratch>>>,
}

unsafe impl Send for PluginMiddleware {}
unsafe impl Sync for PluginMiddleware {}

/// What the middleware did to a batch: the surviving messages, plus one flag
/// per source message so the caller can still address the dropped ones.
struct Filtered {
    kept: Vec<CanonicalMessage>,
    keep_flags: Vec<bool>,
    /// One per kept message, when asked for and something was replaced; empty otherwise.
    originals: Vec<Original>,
}

/// A kept message's source, for handing a failed send back as it came in. A part
/// `kept` reuses unchanged is `None`.
struct Original {
    message_id: u128,
    payload: Option<Bytes>,
    metadata: Option<HashMap<String, String>>,
    /// The payload sent on, to tell apart messages that share an id.
    rewritten: Bytes,
}

/// A failed `middleware_apply`, keeping the ABI status so the caller can
/// classify it the same way an endpoint failure is classified.
struct ApplyFailure {
    status: MqbStatus,
    cause: anyhow::Error,
}

fn malformed(plugin: &LoadedPlugin, what: impl std::fmt::Display) -> ApplyFailure {
    ApplyFailure {
        status: MQB_ERR_PERMANENT,
        cause: anyhow!("middleware plugin `{}` {what}", plugin.name()),
    }
}

/// The out-parameters of one blocking `middleware_apply`.
struct Applied {
    result: MqbFilterHandle,
    messages: *const MqbMessage,
    kept: *const u8,
    err: MqbBuffer,
}

impl Applied {
    const EMPTY: Applied = Applied {
        result: MqbFilterHandle::NULL,
        messages: std::ptr::null(),
        kept: std::ptr::null(),
        err: MqbBuffer::EMPTY,
    };

    fn release(self, plugin: &Arc<LoadedPlugin>) {
        drop(FilterResult {
            plugin: Arc::clone(plugin),
            handle: self.result,
        });
        if !self.err.is_empty() {
            plugin.take_error(self.err);
        }
    }
}

/// The buffers of a `middleware_apply_async` call, reused batch after batch. Only the
/// owner of the `Arc` fills them; the plugin holds a second reference while a call is
/// pending.
struct Scratch {
    call: UnsafeCell<Call>,
    plugin: Arc<LoadedPlugin>,
    /// Where an abandoned result is released, never on the plugin's own thread.
    host: Option<tokio::runtime::Handle>,
    done: AtomicBool,
    status: AtomicI32,
    waker: AtomicWaker,
}

struct Call {
    input: AbiMessages,
    verdicts: Vec<u8>,
    result: MqbFilterHandle,
    replaced: *const MqbMessage,
    err: MqbBuffer,
}

// `call` is reached by its owner between calls, or by the plugin while one is pending.
unsafe impl Send for Scratch {}
unsafe impl Sync for Scratch {}

impl Scratch {
    fn new(plugin: &Arc<LoadedPlugin>) -> Arc<Self> {
        Arc::new(Scratch {
            call: UnsafeCell::new(Call {
                input: AbiMessages::new(Vec::new()),
                verdicts: Vec::new(),
                result: MqbFilterHandle::NULL,
                replaced: std::ptr::null(),
                err: MqbBuffer::EMPTY,
            }),
            plugin: Arc::clone(plugin),
            host: tokio::runtime::Handle::try_current().ok(),
            done: AtomicBool::new(false),
            status: AtomicI32::new(MQB_OK),
            waker: AtomicWaker::new(),
        })
    }
}

impl Call {
    fn unread(&mut self) -> Applied {
        Applied {
            result: std::mem::replace(&mut self.result, MqbFilterHandle::NULL),
            err: std::mem::replace(&mut self.err, MqbBuffer::EMPTY),
            ..Applied::EMPTY
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let call = self.call.get_mut();
        if call.result.is_null() && call.err.is_empty() {
            return;
        }
        let unread = AssertSend(call.unread());
        let plugin = Arc::clone(&self.plugin);
        let release = move || {
            let unread = unread;
            unread.0.release(&plugin);
        };
        match &self.host {
            Some(host) => drop(host.spawn_blocking(release)),
            None => release(),
        }
    }
}

unsafe extern "C" fn completed(ctx: *mut c_void, status: MqbStatus) {
    // SAFETY: `ctx` is the reference `apply_async` handed over for this call.
    let scratch = unsafe { Arc::from_raw(ctx.cast_const().cast::<Scratch>()) };
    scratch.status.store(status, Ordering::Relaxed);
    scratch.done.store(true, Ordering::Release);
    scratch.waker.wake();
}

/// Waits for a pending call. Dropped before it finishes, it leaves the buffers to the
/// plugin and gives the owner fresh ones.
struct Pending<'a> {
    scratch: &'a mut Arc<Scratch>,
    finished: bool,
}

impl Future for Pending<'_> {
    type Output = MqbStatus;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<MqbStatus> {
        self.scratch.waker.register(cx.waker());
        if !self.scratch.done.load(Ordering::Acquire) {
            return Poll::Pending;
        }
        self.finished = true;
        Poll::Ready(self.scratch.status.load(Ordering::Relaxed))
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let plugin = Arc::clone(&self.scratch.plugin);
            *self.scratch = Scratch::new(&plugin);
        }
    }
}

impl PluginMiddleware {
    /// `keep_originals` fills [`Filtered::originals`], which only a send needs.
    async fn apply(
        self: &Arc<Self>,
        scratch: &mut Arc<Scratch>,
        messages: Vec<CanonicalMessage>,
        keep_originals: bool,
    ) -> Result<Filtered, ApplyFailure> {
        let hook = self.plugin.table().middleware_apply_async_hook();
        let messages = match hook {
            Some(_) if !self.blocking.load(Ordering::Relaxed) => {
                match self
                    .apply_async(hook, scratch, messages, keep_originals)
                    .await
                {
                    Ok(filtered) => return filtered,
                    Err(messages) => messages,
                }
            }
            _ => messages,
        };
        self.apply_blocking(messages, keep_originals).await
    }

    /// [`Self::apply`] with buffers from the spare pool, for callers that may overlap.
    async fn apply_pooled(
        self: &Arc<Self>,
        messages: Vec<CanonicalMessage>,
        keep_originals: bool,
    ) -> Result<Filtered, ApplyFailure> {
        let spare = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let mut scratch = spare.unwrap_or_else(|| Scratch::new(&self.plugin));
        let filtered = self.apply(&mut scratch, messages, keep_originals).await;
        let mut spare = self.spare.lock().unwrap_or_else(|e| e.into_inner());
        if spare.len() < 64 {
            spare.push(scratch);
        }
        filtered
    }

    /// `Err` hands the batch back when the plugin has no non-blocking apply after all.
    async fn apply_async(
        &self,
        hook: MqbMiddlewareApplyAsync,
        scratch: &mut Arc<Scratch>,
        messages: Vec<CanonicalMessage>,
        keep_originals: bool,
    ) -> Result<Result<Filtered, ApplyFailure>, Vec<CanonicalMessage>> {
        let Some(hook) = hook else {
            return Err(messages);
        };
        if Arc::get_mut(scratch).is_none() {
            // The plugin has yet to let go of an abandoned call.
            *scratch = Scratch::new(&self.plugin);
        }
        let call = scratch.call.get();
        // SAFETY: we are the only owner, so no call is in flight.
        let (input, len, verdicts) = unsafe {
            let call = &mut *call;
            call.input.reload(messages);
            call.verdicts.clear();
            call.verdicts.resize(call.input.len(), MQB_MESSAGE_KEPT);
            call.replaced = std::ptr::null();
            (
                call.input.as_ptr(),
                call.input.len(),
                call.verdicts.as_mut_ptr(),
            )
        };
        scratch.done.store(false, Ordering::Relaxed);
        let ctx = Arc::into_raw(Arc::clone(scratch))
            .cast_mut()
            .cast::<c_void>();
        let completion = MqbCompletion {
            callback: completed,
            ctx,
        };
        let status = unsafe {
            hook(
                self.handle,
                input,
                len,
                verdicts,
                &raw mut (*call).result,
                &raw mut (*call).replaced,
                &raw mut (*call).err,
                completion,
            )
        };
        let status = if status == MQB_PENDING {
            Pending {
                scratch,
                finished: false,
            }
            .await
        } else {
            // SAFETY: with any other status the completion never runs.
            unsafe { Arc::decrement_strong_count(ctx.cast_const().cast::<Scratch>()) };
            status
        };

        // SAFETY: the call is over; after completing, the plugin touches only atomics.
        let call = unsafe { &mut *scratch.call.get() };
        if status == MQB_ERR_UNSUPPORTED {
            self.blocking.store(true, Ordering::Relaxed);
            call.unread().release(&self.plugin);
            return Err(call.input.take_messages());
        }
        let filtered = if status == MQB_OK {
            unsafe {
                settle(
                    &self.plugin,
                    &mut call.input,
                    &call.verdicts,
                    call.replaced,
                    keep_originals,
                )
            }
        } else {
            let err = std::mem::replace(&mut call.err, MqbBuffer::EMPTY);
            Err(ApplyFailure {
                status,
                cause: anyhow!(
                    "middleware plugin `{}` failed on a batch of {len} messages: {}",
                    self.plugin.name(),
                    self.plugin.take_error(err)
                ),
            })
        };
        call.unread().release(&self.plugin);
        drop(call.input.take_messages());
        Ok(filtered)
    }

    async fn apply_blocking(
        self: &Arc<Self>,
        messages: Vec<CanonicalMessage>,
        keep_originals: bool,
    ) -> Result<Filtered, ApplyFailure> {
        let middleware = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut input = AbiMessages::new(messages);
            let mut out = Applied::EMPTY;
            let status = unsafe {
                (middleware.plugin.table().middleware_apply)(
                    middleware.handle,
                    input.as_ptr(),
                    input.len(),
                    &mut out.result,
                    &mut out.messages,
                    &mut out.kept,
                    &mut out.err,
                )
            };
            let filtered = unsafe {
                read_applied(
                    &middleware.plugin,
                    status,
                    &mut input,
                    &mut out,
                    keep_originals,
                )
            };
            out.release(&middleware.plugin);
            AssertSend(filtered)
        })
        .await
        .map_err(|err| ApplyFailure {
            status: MQB_ERR_CONNECTION,
            cause: join_error(err),
        })?
        .0
    }
}

/// Turns a blocking `middleware_apply`'s parallel arrays into verdicts: a kept entry that
/// still points at the host's own message is unchanged, any other kept entry replaces it.
unsafe fn read_applied(
    plugin: &Arc<LoadedPlugin>,
    status: MqbStatus,
    input: &mut AbiMessages,
    out: &mut Applied,
    keep_originals: bool,
) -> Result<Filtered, ApplyFailure> {
    if status != MQB_OK {
        let err = std::mem::replace(&mut out.err, MqbBuffer::EMPTY);
        return Err(ApplyFailure {
            status,
            cause: anyhow!(
                "middleware plugin `{}` failed on a batch of {} messages: {}",
                plugin.name(),
                input.len(),
                plugin.take_error(err)
            ),
        });
    }
    if out.messages.is_null() || out.kept.is_null() {
        return Err(malformed(plugin, "returned a null result array"));
    }
    let len = input.len();
    // SAFETY: both checked non-null above; the ABI makes them one entry per input message,
    // owned by the result, which the caller releases after this returns.
    let flags = unsafe { std::slice::from_raw_parts(out.kept, len) };
    let results = unsafe { std::slice::from_raw_parts(out.messages, len) };
    let mut verdicts = Vec::with_capacity(len);
    let mut replaced = Vec::new();
    for ((flag, result), ours) in flags.iter().zip(results).zip(input.abi()) {
        // A dropped entry's message is unspecified, so only kept ones are read.
        let verdict = if *flag != MQB_MESSAGE_KEPT {
            MQB_MESSAGE_DROPPED
        } else if result.message_id == ours.message_id
            && same_payload(result, ours)
            && same_metadata(result, ours)
        {
            MQB_MESSAGE_KEPT
        } else {
            replaced.push(*result);
            MQB_MESSAGE_REPLACED
        };
        verdicts.push(verdict);
    }
    unsafe { settle(plugin, input, &verdicts, replaced.as_ptr(), keep_originals) }
}

fn same_payload(theirs: &MqbMessage, ours: &MqbMessage) -> bool {
    theirs.payload.ptr == ours.payload.ptr && theirs.payload.len == ours.payload.len
}

fn same_metadata(theirs: &MqbMessage, ours: &MqbMessage) -> bool {
    theirs.metadata == ours.metadata && theirs.metadata_len == ours.metadata_len
}

/// Applies the verdicts to the batch in place: dropped messages leave it, replaced ones
/// are rewritten, reusing the host's own payload or metadata where the replacement points
/// back at it.
///
/// # Safety
/// `replaced` holds one valid message per [`MQB_MESSAGE_REPLACED`] verdict.
unsafe fn settle(
    plugin: &LoadedPlugin,
    input: &mut AbiMessages,
    verdicts: &[u8],
    replaced: *const MqbMessage,
    keep_originals: bool,
) -> Result<Filtered, ApplyFailure> {
    let count = verdicts
        .iter()
        .filter(|verdict| **verdict == MQB_MESSAGE_REPLACED)
        .count();
    let replacements: &[MqbMessage] = match count {
        0 => &[],
        _ if replaced.is_null() => {
            return Err(malformed(plugin, "returned a null replacement array"))
        }
        _ => unsafe { std::slice::from_raw_parts(replaced, count) },
    };
    let keep_originals = keep_originals && count > 0;
    let mut replacements = replacements.iter();
    let mut keep_flags = Vec::with_capacity(verdicts.len());
    let mut originals = Vec::new();
    let (abi, messages) = input.split();
    for ((message, ours), verdict) in messages.iter_mut().zip(abi).zip(verdicts) {
        match *verdict {
            MQB_MESSAGE_KEPT => {
                if keep_originals {
                    originals.push(Original {
                        message_id: message.message_id,
                        payload: None,
                        metadata: None,
                        rewritten: message.payload.clone(),
                    });
                }
            }
            MQB_MESSAGE_DROPPED => {}
            MQB_MESSAGE_REPLACED => {
                let theirs = replacements.next().expect("counted above");
                // Kept either way: a later replacement may still point into this input.
                originals.push(unsafe { replace_in_place(message, ours, theirs) });
            }
            other => {
                return Err(malformed(
                    plugin,
                    format!("returned unknown verdict {other}"),
                ))
            }
        }
        keep_flags.push(*verdict != MQB_MESSAGE_DROPPED);
    }
    if !keep_originals {
        originals.clear();
    }
    let mut kept = input.take_messages();
    let mut flags = keep_flags.iter();
    kept.retain(|_| flags.next().copied().unwrap_or(false));
    Ok(Filtered {
        kept,
        keep_flags,
        originals,
    })
}

/// Rewrites `message` as `theirs`, returning what it was.
///
/// # Safety
/// `theirs` is a valid message; `ours` is `message` in ABI form.
unsafe fn replace_in_place(
    message: &mut CanonicalMessage,
    ours: &MqbMessage,
    theirs: &MqbMessage,
) -> Original {
    let payload = same_payload(theirs, ours);
    let metadata = same_metadata(theirs, ours);
    let mut fresh = unsafe { read_message(theirs, !payload, !metadata) };
    if payload {
        fresh.payload = message.payload.clone();
    }
    if metadata {
        *fresh.metadata_map_mut() = std::mem::take(message.metadata_map_mut());
    }
    let mut source = std::mem::replace(message, fresh);
    Original {
        message_id: source.message_id,
        payload: (!payload).then(|| std::mem::take(&mut source.payload)),
        metadata: (!metadata).then(|| std::mem::take(source.metadata_map_mut())),
        rewritten: message.payload.clone(),
    }
}

/// Hands failed messages back as they came in, so a retry above re-applies the
/// middleware to the source message rather than to its output. Messages sharing an
/// id are told apart by the payload they were sent with.
fn restore_originals(originals: Vec<Original>, failed: &mut [(CanonicalMessage, PublisherError)]) {
    let mut by_id: HashMap<u128, Vec<Original>> = HashMap::with_capacity(originals.len());
    for original in originals {
        by_id.entry(original.message_id).or_default().push(original);
    }
    for (message, _) in failed {
        let Some(candidates) = by_id.get_mut(&message.message_id) else {
            continue;
        };
        let found = if candidates.len() == 1 {
            Some(0)
        } else {
            candidates
                .iter()
                .position(|original| original.rewritten == message.payload)
        };
        let Some(at) = found else {
            continue;
        };
        let original = candidates.swap_remove(at);
        if let Some(payload) = original.payload {
            message.payload = payload;
        }
        if let Some(metadata) = original.metadata {
            *message.metadata_map_mut() = metadata;
        }
    }
}

impl Drop for PluginMiddleware {
    fn drop(&mut self) {
        unsafe { (self.plugin.table().middleware_free)(self.handle) };
    }
}

/// Releases one `middleware_apply` result.
struct FilterResult {
    plugin: Arc<LoadedPlugin>,
    handle: MqbFilterHandle,
}

impl Drop for FilterResult {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.plugin.table().middleware_result_free)(self.handle) };
        }
    }
}

struct PluginMiddlewareConsumer {
    inner: Box<dyn MessageConsumer>,
    middleware: Arc<PluginMiddleware>,
    /// Filled only here, through `&mut self`.
    scratch: Arc<Scratch>,
    /// Acks for batches the plugin emptied, held on sources with ordered commits.
    deferred: DeferredCommits,
}

#[async_trait]
impl MessageConsumer for PluginMiddlewareConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // Keep pulling until something survives the filter. Returning an empty
        // batch here would be read as "the source is drained" and would end an
        // `exit_on_empty` route early; only the inner consumer may say that.
        loop {
            let batch = self.inner.receive_batch(max_messages).await?;
            if batch.messages.is_empty() {
                // Nothing follows a drain signal to carry the held acks.
                run_all(self.deferred.take())
                    .await
                    .map_err(ConsumerError::Connection)?;
                return Ok(batch);
            }
            let ReceivedBatch { messages, commit } = batch;
            let Filtered {
                kept, keep_flags, ..
            } = self
                .middleware
                .apply(&mut self.scratch, messages, false)
                .await
                .map_err(|failure| consumer_error_from(failure.status, failure.cause))?;

            if kept.is_empty() {
                // The route never commits a batch it never sees, so it is acked
                // here: at once, or after the batches still in flight.
                let ordered = self.inner.commit_requires_order();
                self.deferred
                    .ack_emptied(ordered, commit, keep_flags.len())
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }

            // The route only sees the kept messages, so expand its dispositions
            // back to one per source message, acking the ones we dropped.
            let expected = kept.len();
            let held = self.deferred.take();
            let in_flight = self.deferred.track();
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                Box::pin(async move {
                    // A miscount would silently ack or drop source messages, so
                    // reject it the way the plugin consumer does.
                    if dispositions.len() != expected {
                        return Err(anyhow!(
                            "plugin middleware commit received {} dispositions for {expected} \
                             kept messages",
                            dispositions.len()
                        ));
                    }
                    run_all(held).await?;
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
                    commit(expanded).await?;
                    in_flight.settled();
                    Ok(())
                })
            });
            return Ok(ReceivedBatch {
                messages: kept,
                commit,
            });
        }
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    /// Releases the acks still held for emptied batches, then runs the source's hook.
    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        let inner_hook = self.inner.on_disconnect_hook();
        let held = self.deferred.take_shared();
        if held.is_empty() {
            return inner_hook;
        }
        Some(Box::pin(async move {
            let mut first_error = run_all(held).await.err();
            if let Some(hook) = inner_hook {
                if let Err(error) = hook.await {
                    first_error.get_or_insert(error);
                }
            }
            first_error.map_or(Ok(()), Err)
        }))
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

struct PluginMiddlewarePublisher {
    inner: Box<dyn MessagePublisher>,
    middleware: Arc<PluginMiddleware>,
}

#[async_trait]
impl MessagePublisher for PluginMiddlewarePublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        let filtered = self
            .middleware
            .apply_pooled(messages, true)
            .await
            .map_err(|failure| match failure.status {
                MQB_ERR_CONNECTION => PublisherError::Connection(failure.cause),
                MQB_ERR_PERMANENT
                | MQB_ERR_INVALID_CONFIG
                | MQB_ERR_UNSUPPORTED
                | MQB_ERR_PANIC
                | MQB_END_OF_STREAM => PublisherError::NonRetryable(failure.cause),
                _ => PublisherError::Retryable(failure.cause),
            })?;
        if filtered.kept.is_empty() {
            // Everything was dropped on purpose, which is a successful publish.
            return Ok(SentBatch::Ack);
        }
        match self.inner.send_batch(filtered.kept).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial {
                responses,
                mut failed,
            } => {
                restore_originals(filtered.originals, &mut failed);
                Ok(SentBatch::Partial { responses, failed })
            }
        }
    }

    /// `None` when the plugin fails the batch, so each request is sent, and fails, alone.
    async fn lookup_each(
        &self,
        requests: &[CanonicalMessage],
    ) -> Option<Vec<Result<Option<serde_json::Value>, PublisherError>>> {
        let filtered = self
            .middleware
            .apply_pooled(requests.to_vec(), false)
            .await
            .ok()?;
        let mut kept = filtered.kept.into_iter();
        let outcomes = filtered
            .keep_flags
            .iter()
            .map(|keep| Ok(keep.then(|| kept.next()).flatten()))
            .collect();
        Some(lookup_through(self.inner.as_ref(), outcomes).await)
    }

    crate::traits::forward_publisher_meta!(inner);

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(all(test, feature = "plugin-sdk"))]
mod tests {
    use super::*;
    use crate::extensions::get_middleware_factory;
    use crate::plugin::sdk::{
        build_vtable, BatchFilter, ExportedVTable, MessageRef, MiddlewareFactory, NoEndpoint,
        Verdict, CAPABILITIES_MIDDLEWARE_ONLY,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Passthrough;
    impl BatchFilter for Passthrough {}

    #[derive(Default)]
    struct PassthroughFactory;

    #[async_trait]
    impl MiddlewareFactory for PassthroughFactory {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(Passthrough))
        }
    }

    static TABLE: ExportedVTable = build_vtable::<NoEndpoint, PassthroughFactory>(
        "hook-passthrough",
        "0.0.0",
        CAPABILITIES_MIDDLEWARE_ONLY,
    );

    /// An endpoint that counts its lifecycle hooks and reports itself unhealthy.
    #[derive(Default)]
    struct Hooked {
        connects: Arc<AtomicUsize>,
        disconnects: Arc<AtomicUsize>,
    }

    impl Hooked {
        fn count(counter: &Arc<AtomicUsize>) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        }

        fn unhealthy() -> EndpointStatus {
            EndpointStatus {
                healthy: false,
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl MessageConsumer for Hooked {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Self::count(&self.connects)
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Self::count(&self.disconnects)
        }

        async fn receive_batch(&mut self, _: usize) -> Result<ReceivedBatch, ConsumerError> {
            Err(ConsumerError::EndOfStream)
        }

        async fn status(&self) -> EndpointStatus {
            Self::unhealthy()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait]
    impl MessagePublisher for Hooked {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Self::count(&self.connects)
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Self::count(&self.disconnects)
        }

        async fn send_batch(&self, _: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }

        async fn status(&self) -> EndpointStatus {
            Self::unhealthy()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct DropMarked;

    #[async_trait]
    impl BatchFilter for DropMarked {
        async fn on_receive(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
            Ok(messages
                .into_iter()
                .map(|message| (&message.payload[..] != b"drop").then_some(message))
                .collect())
        }
    }

    #[derive(Default)]
    struct DropMarkedFactory;

    #[async_trait]
    impl MiddlewareFactory for DropMarkedFactory {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(DropMarked))
        }
    }

    static DROP_TABLE: ExportedVTable = build_vtable::<NoEndpoint, DropMarkedFactory>(
        "drop-marked",
        "0.0.0",
        CAPABILITIES_MIDDLEWARE_ONLY,
    );

    struct Rejecting;

    #[async_trait]
    impl BatchFilter for Rejecting {
        async fn on_send(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
            if &messages[0].payload[..] == b"malformed" {
                return Err(PublisherError::NonRetryable(anyhow!("malformed")).into());
            }
            Err(anyhow!("busy"))
        }
    }

    #[derive(Default)]
    struct RejectingFactory;

    #[async_trait]
    impl MiddlewareFactory for RejectingFactory {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(Rejecting))
        }
    }

    static REJECT_TABLE: ExportedVTable = build_vtable::<NoEndpoint, RejectingFactory>(
        "rejecting",
        "0.0.0",
        CAPABILITIES_MIDDLEWARE_ONLY,
    );

    #[tokio::test(flavor = "multi_thread")]
    async fn a_filter_error_keeps_its_permanent_class() {
        unsafe { crate::plugin::register_plugin_table(REJECT_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("rejecting").unwrap();
        let publisher = factory
            .apply_publisher(Box::new(Hooked::default()), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let send = |payload: &'static str| {
            publisher.send_batch(vec![CanonicalMessage::new(
                payload.as_bytes().to_vec(),
                None,
            )])
        };
        assert!(matches!(
            send("malformed").await,
            Err(PublisherError::NonRetryable(_))
        ));
        assert!(matches!(
            send("busy").await,
            Err(PublisherError::Retryable(_))
        ));
    }

    /// Uppercases each payload on the way out.
    struct Upper;

    #[async_trait]
    impl BatchFilter for Upper {
        async fn on_send(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
            Ok(messages
                .into_iter()
                .map(|mut message| {
                    message.payload = message.payload.to_ascii_uppercase().into();
                    Some(message)
                })
                .collect())
        }
    }

    #[derive(Default)]
    struct UpperFactory;

    #[async_trait]
    impl MiddlewareFactory for UpperFactory {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(Upper))
        }
    }

    static UPPER_TABLE: ExportedVTable =
        build_vtable::<NoEndpoint, UpperFactory>("upper", "0.0.0", CAPABILITIES_MIDDLEWARE_ONLY);

    static REUSE_TABLE: ExportedVTable = build_vtable::<NoEndpoint, PassthroughFactory>(
        "reuse-passthrough",
        "0.0.0",
        CAPABILITIES_MIDDLEWARE_ONLY,
    );

    /// Records what it is sent; fails every message when `fail` is set.
    #[derive(Default)]
    struct Recording {
        sent: Arc<std::sync::Mutex<Vec<CanonicalMessage>>>,
        fail: bool,
    }

    #[async_trait]
    impl MessagePublisher for Recording {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            if !self.fail {
                self.sent.lock().unwrap().extend(messages);
                return Ok(SentBatch::Ack);
            }
            self.sent.lock().unwrap().extend(messages.iter().cloned());
            let failed = messages
                .into_iter()
                .map(|m| (m, PublisherError::Retryable(anyhow!("down"))))
                .collect();
            Ok(SentBatch::Partial {
                responses: None,
                failed,
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_message_reaches_the_sink_without_a_copy() {
        unsafe { crate::plugin::register_plugin_table(REUSE_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("reuse-passthrough").unwrap();
        let sink = Recording::default();
        let sent = Arc::clone(&sink.sent);
        let publisher = factory
            .apply_publisher(Box::new(sink), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let mut message = CanonicalMessage::new(b"body".to_vec(), None);
        message.set_metadata("kind", "order");
        let payload = message.payload.as_ptr();
        let key = message.metadata_map().keys().next().unwrap().as_ptr();
        publisher.send_batch(vec![message]).await.unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].payload.as_ptr(), payload);
        assert_eq!(sent[0].metadata_map().keys().next().unwrap().as_ptr(), key);
    }

    /// Reads the batch in place: drops `kind=drop`, keeps `keep`, uppercases the rest.
    #[derive(Default)]
    struct InPlace;

    #[async_trait]
    impl BatchFilter for InPlace {
        async fn on_send_ref(&self, messages: &[MessageRef<'_>]) -> anyhow::Result<Vec<Verdict>> {
            Ok(messages
                .iter()
                .map(
                    |message| match (message.metadata("kind"), message.payload()) {
                        (Some("drop"), _) => Verdict::Drop,
                        (_, b"keep") => Verdict::Keep,
                        (_, payload) => {
                            let mut copy = message.to_message();
                            copy.payload = payload.to_ascii_uppercase().into();
                            Verdict::Replace(copy)
                        }
                    },
                )
                .collect())
        }
    }

    #[async_trait]
    impl MiddlewareFactory for InPlace {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(InPlace))
        }
    }

    static IN_PLACE_TABLE: ExportedVTable =
        build_vtable::<NoEndpoint, InPlace>("in-place", "0.0.0", CAPABILITIES_MIDDLEWARE_ONLY);

    #[tokio::test(flavor = "multi_thread")]
    async fn verdicts_on_a_borrowed_batch_keep_drop_and_replace() {
        unsafe { crate::plugin::register_plugin_table(IN_PLACE_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("in-place").unwrap();
        let sink = Recording::default();
        let sent = Arc::clone(&sink.sent);
        let publisher = factory
            .apply_publisher(Box::new(sink), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let keep = CanonicalMessage::new(b"keep".to_vec(), None);
        let kept = keep.payload.as_ptr();
        let dropped = CanonicalMessage::new(b"x".to_vec(), None).with_metadata_kv("kind", "drop");
        let changed = CanonicalMessage::new(b"abc".to_vec(), None).with_metadata_kv("k", "v");
        publisher
            .send_batch(vec![keep, dropped, changed])
            .await
            .unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].payload.as_ptr(), kept);
        assert_eq!(&sent[1].payload[..], b"ABC");
        assert_eq!(sent[1].get_metadata("k"), Some("v"));
    }

    /// Waits before it answers, so the call completes through the callback.
    #[derive(Default)]
    struct Slow;

    #[async_trait]
    impl BatchFilter for Slow {
        async fn on_send_ref(&self, messages: &[MessageRef<'_>]) -> anyhow::Result<Vec<Verdict>> {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(messages
                .iter()
                .map(|message| {
                    let mut copy = message.to_message();
                    copy.payload = message.payload().to_ascii_uppercase().into();
                    Verdict::Replace(copy)
                })
                .collect())
        }
    }

    #[async_trait]
    impl MiddlewareFactory for Slow {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(Slow))
        }
    }

    static SLOW_TABLE: ExportedVTable =
        build_vtable::<NoEndpoint, Slow>("slow", "0.0.0", CAPABILITIES_MIDDLEWARE_ONLY);

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_call_completes_and_an_abandoned_one_is_released() {
        unsafe { crate::plugin::register_plugin_table(SLOW_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("slow").unwrap();
        let sink = Recording::default();
        let sent = Arc::clone(&sink.sent);
        let publisher = factory
            .apply_publisher(Box::new(sink), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            publisher.send_batch(vec![CanonicalMessage::from("late")]),
        )
        .await;
        assert!(abandoned.is_err());
        publisher
            .send_batch(vec![CanonicalMessage::from("abc")])
            .await
            .unwrap();
        // Lets the abandoned call complete and release its result.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0].payload[..], b"ABC");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_send_hands_back_the_message_as_it_came_in() {
        unsafe { crate::plugin::register_plugin_table(UPPER_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("upper").unwrap();
        let sink = Recording {
            fail: true,
            ..Default::default()
        };
        let sent = Arc::clone(&sink.sent);
        let publisher = factory
            .apply_publisher(Box::new(sink), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let outcome = publisher
            .send_batch(vec![CanonicalMessage::new(b"abc".to_vec(), None)])
            .await
            .unwrap();

        assert_eq!(&sent.lock().unwrap()[0].payload[..], b"ABC");
        let SentBatch::Partial { failed, .. } = outcome else {
            panic!("expected the send to fail");
        };
        assert_eq!(&failed[0].0.payload[..], b"abc");
    }

    #[test]
    fn failed_messages_sharing_an_id_get_their_own_source_back() {
        let original = |payload: &'static [u8], rewritten: &'static [u8]| Original {
            message_id: 7,
            payload: Some(Bytes::from_static(payload)),
            metadata: None,
            rewritten: Bytes::from_static(rewritten),
        };
        let fail = |payload: &[u8]| {
            (
                CanonicalMessage::new(payload.to_vec(), Some(7)),
                PublisherError::Retryable(anyhow!("down")),
            )
        };
        let mut failed = vec![fail(b"B"), fail(b"A")];
        restore_originals(
            vec![original(b"a", b"A"), original(b"b", b"B")],
            &mut failed,
        );
        assert_eq!(&failed[0].0.payload[..], b"b");
        assert_eq!(&failed[1].0.payload[..], b"a");
    }

    /// Drops a request marked `drop`; fails the whole batch if one is marked `fail`.
    struct LookupFilter;

    #[async_trait]
    impl BatchFilter for LookupFilter {
        async fn on_send(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
            if messages.iter().any(|m| &m.payload[..] == b"fail") {
                return Err(anyhow!("failed"));
            }
            Ok(messages
                .into_iter()
                .map(|message| (&message.payload[..] != b"drop").then_some(message))
                .collect())
        }
    }

    #[derive(Default)]
    struct LookupFilterFactory;

    #[async_trait]
    impl MiddlewareFactory for LookupFilterFactory {
        async fn create(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn BatchFilter>> {
            Ok(Box::new(LookupFilter))
        }
    }

    static LOOKUP_TABLE: ExportedVTable = build_vtable::<NoEndpoint, LookupFilterFactory>(
        "lookup-filter",
        "0.0.0",
        CAPABILITIES_MIDDLEWARE_ONLY,
    );

    /// Answers each lookup with its request's payload, all in one call.
    struct Echo;

    #[async_trait]
    impl MessagePublisher for Echo {
        async fn send_batch(&self, _: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
            panic!("a batched lookup must not fall back to send");
        }

        async fn lookup_each(
            &self,
            requests: &[CanonicalMessage],
        ) -> Option<Vec<Result<Option<serde_json::Value>, PublisherError>>> {
            let echo = |r: &CanonicalMessage| Ok(Some(String::from_utf8_lossy(&r.payload).into()));
            Some(requests.iter().map(echo).collect())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lookup_reaches_the_wrapped_endpoint_as_one_batch() {
        unsafe { crate::plugin::register_plugin_table(LOOKUP_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("lookup-filter").unwrap();
        let publisher = factory
            .apply_publisher(Box::new(Echo), "r", &serde_json::json!({}))
            .await
            .unwrap();
        let requests = |payloads: &[&str]| -> Vec<CanonicalMessage> {
            payloads
                .iter()
                .map(|p| CanonicalMessage::new(p.as_bytes().to_vec(), None))
                .collect()
        };
        let answers = publisher
            .lookup_each(&requests(&["a", "drop", "b"]))
            .await
            .unwrap();
        assert_eq!(answers[0].as_ref().unwrap(), &Some("a".into()));
        assert!(answers[1].is_err(), "a dropped request has no answer");
        assert_eq!(answers[2].as_ref().unwrap(), &Some("b".into()));
        assert!(
            publisher
                .lookup_each(&requests(&["a", "fail"]))
                .await
                .is_none(),
            "a failed batch must fall back to one request at a time"
        );
    }

    /// A cumulative-ack source: one message per batch, recording the commit order.
    struct Ordered {
        payloads: std::collections::VecDeque<&'static str>,
        committed: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl MessageConsumer for Ordered {
        async fn receive_batch(&mut self, _: usize) -> Result<ReceivedBatch, ConsumerError> {
            let Some(payload) = self.payloads.pop_front() else {
                return Err(ConsumerError::EndOfStream);
            };
            let committed = Arc::clone(&self.committed);
            Ok(ReceivedBatch {
                messages: vec![CanonicalMessage::new(payload.as_bytes().to_vec(), None)],
                commit: Box::new(move |_| {
                    Box::pin(async move {
                        committed.lock().unwrap().push(payload);
                        Ok(())
                    })
                }),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_emptied_batch_is_not_acked_ahead_of_an_uncommitted_one() {
        unsafe { crate::plugin::register_plugin_table(DROP_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("drop-marked").unwrap();
        let committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = Ordered {
            payloads: ["first", "drop", "second"].into(),
            committed: Arc::clone(&committed),
        };
        let mut consumer = factory
            .apply_consumer(Box::new(inner), "r", &serde_json::json!({}))
            .await
            .unwrap();

        let first = consumer.receive_batch(10).await.unwrap();
        let second = consumer.receive_batch(10).await.unwrap();
        assert!(
            committed.lock().unwrap().is_empty(),
            "the dropped batch was acked while `first` is still uncommitted"
        );
        (first.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        (second.commit)(vec![MessageDisposition::Ack])
            .await
            .unwrap();
        assert_eq!(*committed.lock().unwrap(), ["first", "drop", "second"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_wrapped_endpoints_hooks_and_status_pass_through() {
        unsafe { crate::plugin::register_plugin_table(TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("hook-passthrough").unwrap();
        let config = serde_json::json!({});

        let inner = Hooked::default();
        let (connects, disconnects) = (inner.connects.clone(), inner.disconnects.clone());
        let consumer = factory
            .apply_consumer(Box::new(inner), "r", &config)
            .await
            .unwrap();
        consumer
            .on_connect_hook()
            .expect("connect hook")
            .await
            .unwrap();
        consumer
            .on_disconnect_hook()
            .expect("disconnect hook")
            .await
            .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert_eq!(disconnects.load(Ordering::SeqCst), 1);
        assert!(!consumer.status().await.healthy);

        let inner = Hooked::default();
        let (connects, disconnects) = (inner.connects.clone(), inner.disconnects.clone());
        let publisher = factory
            .apply_publisher(Box::new(inner), "r", &config)
            .await
            .unwrap();
        publisher
            .on_connect_hook()
            .expect("connect hook")
            .await
            .unwrap();
        publisher
            .on_disconnect_hook()
            .expect("disconnect hook")
            .await
            .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert_eq!(disconnects.load(Ordering::SeqCst), 1);
        assert!(!publisher.status().await.healthy);
    }

    /// Marks each message it sees, to show the wrapped endpoint was driven through it.
    struct Tagging<T> {
        inner: T,
        tag: &'static str,
    }

    #[async_trait]
    impl MessageConsumer for Tagging<Box<dyn MessageConsumer>> {
        async fn receive_batch(&mut self, max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let mut batch = self.inner.receive_batch(max).await?;
            for message in &mut batch.messages {
                message.set_metadata(self.tag, "yes");
            }
            Ok(batch)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait]
    impl MessagePublisher for Tagging<Box<dyn MessagePublisher>> {
        async fn send_batch(
            &self,
            mut messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            for message in &mut messages {
                message.set_metadata(self.tag, "yes");
            }
            self.inner.send_batch(messages).await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug, Default)]
    struct TaggingFactory;

    #[async_trait]
    impl CustomMiddlewareFactory for TaggingFactory {
        async fn apply_consumer(
            &self,
            inner: Box<dyn MessageConsumer>,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(Box::new(Tagging {
                inner,
                tag: "received_through",
            }))
        }

        async fn apply_publisher(
            &self,
            inner: Box<dyn MessagePublisher>,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(Tagging {
                inner,
                tag: "sent_through",
            }))
        }
    }

    static WRAP_TABLE: ExportedVTable =
        crate::plugin::sdk::build_wrap_vtable::<TaggingFactory>("wrap-tagging", "0.0.0");

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrapping_middleware_drives_the_lent_endpoint() {
        use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
        unsafe { crate::plugin::register_plugin_table(WRAP_TABLE.as_ptr()) }.unwrap();
        let factory = get_middleware_factory("wrap-tagging").unwrap();
        let config = serde_json::json!({});

        let inner = MemoryPublisher::new_local("plugin_wrap_round_trip", 16);
        let publisher = factory
            .apply_publisher(Box::new(inner), "r", &config)
            .await
            .unwrap();
        let messages = (0..3)
            .map(|i| CanonicalMessage::new(format!("m{i}").into_bytes(), None))
            .collect();
        assert!(matches!(
            publisher.send_batch(messages).await,
            Ok(SentBatch::Ack)
        ));

        let inner = MemoryConsumer::new_local("plugin_wrap_round_trip", 16);
        let mut consumer = factory
            .apply_consumer(Box::new(inner), "r", &config)
            .await
            .unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 3);
        for message in &batch.messages {
            assert_eq!(message.get_metadata("sent_through"), Some("yes"));
            assert_eq!(message.get_metadata("received_through"), Some("yes"));
        }
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        (batch.commit)(acks).await.unwrap();
        consumer.close().await.unwrap();
    }
}
