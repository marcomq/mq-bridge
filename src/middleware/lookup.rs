//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Enriches each message with the responses of other, request-capable endpoints.

use crate::endpoints::create_publisher_from_route;
use crate::errors::InvalidConfig;
use crate::models::{Endpoint, LookupMiddleware};
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Map, Value};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

const FOUND: &str = "lookup.found";
const HTTP_STATUS_CODE: &str = "http_status_code";

struct Entry {
    from: Arc<dyn MessagePublisher>,
    metadata: Vec<(String, CompiledTemplate)>,
    payload: Option<CompiledTemplate>,
    into: Vec<String>,
    found_key: String,
}

impl Entry {
    async fn new(
        from: &Endpoint,
        metadata: &HashMap<String, String>,
        payload: Option<&str>,
        into: &str,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        let into: Vec<String> = into
            .trim()
            .trim_start_matches('$')
            .trim_start_matches('.')
            .split('.')
            .map(str::to_string)
            .collect();
        if into.iter().any(String::is_empty) {
            anyhow::bail!("lookup: invalid `into` path '{}'", into.join("."));
        }
        let metadata = metadata
            .iter()
            .map(|(k, t)| Ok((k.clone(), CompiledTemplate::compile(t, None)?)))
            .collect::<anyhow::Result<_>>()?;
        let payload = payload
            .map(|t| CompiledTemplate::compile(t, None))
            .transpose()?;
        // Box::pin breaks the recursive async type, as in the dlq middleware.
        let from = Box::pin(create_publisher_from_route(route_name, from)).await?;
        let found_key = format!("lookup.{}.found", into.join("."));
        Ok(Self {
            from,
            metadata,
            payload,
            into,
            found_key,
        })
    }

    /// The request `from` answers for this message.
    fn request(&self, msg: &CanonicalMessage) -> CanonicalMessage {
        let mut request = match &self.payload {
            Some(t) => CanonicalMessage::new(t.render(Some(msg)), None),
            None => {
                let mut request = msg.clone();
                // An incoming message must not steer an `http` lookup; only `metadata` may.
                for key in ["http_method", "http_path", "http_query"] {
                    request.metadata.remove(key);
                }
                request
            }
        };
        for (key, template) in &self.metadata {
            let value = String::from_utf8_lossy(&template.render(Some(msg))).into_owned();
            request.metadata.insert(key.clone(), value);
        }
        request
    }

    /// Answers every message, in order: one batched lookup when `from` supports it, else one
    /// request per message, `concurrency` at a time. `None` = not found.
    async fn fetch_many(
        &self,
        msgs: &[&CanonicalMessage],
        concurrency: usize,
    ) -> Vec<Result<Option<Value>, PublisherError>> {
        let requests: Vec<CanonicalMessage> = msgs.iter().map(|m| self.request(m)).collect();
        let n = requests.len();
        match self.from.lookup_batch(&requests).await {
            Some(Ok(found)) if found.len() == n => found.into_iter().map(Ok).collect(),
            Some(Ok(found)) => (0..n)
                .map(|_| {
                    Err(PublisherError::NonRetryable(anyhow::anyhow!(
                        "lookup: `from` answered {} of {n} requests",
                        found.len()
                    )))
                })
                .collect(),
            Some(Err(e)) => (0..n).map(|_| Err(duplicate(&e))).collect(),
            None => {
                futures::stream::iter(requests)
                    .map(|r| self.fetch(r))
                    .buffered(concurrency)
                    .collect()
                    .await
            }
        }
    }

    /// Sends one request; `None` when `from` found nothing.
    async fn fetch(&self, request: CanonicalMessage) -> Result<Option<Value>, PublisherError> {
        let response = match self.from.send(request).await? {
            Sent::Response(response) => response,
            Sent::Ack => {
                return Err(PublisherError::NonRetryable(anyhow::anyhow!(
                    "lookup: the `from` endpoint returned no response; it must be request-capable"
                )))
            }
        };
        if let Some(status) = response
            .metadata
            .get(HTTP_STATUS_CODE)
            .and_then(|s| s.parse::<u16>().ok())
        {
            match status {
                404 => return Ok(None),
                // 401 and 403 are a credential problem, not a fault of the message.
                401 | 403 | 408 | 429 | 500..=504 => {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "lookup: HTTP status {status}"
                    )))
                }
                400.. => {
                    return Err(PublisherError::NonRetryable(anyhow::anyhow!(
                        "lookup: HTTP status {status}"
                    )))
                }
                _ => {}
            }
        }
        if response.payload.is_empty() {
            return Ok(None);
        }
        let value = serde_json::from_slice(&response.payload).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&response.payload).into_owned())
        });
        Ok((!value.is_null()).then_some(value))
    }
}

/// A copy of a batch-wide error for each message it fails.
fn duplicate(e: &PublisherError) -> PublisherError {
    let copy = anyhow::anyhow!("{e:#}");
    match e {
        PublisherError::Retryable(_) => PublisherError::Retryable(copy),
        PublisherError::NonRetryable(_) => PublisherError::NonRetryable(copy),
        PublisherError::Connection(_) => PublisherError::Connection(copy),
    }
}

/// The entries of one `lookup` middleware, shared by its publisher and consumer side.
struct Lookup {
    entries: Vec<Entry>,
    concurrency: usize,
}

impl Lookup {
    async fn new(config: &LookupMiddleware, route_name: &str) -> anyhow::Result<Self> {
        let mut entries = Vec::with_capacity(config.entries.len() + 1);
        match (&config.from, &config.into) {
            (Some(from), Some(into)) => {
                let payload = config.payload.as_deref();
                entries.push(Entry::new(from, &config.metadata, payload, into, route_name).await?);
            }
            (None, None) if config.metadata.is_empty() && config.payload.is_none() => {}
            _ => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "lookup: `from` and `into` must be set together"
                ))
                .into())
            }
        }
        for e in &config.entries {
            let payload = e.payload.as_deref();
            entries.push(Entry::new(&e.from, &e.metadata, payload, &e.into, route_name).await?);
        }
        if entries.is_empty() {
            return Err(InvalidConfig(anyhow::anyhow!(
                "lookup: set `from` and `into`, or list `entries`"
            ))
            .into());
        }
        Ok(Self {
            entries,
            concurrency: config.concurrency.max(1),
        })
    }

    async fn enrich(
        &self,
        msg: CanonicalMessage,
    ) -> Result<CanonicalMessage, (CanonicalMessage, PublisherError)> {
        self.enrich_batch(vec![msg])
            .await
            .pop()
            .expect("one result per message")
    }

    /// Runs every entry once for the whole batch, in parallel, and writes their results.
    async fn enrich_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Vec<Result<CanonicalMessage, (CanonicalMessage, PublisherError)>> {
        let docs: Vec<Result<Value, PublisherError>> = messages
            .iter()
            .map(|m| {
                serde_json::from_slice(&m.payload).map_err(|e| {
                    PublisherError::NonRetryable(anyhow::anyhow!(
                        "lookup: payload is not JSON: {e}"
                    ))
                })
            })
            .collect();
        let targets: Vec<&CanonicalMessage> = messages
            .iter()
            .zip(&docs)
            .filter(|(_, d)| d.is_ok())
            .map(|(m, _)| m)
            .collect();
        let fetches = self
            .entries
            .iter()
            .map(|entry| entry.fetch_many(&targets, self.concurrency));
        let mut per_entry: Vec<_> = futures::future::join_all(fetches)
            .await
            .into_iter()
            .map(Vec::into_iter)
            .collect();
        messages
            .into_iter()
            .zip(docs)
            .map(|(msg, doc)| {
                let doc = doc.map_err(|e| (msg.clone(), e));
                let results = per_entry
                    .iter_mut()
                    .map(|r| r.next().expect("one result per message"));
                self.apply(msg, doc?, results)
            })
            .collect()
    }

    /// Writes each entry's result into the message, or fails it on the first entry error.
    fn apply(
        &self,
        mut msg: CanonicalMessage,
        mut doc: Value,
        results: impl Iterator<Item = Result<Option<Value>, PublisherError>>,
    ) -> Result<CanonicalMessage, (CanonicalMessage, PublisherError)> {
        // Drain every entry first, so the per-entry iterators stay aligned across messages.
        let results = match results
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(results) => results,
            Err(e) => return Err((msg, e)),
        };
        let mut all_found = true;
        for (entry, found) in self.entries.iter().zip(results) {
            all_found &= found.is_some();
            msg.metadata
                .insert(entry.found_key.clone(), found.is_some().to_string());
            if let Err(e) = insert_at(&mut doc, &entry.into, found.unwrap_or(Value::Null)) {
                return Err((msg, PublisherError::NonRetryable(e)));
            }
        }
        msg.metadata
            .insert(FOUND.to_string(), all_found.to_string());
        match serde_json::to_vec(&doc) {
            Ok(bytes) => {
                msg.payload = bytes.into();
                Ok(msg)
            }
            Err(e) => Err((msg, PublisherError::NonRetryable(e.into()))),
        }
    }
}

/// Writes `value` at the dotted `path`, creating objects along the way.
fn insert_at(root: &mut Value, path: &[String], value: Value) -> anyhow::Result<()> {
    let (last, parents) = path.split_last().expect("path is non-empty");
    let mut cur = root;
    for key in parents {
        let Value::Object(map) = cur else {
            anyhow::bail!("lookup: cannot nest under '{key}': it is not an object");
        };
        cur = map
            .entry(key.as_str())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let Value::Object(map) = cur else {
        anyhow::bail!("lookup: cannot set '{last}': its parent is not an object");
    };
    map.insert(last.clone(), value);
    Ok(())
}

pub struct LookupPublisher {
    inner: Box<dyn MessagePublisher>,
    lookup: Lookup,
}

impl LookupPublisher {
    pub async fn new(
        inner: Box<dyn MessagePublisher>,
        config: &LookupMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            lookup: Lookup::new(config, route_name).await?,
        })
    }
}

#[async_trait]
impl MessagePublisher for LookupPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let message = self.lookup.enrich(message).await.map_err(|(_, e)| e)?;
        self.inner.send(message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let results = self.lookup.enrich_batch(messages).await;
        let mut enriched = Vec::with_capacity(results.len());
        let mut failed = Vec::new();
        for result in results {
            match result {
                Ok(m) => enriched.push(m),
                Err(f) => failed.push(f),
            }
        }
        if failed.is_empty() {
            return self.inner.send_batch(enriched).await;
        }
        if enriched.is_empty() {
            return Ok(SentBatch::Partial {
                responses: None,
                failed,
            });
        }
        match self.inner.send_batch(enriched).await? {
            SentBatch::Ack => Ok(SentBatch::Partial {
                responses: None,
                failed,
            }),
            SentBatch::Partial {
                responses,
                failed: mut inner_failed,
            } => {
                inner_failed.extend(failed);
                Ok(SentBatch::Partial {
                    responses,
                    failed: inner_failed,
                })
            }
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Enriches each received batch before the handler sees it. A retryable failure nacks the
/// whole batch and reconnects; a non-retryable one acks and drops only its message.
pub struct LookupConsumer {
    inner: Box<dyn MessageConsumer>,
    lookup: Lookup,
}

impl LookupConsumer {
    pub async fn new(
        inner: Box<dyn MessageConsumer>,
        config: &LookupMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            lookup: Lookup::new(config, route_name).await?,
        })
    }
}

#[async_trait]
impl MessageConsumer for LookupConsumer {
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

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let ReceivedBatch { messages, commit } = self.inner.receive_batch(max_messages).await?;
            if messages.is_empty() {
                return Ok(ReceivedBatch { messages, commit });
            }
            let len = messages.len();
            let mut enriched = Vec::with_capacity(len);
            let mut kept = Vec::with_capacity(len);
            let mut dropped = Vec::new();
            let mut transient = None;
            for (i, result) in self
                .lookup
                .enrich_batch(messages)
                .await
                .into_iter()
                .enumerate()
            {
                match result {
                    Ok(m) => {
                        enriched.push(m);
                        kept.push(i);
                    }
                    Err((m, PublisherError::NonRetryable(e))) => dropped.push((m.message_id, e)),
                    Err((_, PublisherError::Retryable(e) | PublisherError::Connection(e))) => {
                        transient = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = transient {
                if let Err(nack) = commit(vec![MessageDisposition::Nack; len]).await {
                    tracing::warn!("lookup: failed to nack the batch: {nack}");
                }
                return Err(ConsumerError::Connection(e));
            }
            for (id, e) in dropped {
                tracing::error!(
                    message_id = format_args!("{id:032x}"),
                    "lookup: dropping input message: {e:#}"
                );
            }
            if enriched.is_empty() {
                commit(vec![MessageDisposition::Ack; len])
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }
            if kept.len() == len {
                return Ok(ReceivedBatch {
                    messages: enriched,
                    commit,
                });
            }
            // Dropped messages are acked; the kept ones take the route's dispositions.
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                let mut all = vec![MessageDisposition::Ack; len];
                for (i, d) in kept.into_iter().zip(dispositions) {
                    all[i] = d;
                }
                commit(all)
            });
            return Ok(ReceivedBatch {
                messages: enriched,
                commit,
            });
        }
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;
    use serde_json::json;

    async fn lookup(
        sink_name: &str,
        config_yaml: &str,
    ) -> (LookupPublisher, crate::endpoints::memory::MemoryChannel) {
        let sink = MemoryPublisher::new_local(sink_name, 16);
        let channel = sink.channel();
        let config: LookupMiddleware = serde_yaml_ng::from_str(config_yaml).unwrap();
        let publisher = LookupPublisher::new(Box::new(sink), &config, "lookup_test")
            .await
            .unwrap();
        (publisher, channel)
    }

    fn msg(value: Value) -> CanonicalMessage {
        CanonicalMessage::from_json(value).unwrap()
    }

    fn payload(m: &CanonicalMessage) -> Value {
        serde_json::from_slice(&m.payload).unwrap()
    }

    #[tokio::test]
    async fn writes_the_response_at_the_into_path() {
        let (publisher, channel) = lookup(
            "lookup_into",
            r#"
from: { static: { body: '{"name":"user-${payload:id}"}', raw: true } }
into: customer.profile
"#,
        )
        .await;
        let result = publisher
            .send_batch(vec![msg(json!({"id": 1})), msg(json!({"id": 2}))])
            .await
            .unwrap();
        assert!(matches!(result, SentBatch::Ack));
        let sent = channel.drain_messages();
        assert_eq!(
            payload(&sent[0]),
            json!({"id": 1, "customer": {"profile": {"name": "user-1"}}})
        );
        assert_eq!(payload(&sent[1])["customer"]["profile"]["name"], "user-2");
        assert_eq!(
            sent[1].metadata.get(FOUND).map(String::as_str),
            Some("true")
        );
    }

    #[tokio::test]
    async fn an_empty_or_404_response_writes_null() {
        for from in [
            r#"{ static: { body: "", raw: true } }"#,
            r#"{ static: { body: '{"a":1}', raw: true, metadata: { http_status_code: "404" } } }"#,
        ] {
            let (publisher, channel) =
                lookup("lookup_null", &format!("from: {from}\ninto: user")).await;
            publisher.send(msg(json!({"id": 1}))).await.unwrap();
            let sent = channel.drain_messages();
            assert_eq!(payload(&sent[0]), json!({"id": 1, "user": null}), "{from}");
            assert_eq!(
                sent[0].metadata.get(FOUND).map(String::as_str),
                Some("false")
            );
        }
    }

    #[tokio::test]
    async fn an_endpoint_without_a_response_fails_the_message() {
        let (publisher, channel) =
            lookup("lookup_no_response", "from: { null: null }\ninto: user").await;
        let result = publisher
            .send_batch(vec![msg(json!({"id": 1}))])
            .await
            .unwrap();
        let SentBatch::Partial { failed, .. } = result else {
            panic!("expected a partial failure");
        };
        assert_eq!(failed.len(), 1);
        assert!(matches!(failed[0].1, PublisherError::NonRetryable(_)));
        assert_eq!(payload(&failed[0].0), json!({"id": 1}));
        assert!(channel.drain_messages().is_empty());
    }

    #[tokio::test]
    async fn entries_run_in_parallel_with_a_found_flag_each() {
        let (publisher, channel) = lookup(
            "lookup_entries",
            r#"
from: { static: { body: '{"n":1}', raw: true }, middlewares: [ { delay: { delay_ms: 300 } } ] }
into: user
entries:
  - from: { static: { body: "", raw: true }, middlewares: [ { delay: { delay_ms: 300 } } ] }
    into: features.device
"#,
        )
        .await;
        let started = std::time::Instant::now();
        publisher.send(msg(json!({"id": 1}))).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(550));
        let sent = channel.drain_messages();
        assert_eq!(
            payload(&sent[0]),
            json!({"id": 1, "user": {"n": 1}, "features": {"device": null}})
        );
        let meta = |k: &str| sent[0].metadata.get(k).map(String::as_str);
        assert_eq!(meta("lookup.user.found"), Some("true"));
        assert_eq!(meta("lookup.features.device.found"), Some("false"));
        assert_eq!(meta(FOUND), Some("false"));
    }

    #[tokio::test]
    async fn from_and_into_must_be_set_together() {
        let config: LookupMiddleware = serde_yaml_ng::from_str("into: user").unwrap();
        let sink = MemoryPublisher::new_local("lookup_half", 16);
        let err = LookupPublisher::new(Box::new(sink), &config, "lookup_test")
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("together"), "{err}");
    }

    fn memory_source(topic: &str) -> crate::endpoints::memory::MemoryConsumer {
        crate::endpoints::memory::MemoryConsumer::new(&crate::models::MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(16),
            enable_nack: true,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_consumer_enriches_before_the_batch_is_returned() {
        let source = memory_source("lookup_consumer_ok");
        source
            .channel()
            .fill_messages(vec![msg(json!({"id": 1})), msg(json!({"id": 2}))])
            .await
            .unwrap();
        let config: LookupMiddleware = serde_yaml_ng::from_str(
            r#"{ from: { static: { body: '{"v":"${payload:id}"}', raw: true } }, into: prev }"#,
        )
        .unwrap();
        let mut consumer = LookupConsumer::new(Box::new(source), &config, "lookup_test")
            .await
            .unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(
            payload(&batch.messages[0]),
            json!({"id": 1, "prev": {"v": "1"}})
        );
        assert_eq!(payload(&batch.messages[1])["prev"]["v"], "2");
        (batch.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_retryable_consumer_lookup_nacks_the_batch_for_redelivery() {
        let source = memory_source("lookup_consumer_retry");
        source
            .channel()
            .fill_messages(vec![msg(json!({"id": 1}))])
            .await
            .unwrap();
        let config: LookupMiddleware = serde_yaml_ng::from_str(
            r#"{ from: { static: { body: "", raw: true, metadata: { http_status_code: "503" } } }, into: prev }"#,
        )
        .unwrap();
        let mut consumer = LookupConsumer::new(Box::new(source), &config, "lookup_test")
            .await
            .unwrap();
        let err = consumer.receive_batch(10).await.err().unwrap();
        assert!(matches!(err, ConsumerError::Connection(_)), "{err}");

        let mut again = memory_source("lookup_consumer_retry");
        let batch = again.receive_batch(10).await.unwrap();
        assert_eq!(payload(&batch.messages[0]), json!({"id": 1}));
    }

    #[tokio::test]
    async fn a_non_retryable_consumer_lookup_drops_only_its_message() {
        let source = memory_source("lookup_consumer_drop");
        let channel = source.channel();
        channel
            .fill_messages(vec![
                msg(json!({"id": 1})),
                CanonicalMessage::new(b"not json".to_vec(), None),
                msg(json!({"id": 3})),
            ])
            .await
            .unwrap();
        let config: LookupMiddleware = serde_yaml_ng::from_str(
            r#"{ from: { static: { body: '{"v":"${payload:id}"}', raw: true } }, into: prev }"#,
        )
        .unwrap();
        let mut consumer = LookupConsumer::new(Box::new(source), &config, "lookup_test")
            .await
            .unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        let ids: Vec<_> = batch
            .messages
            .iter()
            .map(|m| payload(m)["id"].clone())
            .collect();
        assert_eq!(ids, vec![json!(1), json!(3)]);
        (batch.commit)(vec![MessageDisposition::Ack, MessageDisposition::Nack])
            .await
            .unwrap();
        let requeued = channel.drain_messages();
        assert_eq!(requeued.len(), 1);
        assert_eq!(payload(&requeued[0])["id"], 3);
    }

    type Answer = fn(&[CanonicalMessage]) -> Result<Vec<Option<Value>>, PublisherError>;

    /// Answers `lookup_batch` with `answer` and counts the calls.
    struct Batched {
        answer: Answer,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl MessagePublisher for Batched {
        async fn send_batch(&self, _: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
            panic!("a batched lookup must not fall back to send");
        }

        async fn lookup_batch(
            &self,
            requests: &[CanonicalMessage],
        ) -> Option<Result<Vec<Option<Value>>, PublisherError>> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some((self.answer)(requests))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn batched_lookup(answer: Answer) -> (Lookup, Arc<Batched>) {
        let from = Arc::new(Batched {
            answer,
            calls: Default::default(),
        });
        let entry = Entry {
            from: from.clone(),
            metadata: Vec::new(),
            payload: None,
            into: vec!["user".into()],
            found_key: "lookup.user.found".into(),
        };
        let lookup = Lookup {
            entries: vec![entry],
            concurrency: 16,
        };
        (lookup, from)
    }

    #[tokio::test]
    async fn a_batched_from_answers_the_whole_batch_in_one_call() {
        let (lookup, from) = batched_lookup(|requests| {
            Ok(requests
                .iter()
                .map(|r| {
                    let id = payload(r)["id"].as_i64().unwrap();
                    (id % 2 == 1).then(|| json!({"id": id}))
                })
                .collect())
        });
        let batch = vec![
            msg(json!({"id": 1})),
            msg(json!({"id": 2})),
            msg(json!({"id": 3})),
        ];
        let out: Vec<_> = lookup
            .enrich_batch(batch)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(from.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(payload(&out[0])["user"], json!({"id": 1}));
        assert_eq!(payload(&out[1])["user"], Value::Null);
        assert_eq!(
            out[1].metadata.get(FOUND).map(String::as_str),
            Some("false")
        );
        assert_eq!(payload(&out[2])["user"], json!({"id": 3}));
    }

    #[test]
    fn an_incoming_message_cannot_steer_an_http_lookup() {
        let (mut lookup, _) = batched_lookup(|_| Ok(Vec::new()));
        let entry = &mut lookup.entries[0];
        entry.metadata = vec![(
            "http_path".into(),
            CompiledTemplate::compile("/users/${payload:id}", None).unwrap(),
        )];
        let mut incoming = msg(json!({"id": 7}));
        for (k, v) in [
            ("http_method", "DELETE"),
            ("http_path", "/admin"),
            ("http_query", "x=1"),
        ] {
            incoming.metadata.insert(k.into(), v.into());
        }
        incoming.metadata.insert("tenant".into(), "acme".into());
        let request = entry.request(&incoming);
        let meta = |k: &str| request.metadata.get(k).map(String::as_str);
        assert_eq!(meta("http_path"), Some("/users/7"));
        assert_eq!(meta("http_method"), None);
        assert_eq!(meta("http_query"), None);
        assert_eq!(meta("tenant"), Some("acme"));
    }

    #[tokio::test]
    async fn a_batched_failure_fails_every_message_but_not_unparsed_ones_twice() {
        let (lookup, _) =
            batched_lookup(|_| Err(PublisherError::Retryable(anyhow::anyhow!("db down"))));
        let batch = vec![
            msg(json!({"id": 1})),
            CanonicalMessage::new(b"not json".to_vec(), None),
        ];
        let out = lookup.enrich_batch(batch).await;
        assert!(
            matches!(&out[0], Err((_, PublisherError::Retryable(e))) if e.to_string().contains("db down"))
        );
        assert!(matches!(&out[1], Err((_, PublisherError::NonRetryable(_)))));
    }

    #[tokio::test]
    async fn a_short_batched_answer_is_rejected() {
        let (lookup, _) = batched_lookup(|_| Ok(vec![None]));
        let out = lookup
            .enrich_batch(vec![msg(json!({"id": 1})), msg(json!({"id": 2}))])
            .await;
        assert!(out
            .iter()
            .all(|r| matches!(r, Err((_, PublisherError::NonRetryable(_))))));
    }

    #[cfg(all(feature = "http-bulk", feature = "plugin", feature = "test-utils"))]
    #[tokio::test]
    async fn an_http_bulk_query_answers_a_whole_batch_with_one_request() {
        use crate::plugin::test_support::StubHttpServer;
        let answer = r#"{"docs":[{"found":true,"_source":{"name":"Ada"}},{"found":false},{"found":true,"_source":{"name":"Bob"}}]}"#;
        let server = StubHttpServer::start(move |_| (200, answer.to_string()))
            .await
            .unwrap();
        let config = format!(
            r#"
from:
  http_bulk:
    url: "{}"
    query:
      path: /users/_mget
      format: json_array
      request: '{{"_id":"${{payload:user_id}}"}}'
      envelope: '{{"docs":{{requests}}}}'
      responses: /docs
      value: /_source
      found: /found
into: user
"#,
            server.url()
        );
        let (publisher, channel) = lookup("lookup_http_bulk", &config).await;
        let batch = (1..=3).map(|id| msg(json!({"user_id": id}))).collect();
        publisher.send_batch(batch).await.unwrap();

        let asked = server.requests();
        assert_eq!(asked.len(), 1, "one request for three messages");
        assert_eq!(
            String::from_utf8_lossy(&asked[0].body),
            r#"{"docs":[{"_id":"1"},{"_id":"2"},{"_id":"3"}]}"#
        );
        let sent = channel.drain_messages();
        assert_eq!(payload(&sent[0])["user"], json!({"name": "Ada"}));
        assert_eq!(payload(&sent[1]), json!({"user_id": 2, "user": null}));
        assert_eq!(payload(&sent[2])["user"]["name"], "Bob");
    }
}
