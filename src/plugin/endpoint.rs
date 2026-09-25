//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Safe wrappers that present a loaded plugin's function table as ordinary
//! mq-bridge endpoints.
//!
//! Every ABI call is blocking by contract, so each one runs on
//! [`tokio::task::spawn_blocking`] rather than on the async executor. The
//! exceptions are the two cheap consumer queries the ABI marks as non-blocking
//! (`commit_requires_order`, `set_exit_on_empty`), and a 1.2 plugin's receive,
//! commit, send and flush, which complete through a callback instead. A plugin
//! whose non-blocking entry answers `MQB_ERR_UNSUPPORTED` gets the blocking one.
//!
//! Acknowledgement stays under the route's control: `receive_batch` hands back
//! a batch handle wrapped in [`PluginBatch`], and the plugin only learns the
//! dispositions when the route invokes the batch commit function. A batch that
//! is dropped without being committed is released without acknowledging
//! anything, so the broker can redeliver.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::support::plugin_abi::{
    MqbAsyncHooks, MqbBatchHandle, MqbBuffer, MqbConsumerHandle, MqbMessage, MqbPublisherHandle,
    MqbResponsesHandle, MqbSlice, MqbStatus, MqbStatusHooks, MQB_DELIVERY_ACKNOWLEDGES,
    MQB_DELIVERY_IDEMPOTENT_SINK, MQB_DISPOSITION_ACK, MQB_DISPOSITION_NACK, MQB_DISPOSITION_REPLY,
    MQB_END_OF_STREAM, MQB_ERR_CONNECTION, MQB_ERR_INVALID_CONFIG, MQB_ERR_PANIC,
    MQB_ERR_PERMANENT, MQB_ERR_RETRYABLE, MQB_ERR_UNSUPPORTED, MQB_OK, MQB_OUTCOME_OK,
    MQB_OUTCOME_PERMANENT,
};
use anyhow::anyhow;
use async_trait::async_trait;

use super::{completion, LoadedPlugin};
use crate::errors::{ConsumerError, PublisherError};
use crate::traits::{
    schema_flag, BatchCommitFunc, CustomEndpointFactory, EndpointStatus, MessageConsumer,
    MessageDisposition, MessagePublisher,
};
use crate::{CanonicalMessage, ReceivedBatch, SentBatch};

/// Moves a plugin handle across a `spawn_blocking` boundary.
///
/// The ABI requires handles to be usable from any thread; the raw pointer they
/// wrap is what makes them non-`Send` to the compiler.
pub(super) struct AssertSend<T>(pub(super) T);
unsafe impl<T> Send for AssertSend<T> {}

pub(super) fn join_error(err: tokio::task::JoinError) -> anyhow::Error {
    anyhow!("plugin call did not complete: {err}")
}

/// A [`CustomEndpointFactory`] backed by a loaded plugin's function table.
pub struct PluginEndpointFactory {
    plugin: Arc<LoadedPlugin>,
}

impl PluginEndpointFactory {
    pub(crate) fn new(plugin: Arc<LoadedPlugin>) -> Self {
        Self { plugin }
    }

    /// The plugin's `MQB_DELIVERY_*` answer for `config`, or `None` when it
    /// predates 1.2 or fails, leaving the schema defaults in charge.
    fn delivery_flags(&self, config: &serde_json::Value) -> Option<u8> {
        let hook = self.plugin.table().delivery_hook()?;
        let config = config.to_string();
        let mut flags = 0;
        let mut error = MqbBuffer::EMPTY;
        let status = unsafe {
            hook(
                self.plugin.factory(),
                MqbSlice::from_str(&config),
                &mut flags,
                &mut error,
            )
        };
        if status != MQB_OK {
            tracing::warn!(
                plugin = %self.plugin.name(),
                error = %self.plugin.take_error(error),
                "plugin could not report its delivery guarantees; using its schema"
            );
            return None;
        }
        Some(flags)
    }
}

impl std::fmt::Debug for PluginEndpointFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEndpointFactory")
            .field("name", &self.plugin.name())
            .field("version", &self.plugin.info.version)
            .field("path", &self.plugin.info.path)
            .finish()
    }
}

#[async_trait]
impl CustomEndpointFactory for PluginEndpointFactory {
    fn config_schema(&self) -> Option<serde_json::Value> {
        self.plugin.info.endpoint_schema()
    }

    fn idempotent_sink(&self, config: &serde_json::Value) -> bool {
        match self.delivery_flags(config) {
            Some(flags) => flags & MQB_DELIVERY_IDEMPOTENT_SINK != 0,
            None => schema_flag(self.config_schema(), "x-mqb-idempotent-sink").unwrap_or(false),
        }
    }

    fn acknowledges(&self, config: &serde_json::Value) -> bool {
        match self.delivery_flags(config) {
            Some(flags) => flags & MQB_DELIVERY_ACKNOWLEDGES != 0,
            None => schema_flag(self.config_schema(), "x-mqb-acknowledges").unwrap_or(true),
        }
    }

    async fn create_consumer(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        if !self.plugin.info.supports_consumer {
            return Err(anyhow!(
                "endpoint plugin `{}` does not provide input endpoints",
                self.plugin.name()
            ));
        }
        let plugin = Arc::clone(&self.plugin);
        let route = route_name.to_owned();
        let config = serde_json::to_vec(config)?;
        let handle = tokio::task::spawn_blocking(move || {
            let mut out = MqbConsumerHandle::NULL;
            let mut err = MqbBuffer::EMPTY;
            let status = unsafe {
                (plugin.table().consumer_create)(
                    plugin.factory(),
                    MqbSlice::from_str(&route),
                    MqbSlice::from_bytes(&config),
                    &mut out,
                    &mut err,
                )
            };
            if status == MQB_OK {
                Ok(AssertSend(out))
            } else {
                // Classify rather than flattening to a string: a plugin that
                // rejected its config reports MQB_ERR_INVALID_CONFIG, and the
                // route only breaks out of its reconnect loop for the error
                // classes it can downcast back out of this `anyhow`.
                let cause = anyhow!(
                    "endpoint plugin `{}` could not open an input for route `{route}`: {}",
                    plugin.name(),
                    plugin.take_error(err)
                );
                Err(anyhow::Error::new(consumer_error_from(status, cause)))
            }
        })
        .await
        .map_err(join_error)??;

        Ok(Box::new(PluginConsumer {
            consumer: Arc::new(ConsumerHandle {
                plugin: Arc::clone(&self.plugin),
                handle: handle.0,
                blocking: AtomicBool::new(false),
            }),
        }))
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        if !self.plugin.info.supports_publisher {
            return Err(anyhow!(
                "endpoint plugin `{}` does not provide output endpoints",
                self.plugin.name()
            ));
        }
        let plugin = Arc::clone(&self.plugin);
        let route = route_name.to_owned();
        let config = serde_json::to_vec(config)?;
        let handle = tokio::task::spawn_blocking(move || {
            let mut out = MqbPublisherHandle::NULL;
            let mut err = MqbBuffer::EMPTY;
            let status = unsafe {
                (plugin.table().publisher_create)(
                    plugin.factory(),
                    MqbSlice::from_str(&route),
                    MqbSlice::from_bytes(&config),
                    &mut out,
                    &mut err,
                )
            };
            if status == MQB_OK {
                Ok(AssertSend(out))
            } else {
                // Classified for the same reason as the consumer side above.
                let cause = anyhow!(
                    "endpoint plugin `{}` could not open an output for route `{route}`: {}",
                    plugin.name(),
                    plugin.take_error(err)
                );
                Err(anyhow::Error::new(publisher_error_from(status, cause)))
            }
        })
        .await
        .map_err(join_error)??;

        Ok(Box::new(PluginPublisher {
            publisher: Arc::new(PublisherHandle {
                plugin: Arc::clone(&self.plugin),
                handle: handle.0,
                blocking: AtomicBool::new(false),
            }),
        }))
    }
}

/// One received batch, still owned by the plugin.
///
/// Committing consumes the handle; dropping an uncommitted batch releases it
/// without acknowledging, leaving redelivery to the broker.
pub(crate) struct PluginBatch {
    plugin: Arc<LoadedPlugin>,
    handle: MqbBatchHandle,
}

unsafe impl Send for PluginBatch {}

impl PluginBatch {
    fn commit(mut self, dispositions: Vec<MessageDisposition>) -> anyhow::Result<()> {
        let handle = std::mem::replace(&mut self.handle, MqbBatchHandle::NULL);
        if handle.is_null() {
            return Ok(());
        }
        let len = dispositions.len();
        let mut err = MqbBuffer::EMPTY;
        let replies_hook = self.plugin.table().request_reply_hooks().filter(|_| {
            dispositions
                .iter()
                .any(|disposition| matches!(disposition, MessageDisposition::Reply(_)))
        });
        let status = match replies_hook {
            Some(hooks) => {
                let (codes, replies) = reply_dispositions(dispositions);
                unsafe {
                    (hooks.batch_commit_replies)(
                        handle,
                        codes.as_ptr(),
                        replies.as_ptr(),
                        len,
                        &mut err,
                    )
                }
            }
            None => {
                let codes: Vec<u8> = dispositions.iter().map(disposition_code).collect();
                unsafe { (self.plugin.table().batch_commit)(handle, codes.as_ptr(), len, &mut err) }
            }
        };
        if status == MQB_OK {
            Ok(())
        } else {
            Err(anyhow!(
                "endpoint plugin `{}` failed to commit a batch of {} messages: {}",
                self.plugin.name(),
                len,
                self.plugin.take_error(err)
            ))
        }
    }
}

impl PluginBatch {
    /// The 1.2 commit: awaited on the plugin's runtime, no blocking thread.
    async fn commit_async(
        mut self,
        hooks: MqbAsyncHooks,
        dispositions: Vec<MessageDisposition>,
    ) -> anyhow::Result<()> {
        let len = dispositions.len();
        let pending = {
            let handle = std::mem::replace(&mut self.handle, MqbBatchHandle::NULL);
            if handle.is_null() {
                return Ok(());
            }
            // Replies cost a placeholder per message, so only when there are any.
            let has_replies = dispositions
                .iter()
                .any(|disposition| matches!(disposition, MessageDisposition::Reply(_)));
            let (codes, replies) = if has_replies {
                let (codes, replies) = reply_dispositions(dispositions);
                (codes, Some(replies))
            } else {
                (dispositions.iter().map(disposition_code).collect(), None)
            };
            let replies = replies
                .as_ref()
                .map_or(std::ptr::null(), |replies| replies.as_ptr());
            let slot = ErrSlot {
                plugin: Arc::clone(&self.plugin),
                err: MqbBuffer::EMPTY,
                _owner: (),
            };
            completion::call(slot, |slot, completion| unsafe {
                (hooks.batch_commit)(
                    handle,
                    codes.as_ptr(),
                    replies,
                    len,
                    std::ptr::addr_of_mut!((*slot).err),
                    completion,
                )
            })
        };
        let (status, slot) = pending.finish().await?;
        if status == MQB_OK {
            return Ok(());
        }
        Err(anyhow!(
            "endpoint plugin `{}` failed to commit a batch of {len} messages: {}",
            self.plugin.name(),
            self.plugin.take_error(slot.err)
        ))
    }
}

/// Error-only out-parameter of a 1.2 call, holding `owner` alive until it ends.
struct ErrSlot<O> {
    plugin: Arc<LoadedPlugin>,
    err: MqbBuffer,
    _owner: O,
}

unsafe impl<O: Send> Send for ErrSlot<O> {}

impl<O: Send + 'static> completion::Slots for ErrSlot<O> {
    fn abandon(self) {
        let _ = self.plugin.take_error(self.err);
    }
}

struct ReceiveSlots {
    consumer: Arc<ConsumerHandle>,
    batch: MqbBatchHandle,
    messages: *const MqbMessage,
    len: usize,
    err: MqbBuffer,
}

unsafe impl Send for ReceiveSlots {}

impl completion::Slots for ReceiveSlots {
    fn abandon(self) {
        let plugin = &self.consumer.plugin;
        if !self.batch.is_null() {
            // Released unacknowledged, as a dropped `PluginBatch` would be.
            unsafe { (plugin.table().batch_free)(self.batch) };
        }
        let _ = plugin.take_error(self.err);
    }
}

struct SendSlots {
    publisher: Arc<PublisherHandle>,
    outcomes: Vec<u8>,
    result: MqbResponsesHandle,
    responses: *const MqbMessage,
    responses_len: usize,
    err: MqbBuffer,
}

unsafe impl Send for SendSlots {}

impl SendSlots {
    /// Copies the responses out and releases the plugin's result.
    fn take_responses(&mut self) -> Vec<CanonicalMessage> {
        let plugin = &self.publisher.plugin;
        let responses = unsafe { super::message::from_abi(self.responses, self.responses_len) };
        let result = std::mem::replace(&mut self.result, MqbResponsesHandle::NULL);
        if let (false, Some(hooks)) = (result.is_null(), plugin.table().request_reply_hooks()) {
            unsafe { (hooks.responses_free)(result) };
        }
        responses
    }
}

impl completion::Slots for SendSlots {
    fn abandon(mut self) {
        let _ = self.take_responses();
        let _ = self.publisher.plugin.take_error(self.err);
    }
}

impl Drop for PluginBatch {
    fn drop(&mut self) {
        let handle = std::mem::replace(&mut self.handle, MqbBatchHandle::NULL);
        if handle.is_null() {
            return;
        }
        let plugin = Arc::clone(&self.plugin);
        let handle = AssertSend(handle);
        // `batch_free` is a blocking ABI call like any other, so keep it off the
        // executor when a drop happens inside async code.
        blocking_cleanup(move || {
            // Bind the wrapper itself: capturing only its field would move a
            // bare, non-`Send` handle into the task.
            let handle = handle;
            unsafe { (plugin.table().batch_free)(handle.0) };
        });
    }
}

/// Runs a blocking ABI cleanup call, off the executor when one is available.
///
/// `Drop` cannot await, so this is fire-and-forget: the work is handed to the
/// blocking pool and the handle it captures stays alive until it runs.
fn blocking_cleanup(cleanup: impl FnOnce() + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(cleanup);
        }
        Err(_) => cleanup(),
    }
}

/// Owns a plugin consumer handle and frees it exactly once, when the last user
/// lets go.
///
/// Refcounted rather than owned outright because an ABI call runs on a blocking
/// task that outlives cancellation: a route cancelled mid-`receive_batch` drops
/// the consumer while the plugin is still inside the call, and freeing the
/// handle there would pull the state out from under it.
struct ConsumerHandle {
    plugin: Arc<LoadedPlugin>,
    handle: MqbConsumerHandle,
    /// Set once the non-blocking receive answered `MQB_ERR_UNSUPPORTED`.
    blocking: AtomicBool,
}

unsafe impl Send for ConsumerHandle {}
unsafe impl Sync for ConsumerHandle {}

impl ConsumerHandle {
    fn async_hooks(&self) -> Option<MqbAsyncHooks> {
        if self.blocking.load(Ordering::Relaxed) {
            return None;
        }
        self.plugin.table().async_hooks()
    }
}

impl Drop for ConsumerHandle {
    fn drop(&mut self) {
        let plugin = Arc::clone(&self.plugin);
        let handle = self.handle.0 as usize;
        blocking_cleanup(move || unsafe {
            (plugin.table().consumer_free)(MqbConsumerHandle(handle as *mut _))
        });
    }
}

struct PluginConsumer {
    consumer: Arc<ConsumerHandle>,
}

#[async_trait]
impl MessageConsumer for PluginConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // The clone is what keeps the handle alive for the whole call.
        let consumer = Arc::clone(&self.consumer);
        let mut async_hooks = consumer.async_hooks();
        let received = match async_hooks {
            Some(hooks) => receive_async(Arc::clone(&consumer), hooks, max_messages).await?,
            None => None,
        };
        let (batch, messages) = match received {
            Some(received) => received,
            None => {
                async_hooks = None;
                receive_blocking(consumer, max_messages).await?
            }
        };
        let expected = messages.len();
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                if dispositions.len() != expected {
                    return Err(anyhow!(
                        "plugin batch commit received {} dispositions for {expected} messages",
                        dispositions.len()
                    ));
                }
                match async_hooks {
                    Some(hooks) => batch.commit_async(hooks, dispositions).await,
                    None => tokio::task::spawn_blocking(move || batch.commit(dispositions))
                        .await
                        .map_err(join_error)?,
                }
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn commit_requires_order(&self) -> bool {
        let consumer = &self.consumer;
        unsafe { (consumer.plugin.table().consumer_commit_requires_order)(consumer.handle) != 0 }
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        let consumer = &self.consumer;
        unsafe {
            (consumer.plugin.table().consumer_set_exit_on_empty)(
                consumer.handle,
                exit_on_empty.into(),
            )
        };
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        let consumer = Arc::clone(&self.consumer);
        tokio::task::spawn_blocking(move || {
            let mut err = MqbBuffer::EMPTY;
            let status =
                unsafe { (consumer.plugin.table().consumer_close)(consumer.handle, &mut err) };
            plugin_result(&consumer.plugin, status, err, "close an input")
        })
        .await
        .map_err(join_error)?
    }

    async fn status(&self) -> EndpointStatus {
        let consumer = Arc::clone(&self.consumer);
        let plugin = Arc::clone(&consumer.plugin);
        plugin_status(plugin, move |hooks, out, err| unsafe {
            (hooks.consumer_status)(consumer.handle, out, err)
        })
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn receive_blocking(
    consumer: Arc<ConsumerHandle>,
    max_messages: usize,
) -> Result<(PluginBatch, Vec<CanonicalMessage>), ConsumerError> {
    let received = tokio::task::spawn_blocking(move || {
        let plugin = Arc::clone(&consumer.plugin);
        let mut batch = MqbBatchHandle::NULL;
        let mut messages: *const MqbMessage = std::ptr::null();
        let mut len: usize = 0;
        let mut err = MqbBuffer::EMPTY;
        let status = unsafe {
            (plugin.table().consumer_receive_batch)(
                consumer.handle,
                max_messages,
                &mut batch,
                &mut messages,
                &mut len,
                &mut err,
            )
        };
        // Take ownership even when the plugin wrote a handle before failing.
        let guard = PluginBatch {
            plugin: Arc::clone(&plugin),
            handle: batch,
        };
        if status != MQB_OK {
            return Err(consumer_error(&plugin, status, err, "receive a batch"));
        }
        let messages = unsafe { super::message::from_abi(messages, len) };
        Ok(AssertSend((guard, messages)))
    })
    .await
    .map_err(|err| ConsumerError::Connection(join_error(err)))??;
    Ok(received.0)
}

/// The 1.2 receive. Dropping it mid-call releases the batch unacknowledged.
/// `None` means the plugin has no non-blocking receive.
async fn receive_async(
    consumer: Arc<ConsumerHandle>,
    hooks: MqbAsyncHooks,
    max_messages: usize,
) -> Result<Option<(PluginBatch, Vec<CanonicalMessage>)>, ConsumerError> {
    let handle = AssertSend(consumer.handle);
    let slots = ReceiveSlots {
        consumer,
        batch: MqbBatchHandle::NULL,
        messages: std::ptr::null(),
        len: 0,
        err: MqbBuffer::EMPTY,
    };
    let pending = completion::call(slots, |slots, completion| unsafe {
        (hooks.receive_batch)(
            handle.0,
            max_messages,
            std::ptr::addr_of_mut!((*slots).batch),
            std::ptr::addr_of_mut!((*slots).messages),
            std::ptr::addr_of_mut!((*slots).len),
            std::ptr::addr_of_mut!((*slots).err),
            completion,
        )
    });
    let (status, slots) = pending.finish().await.map_err(ConsumerError::Connection)?;
    let plugin = &slots.consumer.plugin;
    let guard = PluginBatch {
        plugin: Arc::clone(plugin),
        handle: slots.batch,
    };
    if status == MQB_ERR_UNSUPPORTED {
        slots.consumer.blocking.store(true, Ordering::Relaxed);
        let _ = plugin.take_error(slots.err);
        return Ok(None);
    }
    if status != MQB_OK {
        return Err(consumer_error(plugin, status, slots.err, "receive a batch"));
    }
    let messages = unsafe { super::message::from_abi(slots.messages, slots.len) };
    Ok(Some((guard, messages)))
}

/// Owns a plugin publisher handle, refcounted for the same reason as
/// [`ConsumerHandle`]: a send may still be inside the plugin when the route
/// that owns the publisher goes away.
struct PublisherHandle {
    plugin: Arc<LoadedPlugin>,
    handle: MqbPublisherHandle,
    /// Set once the non-blocking send answered `MQB_ERR_UNSUPPORTED`.
    blocking: AtomicBool,
}

unsafe impl Send for PublisherHandle {}
unsafe impl Sync for PublisherHandle {}

impl PublisherHandle {
    fn async_hooks(&self) -> Option<MqbAsyncHooks> {
        if self.blocking.load(Ordering::Relaxed) {
            return None;
        }
        self.plugin.table().async_hooks()
    }
}

impl Drop for PublisherHandle {
    fn drop(&mut self) {
        let plugin = Arc::clone(&self.plugin);
        let handle = AssertSend(self.handle);
        // `publisher_close` can talk to the broker, so it must not run on the
        // executor when the publisher is dropped from async code.
        blocking_cleanup(move || {
            // See `PluginBatch::drop`: the wrapper is what carries `Send`.
            let handle = handle;
            let mut err = MqbBuffer::EMPTY;
            // Best effort: a route that shuts down without flushing still gives
            // the plugin a chance to release broker-side resources.
            let status = unsafe { (plugin.table().publisher_close)(handle.0, &mut err) };
            if status != MQB_OK {
                tracing::warn!(
                    endpoint = plugin.name(),
                    "endpoint plugin failed to close an output: {}",
                    plugin.take_error(err)
                );
            }
            unsafe { (plugin.table().publisher_free)(handle.0) };
        });
    }
}

struct PluginPublisher {
    publisher: Arc<PublisherHandle>,
}

#[async_trait]
impl MessagePublisher for PluginPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        let publisher = Arc::clone(&self.publisher);
        let messages = match publisher.async_hooks() {
            Some(hooks) => match send_async(Arc::clone(&publisher), hooks, messages).await {
                Ok(sent) => return sent,
                Err(messages) => messages,
            },
            None => messages,
        };
        tokio::task::spawn_blocking(move || {
            let plugin = &publisher.plugin;
            let messages = super::message::AbiMessages::new(messages);
            let mut err = MqbBuffer::EMPTY;
            // A newer entry answering MQB_ERR_UNSUPPORTED falls through to the older one.
            if let Some(hooks) = plugin.table().request_reply_hooks() {
                let mut outcomes = vec![MQB_OUTCOME_OK; messages.len()];
                let mut result = MqbResponsesHandle::NULL;
                let mut responses: *const MqbMessage = std::ptr::null();
                let mut responses_len = 0usize;
                let status = unsafe {
                    (hooks.send_batch_responses)(
                        publisher.handle,
                        messages.as_ptr(),
                        messages.len(),
                        outcomes.as_mut_ptr(),
                        &mut result,
                        &mut responses,
                        &mut responses_len,
                        &mut err,
                    )
                };
                let responses = unsafe { super::message::from_abi(responses, responses_len) };
                if !result.is_null() {
                    unsafe { (hooks.responses_free)(result) };
                }
                if status != MQB_ERR_UNSUPPORTED {
                    return sent_with_responses(
                        plugin,
                        status,
                        err,
                        messages.into_messages(),
                        &outcomes,
                        responses,
                    );
                }
                let _ = plugin.take_error(std::mem::replace(&mut err, MqbBuffer::EMPTY));
            }
            if let Some(hook) = plugin.table().publisher_outcomes_hook() {
                // Host-allocated, one byte per message: the plugin writes back into
                // this, so no payload travels the other way.
                let mut outcomes = vec![MQB_OUTCOME_OK; messages.len()];
                let status = unsafe {
                    hook(
                        publisher.handle,
                        messages.as_ptr(),
                        messages.len(),
                        outcomes.as_mut_ptr(),
                        &mut err,
                    )
                };
                match status {
                    MQB_OK => return Ok(SentBatch::Ack),
                    MQB_ERR_UNSUPPORTED => {
                        let _ = plugin.take_error(std::mem::replace(&mut err, MqbBuffer::EMPTY));
                    }
                    _ => {
                        return publish_outcome(
                            plugin,
                            status,
                            err,
                            messages.into_messages(),
                            &outcomes,
                            None,
                        )
                    }
                }
            }
            let status = unsafe {
                (plugin.table().publisher_send_batch)(
                    publisher.handle,
                    messages.as_ptr(),
                    messages.len(),
                    &mut err,
                )
            };
            if status == MQB_OK {
                Ok(SentBatch::Ack)
            } else {
                Err(publisher_error(plugin, status, err, "publish a batch"))
            }
        })
        .await
        .map_err(|err| PublisherError::Retryable(join_error(err)))?
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let publisher = Arc::clone(&self.publisher);
        if let Some(hooks) = publisher.async_hooks() {
            let plugin = Arc::clone(&publisher.plugin);
            let handle = AssertSend(publisher.handle);
            let slot = ErrSlot {
                plugin,
                err: MqbBuffer::EMPTY,
                _owner: Arc::clone(&publisher),
            };
            let pending = completion::call(slot, |slot, completion| unsafe {
                (hooks.flush)(handle.0, std::ptr::addr_of_mut!((*slot).err), completion)
            });
            let (status, slot) = pending.finish().await?;
            if status != MQB_ERR_UNSUPPORTED {
                return plugin_result(&slot.plugin, status, slot.err, "flush an output");
            }
            let _ = slot.plugin.take_error(slot.err);
        }
        tokio::task::spawn_blocking(move || {
            let mut err = MqbBuffer::EMPTY;
            let status =
                unsafe { (publisher.plugin.table().publisher_flush)(publisher.handle, &mut err) };
            plugin_result(&publisher.plugin, status, err, "flush an output")
        })
        .await
        .map_err(join_error)?
    }

    async fn status(&self) -> EndpointStatus {
        let publisher = Arc::clone(&self.publisher);
        let plugin = Arc::clone(&publisher.plugin);
        plugin_status(plugin, move |hooks, out, err| unsafe {
            (hooks.publisher_status)(publisher.handle, out, err)
        })
        .await
    }

    /// Asks the plugin whether its sends must stay in source order.
    ///
    /// A plugin built against ABI 1.0 has no such entry, and answering `true`
    /// on its behalf would serialise every existing plugin sink. So it keeps
    /// the trait default, `false` — what those plugins already get today.
    fn requires_ordered_publish(&self) -> bool {
        let publisher = &self.publisher;
        match publisher.plugin.table().publisher_ordering_hook() {
            Some(hook) => unsafe { hook(publisher.handle) != 0 },
            None => false,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The 1.2 publish: awaited on the plugin's runtime, no blocking thread.
/// `Err` hands the messages back when the plugin has no non-blocking send.
async fn send_async(
    publisher: Arc<PublisherHandle>,
    hooks: MqbAsyncHooks,
    messages: Vec<CanonicalMessage>,
) -> Result<Result<SentBatch, PublisherError>, Vec<CanonicalMessage>> {
    let len = messages.len();
    let handle = AssertSend(publisher.handle);
    let (pending, messages) = {
        let messages = super::message::AbiMessages::new(messages);
        let slots = SendSlots {
            publisher,
            outcomes: vec![MQB_OUTCOME_OK; len],
            result: MqbResponsesHandle::NULL,
            responses: std::ptr::null(),
            responses_len: 0,
            err: MqbBuffer::EMPTY,
        };
        let pending = completion::call(slots, |slots, completion| unsafe {
            (hooks.send_batch)(
                handle.0,
                messages.as_ptr(),
                len,
                (*slots).outcomes.as_mut_ptr(),
                std::ptr::addr_of_mut!((*slots).result),
                std::ptr::addr_of_mut!((*slots).responses),
                std::ptr::addr_of_mut!((*slots).responses_len),
                std::ptr::addr_of_mut!((*slots).err),
                completion,
            )
        });
        // The plugin copied the input before returning.
        (pending, messages.into_messages())
    };
    let (status, mut slots) = match pending.finish().await {
        Ok(finished) => finished,
        Err(err) => return Ok(Err(PublisherError::Retryable(err))),
    };
    if status == MQB_ERR_UNSUPPORTED {
        slots.publisher.blocking.store(true, Ordering::Relaxed);
        completion::Slots::abandon(slots);
        return Err(messages);
    }
    let responses = slots.take_responses();
    Ok(sent_with_responses(
        &slots.publisher.plugin,
        status,
        slots.err,
        messages,
        &slots.outcomes,
        responses,
    ))
}

/// The outcome of a 1.2 publish, which may carry responses either way.
fn sent_with_responses(
    plugin: &LoadedPlugin,
    status: MqbStatus,
    err: MqbBuffer,
    messages: Vec<CanonicalMessage>,
    outcomes: &[u8],
    responses: Vec<CanonicalMessage>,
) -> Result<SentBatch, PublisherError> {
    let responses = (!responses.is_empty()).then_some(responses);
    if status == MQB_OK {
        return Ok(match responses {
            None => SentBatch::Ack,
            responses => SentBatch::Partial {
                responses,
                failed: Vec::new(),
            },
        });
    }
    publish_outcome(plugin, status, err, messages, outcomes, responses)
}

/// Turns a failed 1.1 publish into the outcome the route reacts to.
///
/// Only a *subset* of failures becomes [`SentBatch::Partial`]. When every
/// message failed — or the plugin marked none, which is a plugin that reported
/// an error it could not attribute — the whole-batch status is returned as it
/// is, so [`MQB_ERR_CONNECTION`] still reconnects the endpoint and nothing is
/// silently acknowledged.
fn publish_outcome(
    plugin: &LoadedPlugin,
    status: MqbStatus,
    err: MqbBuffer,
    messages: Vec<CanonicalMessage>,
    outcomes: &[u8],
    responses: Option<Vec<CanonicalMessage>>,
) -> Result<SentBatch, PublisherError> {
    let cause = plugin.take_error(err);
    let failures = outcomes
        .iter()
        .filter(|outcome| **outcome != MQB_OUTCOME_OK)
        .count();
    if failures == 0 || failures == messages.len() {
        return Err(publisher_error_from(
            status,
            anyhow!(
                "endpoint plugin `{}` failed to publish a batch: {cause}",
                plugin.name()
            ),
        ));
    }
    let failed = messages
        .into_iter()
        .zip(outcomes)
        .filter(|(_, outcome)| **outcome != MQB_OUTCOME_OK)
        .map(|(message, outcome)| {
            // One batch-level message by design; the byte carries the class. An
            // unrecognised byte is treated as retryable, as for a status code.
            let cause = anyhow!(
                "endpoint plugin `{}` failed to publish this message: {cause}",
                plugin.name()
            );
            let error = if *outcome == MQB_OUTCOME_PERMANENT {
                PublisherError::NonRetryable(cause)
            } else {
                PublisherError::Retryable(cause)
            };
            (message, error)
        })
        .collect();
    Ok(SentBatch::Partial { responses, failed })
}

/// Asks a 1.2 plugin for an endpoint's status. An older plugin, or one that
/// answers `MQB_ERR_UNSUPPORTED`, reports the trait default; a failed call
/// reports unhealthy with the plugin's error.
async fn plugin_status(
    plugin: Arc<LoadedPlugin>,
    call: impl FnOnce(MqbStatusHooks, *mut MqbBuffer, *mut MqbBuffer) -> MqbStatus + Send + 'static,
) -> EndpointStatus {
    let Some(hooks) = plugin.table().status_hooks() else {
        return EndpointStatus::default();
    };
    let fetched = tokio::task::spawn_blocking(move || {
        let mut out = MqbBuffer::EMPTY;
        let mut err = MqbBuffer::EMPTY;
        let status = call(hooks, &mut out, &mut err);
        if status == MQB_ERR_UNSUPPORTED {
            let _ = plugin.take_buffer(out);
            let _ = plugin.take_error(err);
            return Ok(EndpointStatus::default());
        }
        if status != MQB_OK {
            let _ = plugin.take_buffer(out);
            return Err(plugin_cause(&plugin, err, "report its status"));
        }
        let text = plugin.take_buffer_utf8(out)?;
        serde_json::from_str::<EndpointStatus>(&text).map_err(|e| {
            anyhow!(
                "endpoint plugin `{}` sent an invalid status: {e}",
                plugin.name()
            )
        })
    })
    .await
    .map_err(join_error)
    .and_then(|fetched| fetched);
    fetched.unwrap_or_else(|error| EndpointStatus {
        healthy: false,
        error: Some(format!("{error:#}")),
        ..Default::default()
    })
}

/// Encodes dispositions for `batch_commit_replies`, with a reply array parallel
/// to them; non-reply slots hold an empty placeholder the plugin never reads.
fn reply_dispositions(
    dispositions: Vec<MessageDisposition>,
) -> (Vec<u8>, super::message::AbiMessages) {
    let mut codes = Vec::with_capacity(dispositions.len());
    let replies = dispositions
        .into_iter()
        .map(|disposition| match disposition {
            MessageDisposition::Reply(reply) => {
                codes.push(MQB_DISPOSITION_REPLY);
                reply
            }
            other => {
                codes.push(disposition_code(&other));
                CanonicalMessage::new(Vec::new(), Some(0))
            }
        })
        .collect();
    (codes, super::message::AbiMessages::new(replies))
}

fn disposition_code(disposition: &MessageDisposition) -> u8 {
    match disposition {
        // Without the 1.2 reply entry, a reply still acknowledges the source
        // message, matching in-tree endpoints without request/reply support.
        MessageDisposition::Ack | MessageDisposition::Reply(_) => MQB_DISPOSITION_ACK,
        MessageDisposition::Nack => MQB_DISPOSITION_NACK,
    }
}

fn plugin_result(
    plugin: &LoadedPlugin,
    status: MqbStatus,
    err: MqbBuffer,
    action: &str,
) -> anyhow::Result<()> {
    if status == MQB_OK {
        Ok(())
    } else {
        Err(anyhow!(
            "endpoint plugin `{}` failed to {action}: {}",
            plugin.name(),
            plugin.take_error(err)
        ))
    }
}

fn plugin_cause(plugin: &LoadedPlugin, err: MqbBuffer, action: &str) -> anyhow::Error {
    anyhow!(
        "endpoint plugin `{}` failed to {action}: {}",
        plugin.name(),
        plugin.take_error(err)
    )
}

/// Maps a plugin status onto the consumer error classes the route reacts to.
fn consumer_error(
    plugin: &LoadedPlugin,
    status: MqbStatus,
    err: MqbBuffer,
    action: &str,
) -> ConsumerError {
    if status == MQB_END_OF_STREAM {
        // The buffer, if any, carries no information the route can use.
        let _ = plugin.take_error(err);
        return ConsumerError::EndOfStream;
    }
    consumer_error_from(status, plugin_cause(plugin, err, action))
}

/// The status-to-class half of [`consumer_error`], for callers that already
/// turned the plugin's error buffer into a message.
pub(super) fn consumer_error_from(status: MqbStatus, cause: anyhow::Error) -> ConsumerError {
    match status {
        // A panic is a bug in the plugin, not a transient fault: retrying it
        // would loop on the same crash.
        MQB_ERR_PERMANENT | MQB_ERR_INVALID_CONFIG | MQB_ERR_UNSUPPORTED | MQB_ERR_PANIC => {
            ConsumerError::Permanent(cause)
        }
        // Retryable and connection-level failures both mean "reconnect and
        // retry" for a consumer; the route has no separate retry class here.
        _ => ConsumerError::Connection(cause),
    }
}

/// Maps a plugin status onto the publisher error classes, preserving whether
/// the route may retry the batch.
fn publisher_error(
    plugin: &LoadedPlugin,
    status: MqbStatus,
    err: MqbBuffer,
    action: &str,
) -> PublisherError {
    publisher_error_from(status, plugin_cause(plugin, err, action))
}

/// The status-to-class half of [`publisher_error`], for callers that already
/// turned the plugin's error buffer into a message.
pub(super) fn publisher_error_from(status: MqbStatus, cause: anyhow::Error) -> PublisherError {
    match status {
        MQB_ERR_RETRYABLE => PublisherError::Retryable(cause),
        MQB_ERR_CONNECTION => PublisherError::Connection(cause),
        MQB_ERR_PERMANENT
        | MQB_ERR_INVALID_CONFIG
        | MQB_ERR_UNSUPPORTED
        | MQB_ERR_PANIC
        | MQB_END_OF_STREAM => PublisherError::NonRetryable(cause),
        _ => PublisherError::Retryable(cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_map_to_abi_codes() {
        assert_eq!(
            disposition_code(&MessageDisposition::Ack),
            MQB_DISPOSITION_ACK
        );
        assert_eq!(
            disposition_code(&MessageDisposition::Nack),
            MQB_DISPOSITION_NACK
        );
        assert_eq!(
            disposition_code(&MessageDisposition::Reply(CanonicalMessage::from("x"))),
            MQB_DISPOSITION_ACK
        );
    }

    // A plugin that rejected its config must not look like a transient fault:
    // `create_consumer`/`create_publisher` classify with these, and the route
    // only stops reconnecting for the permanent classes.
    #[test]
    fn a_rejected_plugin_config_is_permanent_not_retryable() {
        assert!(matches!(
            consumer_error_from(MQB_ERR_INVALID_CONFIG, anyhow!("bad field")),
            ConsumerError::Permanent(_)
        ));
        assert!(matches!(
            publisher_error_from(MQB_ERR_INVALID_CONFIG, anyhow!("bad field")),
            PublisherError::NonRetryable(_)
        ));
    }

    // The other half of the contract: a connection failure stays retryable, so
    // a broker that is merely down still gets the reconnect loop.
    #[test]
    fn a_plugin_connection_failure_stays_retryable() {
        assert!(matches!(
            consumer_error_from(MQB_ERR_CONNECTION, anyhow!("broker down")),
            ConsumerError::Connection(_)
        ));
        assert!(matches!(
            publisher_error_from(MQB_ERR_CONNECTION, anyhow!("broker down")),
            PublisherError::Connection(_)
        ));
    }
}
