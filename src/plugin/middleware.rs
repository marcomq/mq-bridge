//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! A loaded plugin's middleware, presented as a [`CustomMiddlewareFactory`].
//!
//! Only the messages cross the ABI. The wrapper around the inner consumer or
//! publisher stays here on the host side, so a plugin never has to call back
//! into host objects — which is what keeps the ABI one-directional.

use std::any::Any;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;

use super::endpoint::{consumer_error_from, join_error, AssertSend};
use super::message::{from_abi, AbiMessages};
use super::LoadedPlugin;
use crate::errors::{ConsumerError, PublisherError};
use crate::support::plugin_abi::{
    MqbBuffer, MqbFilterHandle, MqbMessage, MqbMiddlewareHandle, MqbSlice, MqbStatus,
    MQB_END_OF_STREAM, MQB_ERR_CONNECTION, MQB_ERR_INVALID_CONFIG, MQB_ERR_PANIC,
    MQB_ERR_PERMANENT, MQB_ERR_UNSUPPORTED, MQB_MESSAGE_KEPT, MQB_MIDDLEWARE_RECEIVE,
    MQB_MIDDLEWARE_SEND, MQB_OK,
};
use crate::traits::{
    BatchCommitFunc, CustomMiddlewareFactory, MessageConsumer, MessageDisposition, MessagePublisher,
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
    async fn apply_consumer(
        &self,
        consumer: Box<dyn MessageConsumer>,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let middleware = self
            .open(route_name, config, MQB_MIDDLEWARE_RECEIVE)
            .await?;
        Ok(Box::new(PluginMiddlewareConsumer {
            inner: consumer,
            middleware: Arc::new(middleware),
        }))
    }

    async fn apply_publisher(
        &self,
        publisher: Box<dyn MessagePublisher>,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let middleware = self.open(route_name, config, MQB_MIDDLEWARE_SEND).await?;
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
}

unsafe impl Send for PluginMiddleware {}
unsafe impl Sync for PluginMiddleware {}

/// What the middleware did to a batch: the surviving messages, plus one flag
/// per source message so the caller can still address the dropped ones.
struct Filtered {
    kept: Vec<CanonicalMessage>,
    keep_flags: Vec<bool>,
}

/// A failed `middleware_apply`, keeping the ABI status so the caller can
/// classify it the same way an endpoint failure is classified.
struct ApplyFailure {
    status: MqbStatus,
    cause: anyhow::Error,
}

impl PluginMiddleware {
    async fn apply(
        self: &Arc<Self>,
        messages: Vec<CanonicalMessage>,
    ) -> Result<Filtered, ApplyFailure> {
        let middleware = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let input = AbiMessages::new(messages);
            let mut result = MqbFilterHandle::NULL;
            let mut out_messages: *const MqbMessage = std::ptr::null();
            let mut kept: *const u8 = std::ptr::null();
            let mut err = MqbBuffer::EMPTY;
            let table = middleware.plugin.table();
            let status = unsafe {
                (table.middleware_apply)(
                    middleware.handle,
                    input.as_ptr(),
                    input.len(),
                    &mut result,
                    &mut out_messages,
                    &mut kept,
                    &mut err,
                )
            };
            // Released even if the call failed after writing a handle, or if the
            // arrays below turn out malformed.
            let _guard = FilterResult {
                plugin: Arc::clone(&middleware.plugin),
                handle: result,
            };
            if status != MQB_OK {
                return Err(ApplyFailure {
                    status,
                    cause: anyhow!(
                        "middleware plugin `{}` failed on a batch of {} messages: {}",
                        middleware.plugin.name(),
                        input.len(),
                        middleware.plugin.take_error(err)
                    ),
                });
            }
            if out_messages.is_null() || kept.is_null() {
                return Err(ApplyFailure {
                    status: MQB_ERR_PERMANENT,
                    cause: anyhow!(
                        "middleware plugin `{}` returned a null result array",
                        middleware.plugin.name()
                    ),
                });
            }

            let keep_flags: Vec<bool> = unsafe { std::slice::from_raw_parts(kept, input.len()) }
                .iter()
                .map(|flag| *flag == MQB_MESSAGE_KEPT)
                .collect();
            let all = unsafe { from_abi(out_messages, input.len()) };
            let kept = all
                .into_iter()
                .zip(&keep_flags)
                .filter_map(|(message, keep)| keep.then_some(message))
                .collect();
            Ok(AssertSend(Filtered { kept, keep_flags }))
        })
        .await
        .map_err(|err| ApplyFailure {
            status: MQB_ERR_CONNECTION,
            cause: join_error(err),
        })?
        .map(|sent| sent.0)
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
                return Ok(batch);
            }
            let ReceivedBatch { messages, commit } = batch;
            let Filtered { kept, keep_flags } = self
                .middleware
                .apply(messages)
                .await
                .map_err(|failure| consumer_error_from(failure.status, failure.cause))?;

            if kept.is_empty() {
                // The route will never commit a batch it never sees, so ack the
                // dropped messages here or the source redelivers them forever.
                commit(vec![MessageDisposition::Ack; keep_flags.len()])
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }

            // The route only sees the kept messages, so expand its dispositions
            // back to one per source message, acking the ones we dropped.
            let expected = kept.len();
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
                    commit(expanded).await
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
        let filtered =
            self.middleware
                .apply(messages)
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
        self.inner.send_batch(filtered.kept).await
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
