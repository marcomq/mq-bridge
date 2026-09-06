//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Safe wrappers that present a loaded plugin's function table as ordinary
//! mq-bridge endpoints.
//!
//! Every ABI call is blocking by contract, so each one runs on
//! [`tokio::task::spawn_blocking`] rather than on the async executor. The only
//! exceptions are the two cheap, non-blocking consumer queries the ABI marks as
//! such (`commit_requires_order`, `set_exit_on_empty`).
//!
//! Acknowledgement stays under the route's control: `receive_batch` hands back
//! a batch handle wrapped in [`PluginBatch`], and the plugin only learns the
//! dispositions when the route invokes the batch commit function. A batch that
//! is dropped without being committed is released without acknowledging
//! anything, so the broker can redeliver.

use std::any::Any;
use std::sync::Arc;

use crate::support::plugin_abi::{
    MqbBatchHandle, MqbBuffer, MqbConsumerHandle, MqbMessage, MqbPublisherHandle, MqbSlice,
    MqbStatus, MQB_DISPOSITION_ACK, MQB_DISPOSITION_NACK, MQB_END_OF_STREAM, MQB_ERR_CONNECTION,
    MQB_ERR_INVALID_CONFIG, MQB_ERR_PANIC, MQB_ERR_PERMANENT, MQB_ERR_RETRYABLE,
    MQB_ERR_UNSUPPORTED, MQB_OK,
};
use anyhow::anyhow;
use async_trait::async_trait;

use super::LoadedPlugin;
use crate::errors::{ConsumerError, PublisherError};
use crate::traits::{
    BatchCommitFunc, CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher,
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
    fn commit(mut self, dispositions: Vec<u8>) -> anyhow::Result<()> {
        let handle = std::mem::replace(&mut self.handle, MqbBatchHandle::NULL);
        if handle.is_null() {
            return Ok(());
        }
        let mut err = MqbBuffer::EMPTY;
        let status = unsafe {
            (self.plugin.table().batch_commit)(
                handle,
                dispositions.as_ptr(),
                dispositions.len(),
                &mut err,
            )
        };
        if status == MQB_OK {
            Ok(())
        } else {
            Err(anyhow!(
                "endpoint plugin `{}` failed to commit a batch of {} messages: {}",
                self.plugin.name(),
                dispositions.len(),
                self.plugin.take_error(err)
            ))
        }
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
}

unsafe impl Send for ConsumerHandle {}
unsafe impl Sync for ConsumerHandle {}

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

        let (batch, messages) = received.0;
        let expected = messages.len();
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                if dispositions.len() != expected {
                    return Err(anyhow!(
                        "plugin batch commit received {} dispositions for {expected} messages",
                        dispositions.len()
                    ));
                }
                let dispositions: Vec<u8> = dispositions.iter().map(disposition_code).collect();
                tokio::task::spawn_blocking(move || batch.commit(dispositions))
                    .await
                    .map_err(join_error)?
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Owns a plugin publisher handle, refcounted for the same reason as
/// [`ConsumerHandle`]: a send may still be inside the plugin when the route
/// that owns the publisher goes away.
struct PublisherHandle {
    plugin: Arc<LoadedPlugin>,
    handle: MqbPublisherHandle,
}

unsafe impl Send for PublisherHandle {}
unsafe impl Sync for PublisherHandle {}

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
        tokio::task::spawn_blocking(move || {
            let plugin = &publisher.plugin;
            let messages = super::message::AbiMessages::new(messages);
            let mut err = MqbBuffer::EMPTY;
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
        tokio::task::spawn_blocking(move || {
            let mut err = MqbBuffer::EMPTY;
            let status =
                unsafe { (publisher.plugin.table().publisher_flush)(publisher.handle, &mut err) };
            plugin_result(&publisher.plugin, status, err, "flush an output")
        })
        .await
        .map_err(join_error)?
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn disposition_code(disposition: &MessageDisposition) -> u8 {
    match disposition {
        // The v1 ABI has no reply channel: a reply-producing handler still
        // acknowledges the source message, matching in-tree endpoints without
        // request/reply support.
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
