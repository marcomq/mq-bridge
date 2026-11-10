//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `builtin_map_middleware`: built-in middlewares run as a batch map. The host keeps the
//! endpoint and its commits, so a batch crosses the plugin boundary once instead of once per
//! endpoint call. A list with a middleware that acts on commits is refused.

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mq_bridge::errors::ProcessingError;
use mq_bridge::middleware::{
    apply_middlewares_to_consumer, apply_middlewares_to_publisher, runs_as_map, Filters,
    MessageView,
};
use mq_bridge::models::Endpoint;
use mq_bridge::plugin::sdk::{BatchFilter, MessageRef, MiddlewareFactory, Verdict};
use mq_bridge::traits::{ConsumerError, MessageConsumer, MessagePublisher, PublisherError};
use mq_bridge::{CanonicalMessage, ReceivedBatch, SentBatch};
use serde_json::Value;

type Slot = Arc<Mutex<Vec<CanonicalMessage>>>;

#[derive(Debug, Default)]
pub struct BuiltinMapMiddleware;

#[async_trait]
impl MiddlewareFactory for BuiltinMapMiddleware {
    async fn create(
        &self,
        route_name: &str,
        config: &Value,
    ) -> anyhow::Result<Box<dyn BatchFilter>> {
        let endpoint = crate::middlewares(config)?;
        if !runs_as_map(&endpoint.middlewares) {
            anyhow::bail!(
                "deduplication and aggregate act on the source's commit, which a batch map \
                 does not see; they run through `builtin_middleware`"
            );
        }
        if let Some(filters) = Filters::from_list(&endpoint.middlewares)? {
            return Ok(Box::new(InPlace(filters)));
        }
        let map = Map {
            route_name: route_name.to_string(),
            endpoint,
            consumers: Mutex::default(),
            publishers: Mutex::default(),
        };
        // Builds one chain now, so a bad config fails at route start.
        let publisher = map.publisher().await?;
        map.publishers
            .lock()
            .expect("pool poisoned")
            .push(publisher);
        Ok(Box::new(map))
    }
}

/// Chains of the engine's own middlewares over a stub endpoint, pooled so concurrent
/// batches each get one.
struct Map {
    route_name: String,
    endpoint: Endpoint,
    consumers: Mutex<Vec<(Box<dyn MessageConsumer>, Slot)>>,
    publishers: Mutex<Vec<(Arc<dyn MessagePublisher>, Slot)>>,
}

impl Map {
    async fn consumer(&self) -> anyhow::Result<(Box<dyn MessageConsumer>, Slot)> {
        if let Some(chain) = self.consumers.lock().expect("pool poisoned").pop() {
            return Ok(chain);
        }
        let slot = Slot::default();
        let feed = Box::new(Stub(slot.clone()));
        let chain = apply_middlewares_to_consumer(feed, &self.endpoint, &self.route_name).await?;
        Ok((chain, slot))
    }

    async fn publisher(&self) -> anyhow::Result<(Arc<dyn MessagePublisher>, Slot)> {
        if let Some(chain) = self.publishers.lock().expect("pool poisoned").pop() {
            return Ok(chain);
        }
        let slot = Slot::default();
        let sink = Box::new(Stub(slot.clone()));
        let chain = apply_middlewares_to_publisher(sink, &self.endpoint, &self.route_name).await?;
        Ok((chain, slot))
    }

    async fn receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<CanonicalMessage>> {
        let (mut chain, slot) = self.consumer().await?;
        let max = messages.len();
        *slot.lock().expect("slot poisoned") = messages;
        let batch = chain.receive_batch(max).await;
        self.consumers
            .lock()
            .expect("pool poisoned")
            .push((chain, slot));
        Ok(batch?.messages)
    }

    async fn send(&self, messages: Vec<CanonicalMessage>) -> anyhow::Result<Vec<CanonicalMessage>> {
        let (chain, slot) = self.publisher().await?;
        let sent = chain.send_batch(messages).await;
        let out = std::mem::take(&mut *slot.lock().expect("slot poisoned"));
        self.publishers
            .lock()
            .expect("pool poisoned")
            .push((chain, slot));
        if let SentBatch::Partial { mut failed, .. } = sent? {
            if let Some((_, error)) = failed.pop() {
                return Err(error.into());
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl BatchFilter for Map {
    async fn on_receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        per_message_if_ids_repeat(messages, |batch| self.receive(batch)).await
    }

    async fn on_send(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        per_message_if_ids_repeat(messages, |batch| self.send(batch)).await
    }
}

/// A list of only filters, decided on the host's batch without copying it.
struct InPlace(Filters);

impl InPlace {
    // As the engine's filter: an expression that cannot read the message is permanent.
    fn matches(&self, message: &impl MessageView) -> anyhow::Result<bool> {
        Ok(self
            .0
            .matches(message)
            .map_err(ProcessingError::NonRetryable)?)
    }

    fn verdicts(&self, messages: &[MessageRef<'_>]) -> anyhow::Result<Vec<Verdict>> {
        messages
            .iter()
            .map(|message| {
                Ok(if self.matches(message)? {
                    Verdict::Keep
                } else {
                    Verdict::Drop
                })
            })
            .collect()
    }

    fn kept(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        messages
            .into_iter()
            .map(|message| Ok(self.matches(&message)?.then_some(message)))
            .collect()
    }
}

#[async_trait]
impl BatchFilter for InPlace {
    async fn on_receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        self.kept(messages)
    }

    async fn on_send(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        self.kept(messages)
    }

    async fn on_receive_ref(&self, messages: &[MessageRef<'_>]) -> anyhow::Result<Vec<Verdict>> {
        self.verdicts(messages)
    }

    async fn on_send_ref(&self, messages: &[MessageRef<'_>]) -> anyhow::Result<Vec<Verdict>> {
        self.verdicts(messages)
    }
}

/// Runs `map` on the batch and puts each output back at the input with its id; a batch
/// with repeated ids goes one message at a time, so the placement stays unambiguous.
async fn per_message_if_ids_repeat<F, Fut>(
    messages: Vec<CanonicalMessage>,
    map: F,
) -> anyhow::Result<Vec<Option<CanonicalMessage>>>
where
    F: Fn(Vec<CanonicalMessage>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Vec<CanonicalMessage>>>,
{
    let ids: Vec<u128> = messages.iter().map(|m| m.message_id).collect();
    if !ids_repeat(&ids) {
        return place(&ids, map(messages).await?);
    }
    let mut placed = Vec::with_capacity(messages.len());
    for message in messages {
        let id = [message.message_id];
        placed.extend(place(&id, map(vec![message]).await?)?);
    }
    Ok(placed)
}

fn ids_repeat(ids: &[u128]) -> bool {
    if ids.len() < 2 {
        return false;
    }
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).any(|pair| pair[0] == pair[1])
}

fn place(
    ids: &[u128],
    out: Vec<CanonicalMessage>,
) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
    let mut placed: Vec<Option<CanonicalMessage>> = ids.iter().map(|_| None).collect();
    let mut at = 0;
    for message in out {
        let Some(offset) = ids[at..].iter().position(|id| *id == message.message_id) else {
            anyhow::bail!("a built-in middleware reordered messages or changed a message id");
        };
        placed[at + offset] = Some(message);
        at += offset + 1;
    }
    Ok(placed)
}

/// The endpoint under a chain: as a source it yields the batch it was handed, then
/// nothing; as a sink it keeps what reaches it.
struct Stub(Slot);

#[async_trait]
impl MessageConsumer for Stub {
    async fn receive_batch(
        &mut self,
        _max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let messages = std::mem::take(&mut *self.0.lock().expect("slot poisoned"));
        Ok(ReceivedBatch {
            messages,
            commit: Box::new(|_| Box::pin(async { Ok(()) })),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait]
impl MessagePublisher for Stub {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        self.0.lock().expect("slot poisoned").extend(messages);
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u128, payload: &str) -> CanonicalMessage {
        let mut message = CanonicalMessage::from(payload);
        message.message_id = id;
        message
    }

    #[test]
    fn outputs_go_back_to_their_inputs() {
        let placed = place(&[1, 2, 3], vec![message(1, "a"), message(3, "c")]).unwrap();
        let ids: Vec<Option<u128>> = placed
            .iter()
            .map(|m| m.as_ref().map(|m| m.message_id))
            .collect();
        assert_eq!(ids, [Some(1), None, Some(3)]);
        assert!(place(&[1, 2], vec![message(2, "b"), message(1, "a")]).is_err());
    }

    #[tokio::test]
    async fn filter_and_compression_round_trip() {
        let config = serde_json::json!([
            { "filter": "meta.kind != \"drop\"" },
            { "compression": { "algorithm": "zstd" } },
        ]);
        let send = BuiltinMapMiddleware.create("map", &config).await.unwrap();
        let receive = BuiltinMapMiddleware.create("map", &config).await.unwrap();
        let dropped = message(2, "{}").with_metadata_kv("kind", "drop");
        let input = vec![message(1, "{}"), dropped, message(3, "{}")];
        let sent = send.on_send(input).await.unwrap();
        assert!(sent[1].is_none());
        assert_ne!(sent[0].as_ref().unwrap().payload.as_ref(), b"{}");
        let received = receive
            .on_receive(sent.into_iter().flatten().collect())
            .await
            .unwrap();
        let payloads: Vec<&[u8]> = received
            .iter()
            .flatten()
            .map(|m| m.payload.as_ref())
            .collect();
        assert_eq!(payloads, [b"{}".as_slice(), b"{}"]);
    }

    #[tokio::test]
    async fn a_filter_error_is_permanent() {
        let config = serde_json::json!([{ "filter": "amount > 1" }]);
        let filter = BuiltinMapMiddleware.create("map", &config).await.unwrap();
        let error = filter
            .on_send(vec![message(1, "not json")])
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ProcessingError>(),
            Some(ProcessingError::NonRetryable(_))
        ));
    }

    #[tokio::test]
    async fn a_middleware_that_acts_on_commit_is_refused() {
        let config =
            serde_json::json!([{ "deduplication": { "store": "memory://t", "ttl_seconds": 60 } }]);
        assert!(BuiltinMapMiddleware.create("map", &config).await.is_err());
    }
}
