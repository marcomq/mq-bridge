//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Declarative JSON reshaping: field mapping, then schema-directed coercion.
//!
//! Both stages run over a single `serde_json::Value` parsed once per message, so a
//! message is never round-tripped through bytes between stages. Everything derived from
//! configuration (source paths, the schema) is compiled once in `new()`.
//!
//! Building that `Value` is the dominant cost — a seven-field object costs about fifteen
//! allocations, most of them for fields the schema leaves exactly as they were. So when
//! the configuration allows it (`fast_eligible`), `transform_fast` walks the payload's
//! top-level fields as borrowed spans and copies the untouched ones straight to the
//! output, parsing only the fields that actually need work and handing those to the very
//! same `apply`. It decides *whether* a field is worth parsing, never *how* it is
//! transformed, so the two routes cannot drift apart in behaviour; `fast_path_equivalence`
//! holds them to that, including the two differences it documents.
//!
//! A projection — a mapping that lifts top-level fields to top-level output keys, with no
//! schema — gets the same treatment from `project_fast`: its output is a subset of the
//! input's fields, so the wanted spans are copied straight through and the dropped ones,
//! which are often the largest, are never parsed. Mappings outside that subset still parse
//! once into a `Value`, but pick values by moving them out rather than deep-cloning,
//! whenever no rule reads through another's path.
//!
//! Only the JSON Schema subset that matters for message integration is honoured:
//! `type`, `properties`, `required`, `default`, `items`, `nullable`, `enum`, plus
//! `contentMediaType`/`contentSchema` for embedded JSON. Other keywords are ignored
//! rather than rejected, so a fuller schema can be pointed at without being rewritten.
//!
//! The module is split by stage: [`error`] (failure type), [`path`] (source-path parsing
//! and the mapping stage), [`coerce`] (JSON types and value coercion), [`schema`] (the
//! compiled JSON Schema subset), [`compiled`] (the per-config engine that ties the stages
//! together) and [`endpoint`] (the publisher/consumer attach points).

mod coerce;
mod compiled;
mod error;
mod path;
mod schema;

use crate::models::TransformMiddleware;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Received, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use compiled::Compiled;
use std::any::Any;
use std::sync::Arc;

/// Metadata key carrying the failure description when `on_error: pass_through`.
pub const TRANSFORM_ERROR_KEY: &str = "mqb.transform_error";

#[cfg(test)]
mod tests;

/// Bench-only: applies the compiled configuration to every message and returns the total
/// output size. Lives here because `Compiled` is private to this module.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub(crate) fn bench_apply(
    config: &TransformMiddleware,
    messages: &[CanonicalMessage],
) -> anyhow::Result<usize> {
    let compiled = Compiled::new(config)?;
    let mut total = 0;
    for message in messages {
        let mut message = message.clone();
        compiled
            .transform(&mut message)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        total += message.payload.len();
    }
    Ok(total)
}

/// One message's outcome: transformed (possibly annotated by the failure policy), or
/// rejected with the error that rejected it.
///
/// Boxed on the rejected side so the whole batch's `Vec` is sized by a transformed
/// message rather than by the far larger error pair.
type Transformed = Result<CanonicalMessage, Box<(CanonicalMessage, error::TransformError)>>;

/// Transforms a whole batch, spreading the work over cores when it is big enough.
/// Outcomes come back in input order.
async fn apply_batch(
    compiled: &Arc<Compiled>,
    messages: Vec<CanonicalMessage>,
) -> Vec<Transformed> {
    let compiled = Arc::clone(compiled);
    crate::support::parallel::map_messages(messages, move |mut message| {
        match compiled.transform(&mut message) {
            Ok(()) => Ok(message),
            Err(error) => match compiled.handle_failure(&mut message, error) {
                Ok(()) => Ok(message),
                Err(error) => Err(Box::new((message, error))),
            },
        }
    })
    .await
}

// --- Publisher attach point ---

pub struct TransformPublisher {
    inner: Box<dyn MessagePublisher>,
    compiled: Arc<Compiled>,
}

impl TransformPublisher {
    pub fn new(
        inner: Box<dyn MessagePublisher>,
        config: &TransformMiddleware,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            compiled: Arc::new(Compiled::new(config)?),
        })
    }

    async fn apply_to_batch(&self, messages: Vec<CanonicalMessage>) -> Vec<Transformed> {
        apply_batch(&self.compiled, messages).await
    }
}

#[async_trait]
impl MessagePublisher for TransformPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if self.compiled.is_noop() {
            return self.inner.send_batch(messages).await;
        }

        // Split across cores: an ordered sink serializes this whole call, so route
        // concurrency cannot overlap it and the batch itself is what has to parallelise.
        let outcomes = self.apply_to_batch(messages).await;

        let mut forwarded = Vec::with_capacity(outcomes.len());
        let mut failed: Vec<(CanonicalMessage, PublisherError)> = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(message) => forwarded.push(message),
                Err(rejected) => {
                    let (message, error) = *rejected;
                    failed.push((message, error.into()));
                }
            }
        }

        if failed.is_empty() {
            return self.inner.send_batch(forwarded).await;
        }

        let mut responses = None;
        if !forwarded.is_empty() {
            // A transport-level error covers the whole batch, so it takes precedence:
            // the route retries it, and the rejected messages come back through here.
            match self.inner.send_batch(forwarded).await? {
                SentBatch::Ack => {}
                SentBatch::Partial {
                    responses: inner_responses,
                    failed: inner_failed,
                } => {
                    responses = inner_responses;
                    failed.extend(inner_failed);
                }
            }
        }

        Ok(SentBatch::Partial { responses, failed })
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

// --- Consumer attach point ---

pub struct TransformConsumer {
    inner: Box<dyn MessageConsumer>,
    compiled: Arc<Compiled>,
}

impl TransformConsumer {
    pub fn new(
        inner: Box<dyn MessageConsumer>,
        config: &TransformMiddleware,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            compiled: Arc::new(Compiled::new(config)?),
        })
    }
}

#[async_trait]
impl MessageConsumer for TransformConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            let received = self.inner.receive().await?;
            if self.compiled.is_noop() {
                return Ok(received);
            }
            let Received {
                mut message,
                commit,
            } = received;
            let outcome = self
                .compiled
                .transform(&mut message)
                .or_else(|error| self.compiled.handle_failure(&mut message, error));
            match outcome {
                Ok(()) => return Ok(Received { message, commit }),
                Err(error) => {
                    tracing::error!(
                        message_id = format_args!("{:032x}", message.message_id),
                        "Rejecting invalid input message: {error}"
                    );
                    // Ack the rejected message so it is not redelivered forever, then wait
                    // for the next one. A data problem must not surface as a consumer
                    // error, which the route would treat as a reason to reconnect.
                    commit(MessageDisposition::Ack)
                        .await
                        .map_err(ConsumerError::Connection)?;
                }
            }
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let batch = self.inner.receive_batch(max_messages).await?;
            if self.compiled.is_noop() {
                return Ok(batch);
            }

            let ReceivedBatch { messages, commit } = batch;
            let original_len = messages.len();
            let outcomes = apply_batch(&self.compiled, messages).await;
            let mut kept = Vec::with_capacity(original_len);
            let mut kept_indices: Vec<usize> = Vec::with_capacity(original_len);

            for (index, outcome) in outcomes.into_iter().enumerate() {
                match outcome {
                    Ok(message) => {
                        kept_indices.push(index);
                        kept.push(message);
                    }
                    Err(rejected) => {
                        let (message, error) = *rejected;
                        tracing::error!(
                            message_id = format_args!("{:032x}", message.message_id),
                            "Rejecting invalid input message: {error}"
                        );
                    }
                }
            }

            if kept.len() == original_len {
                return Ok(ReceivedBatch {
                    messages: kept,
                    commit,
                });
            }

            if kept.is_empty() {
                // Every message in a non-empty batch was rejected. Ack them so they are not
                // redelivered forever, then loop to fetch the next batch rather than surfacing
                // an empty (idle-looking) one — mirroring the `receive()` path above.
                if original_len > 0 {
                    commit(vec![MessageDisposition::Ack; original_len])
                        .await
                        .map_err(ConsumerError::Connection)?;
                }
                continue;
            }

            // Partial retention: rejected slots are acked and the caller's dispositions are
            // placed back at the indices they actually came from, which keeps at-least-once
            // intact for the surviving messages.
            let remapped = Box::new(move |dispositions: Vec<MessageDisposition>| {
                let mut full = vec![MessageDisposition::Ack; original_len];
                for (slot, disposition) in kept_indices.into_iter().zip(dispositions) {
                    full[slot] = disposition;
                }
                commit(full)
            });

            return Ok(ReceivedBatch {
                messages: kept,
                commit: remapped,
            });
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
