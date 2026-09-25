//! An in-memory endpoint that exists to exercise the plugin boundary.
//!
//! It is deliberately trivial — a named queue in a `Mutex` — so that a test
//! failure means the ABI, loader or SDK is wrong rather than a broker being
//! slow. Failure injection lets the tests assert that error classes survive the
//! round trip through C.
//!
//! Configuration:
//!
//! ```yaml
//! custom:
//!   name: fixture
//!   config:
//!     queue: orders            # required: which in-process queue to attach to
//!     fail_receive: retryable  # none | retryable | permanent | end_of_stream
//!     fail_send: none          # none | retryable | permanent
//!     fail_send_at: [1, 3]     # publish the rest, fail these: a partial batch
//!     panic_on_receive: false  # exercises the SDK's panic containment
//!     commit_requires_order: true
//!     requires_ordered_publish: false
//!     respond: false           # answer each published message with `re:<payload>`
//!     idempotent: false        # report the publisher as an idempotent sink
//!     acknowledges: true       # report the consumer as acknowledging
//! ```
//!
//! The same plugin also exports a middleware under the name `fixture`:
//!
//! ```yaml
//! middlewares:
//!   - custom:
//!       name: fixture
//!       config:
//!         drop_prefix: "skip-"  # messages starting with this are dropped
//!         suffix: "-seen"       # appended to every surviving payload
//!         fail: false           # make the middleware return an error
//! ```
//!
//! Note that the queues live in whichever copy of this library is loaded: a
//! directly linked fixture and a `dlopen`ed one do not share state, which is
//! exactly what makes it a useful plugin test.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use mq_bridge::errors::{ConsumerError, PublisherError};
use mq_bridge::traits::{
    BatchCommitFunc, CustomEndpointFactory, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher,
};
use mq_bridge::{CanonicalMessage, ReceivedBatch, SentBatch};
use serde::Deserialize;

mq_bridge::export_endpoint_plugins! {
    { name: "fixture", factory: FixtureFactory, middleware: FixtureMiddlewareFactory },
    // A second plugin in the same library, sharing the fixture's queues.
    {
        name: "fixture-sink",
        factory: FixtureFactory,
        capabilities: mq_bridge::plugin::sdk::CAPABILITIES_OUTPUT_ONLY,
    },
}

/// Injected failure for the input side.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveFailure {
    #[default]
    None,
    Retryable,
    Permanent,
    EndOfStream,
}

/// Injected failure for the output side.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendFailure {
    #[default]
    None,
    Retryable,
    Permanent,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureConfig {
    /// Queue to attach to. Defaults to the route name.
    #[serde(default)]
    pub queue: Option<String>,
    #[serde(default)]
    pub fail_receive: ReceiveFailure,
    #[serde(default)]
    pub fail_send: SendFailure,
    /// Indices of the batch that fail while every other message is published,
    /// which is the only way to reach [`SentBatch::Partial`] from a test. Takes
    /// precedence over `fail_send`, which then only classifies these failures.
    #[serde(default)]
    pub fail_send_at: Vec<usize>,
    #[serde(default)]
    pub panic_on_receive: bool,
    #[serde(default = "default_true")]
    pub commit_requires_order: bool,
    /// Defaults to `false` like the trait itself, so a test that asks for
    /// ordered publishing has to say so and cannot pass by accident.
    #[serde(default)]
    pub requires_ordered_publish: bool,
    /// Returns a response for every published message, for request/reply.
    #[serde(default)]
    pub respond: bool,
    /// Only reported through the delivery flags; the queue itself never dedups.
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default = "default_true")]
    pub acknowledges: bool,
}

fn default_true() -> bool {
    true
}

/// The queue backing one fixture endpoint name.
#[derive(Default)]
struct Queue {
    ready: VecDeque<CanonicalMessage>,
}

type SharedQueue = Arc<Mutex<Queue>>;

fn queue(name: &str) -> SharedQueue {
    static QUEUES: OnceLock<Mutex<HashMap<String, SharedQueue>>> = OnceLock::new();
    let queues = QUEUES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut queues = queues.lock().expect("fixture queue registry poisoned");
    Arc::clone(queues.entry(name.to_string()).or_default())
}

/// Number of messages waiting in a queue.
pub fn queue_depth(name: &str) -> usize {
    queue(name)
        .lock()
        .expect("fixture queue poisoned")
        .ready
        .len()
}

/// Queue that records each committed message, so a test on the *other* side of
/// the ABI can observe when acknowledgement actually happened. Every commit
/// appends the message with a `disposition` of `ack`, `nack` or `reply`; a
/// reply's payload is recorded under `reply`.
pub fn commit_log_queue(name: &str) -> String {
    format!("{name}#committed")
}

#[derive(Debug, Default)]
pub struct FixtureFactory;

fn resolve(route_name: &str, value: &serde_json::Value) -> anyhow::Result<(FixtureConfig, String)> {
    let config: FixtureConfig =
        serde_json::from_value(value.clone()).context("invalid fixture endpoint configuration")?;
    let name = config
        .queue
        .clone()
        .unwrap_or_else(|| route_name.to_owned());
    if name.trim().is_empty() {
        return Err(anyhow!("fixture `queue` must not be empty"));
    }
    Ok((config, name))
}

/// Hand-written rather than derived, because that is what a plugin in another
/// language has to do and the ABI carries nothing but the JSON.
fn fixture_config_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "title": "Fixture queue",
        "additionalProperties": false,
        "properties": {
            "queue": {
                "type": "string",
                "description": "In-process queue to attach to. Defaults to the route name.",
                "x-mqb-uri": "path",
            },
            "fail_receive": {
                "type": "string",
                "enum": ["none", "retryable", "permanent", "end_of_stream"],
                "default": "none",
            },
            "fail_send": {
                "type": "string",
                "enum": ["none", "retryable", "permanent"],
                "default": "none",
            },
            "fail_send_at": { "type": "array", "items": { "type": "integer" } },
            "panic_on_receive": { "type": "boolean", "default": false },
            "commit_requires_order": { "type": "boolean", "default": true },
            "requires_ordered_publish": { "type": "boolean", "default": false },
            "respond": { "type": "boolean", "default": false },
            "idempotent": { "type": "boolean", "default": false },
            "acknowledges": { "type": "boolean", "default": true },
        },
    })
}

#[async_trait]
impl CustomEndpointFactory for FixtureFactory {
    fn config_schema(&self) -> Option<serde_json::Value> {
        Some(fixture_config_schema())
    }

    fn idempotent_sink(&self, config: &serde_json::Value) -> bool {
        FixtureConfig::deserialize(config).is_ok_and(|config| config.idempotent)
    }

    fn acknowledges(&self, config: &serde_json::Value) -> bool {
        FixtureConfig::deserialize(config).map_or(true, |config| config.acknowledges)
    }

    async fn create_consumer(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let (config, name) = resolve(route_name, config)?;
        Ok(Box::new(FixtureConsumer {
            queue: queue(&name),
            name,
            config,
        }))
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let (config, name) = resolve(route_name, config)?;
        tracing::info!(queue = %name, "fixture opened a publisher");
        Ok(Box::new(FixturePublisher {
            queue: queue(&name),
            name,
            config,
        }))
    }
}

struct FixtureConsumer {
    queue: SharedQueue,
    name: String,
    config: FixtureConfig,
}

/// Messages handed out but not yet committed.
///
/// Dropping the batch's commit function — which is what happens when a route
/// shuts down mid-batch, and what the plugin loader does for an uncommitted
/// batch — must put them back, exactly like a broker redelivering unacked
/// messages.
struct InFlight {
    queue: SharedQueue,
    messages: Option<Vec<CanonicalMessage>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let Some(messages) = self.messages.take() else {
            return;
        };
        let mut queue = self.queue.lock().expect("fixture queue poisoned");
        for message in messages.into_iter().rev() {
            queue.ready.push_front(message);
        }
    }
}

#[async_trait]
impl MessageConsumer for FixtureConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if self.config.panic_on_receive {
            panic!("fixture was configured to panic on receive");
        }
        match self.config.fail_receive {
            ReceiveFailure::None => {}
            ReceiveFailure::Retryable => {
                return Err(ConsumerError::Connection(anyhow!(
                    "fixture injected a retryable receive failure"
                )))
            }
            ReceiveFailure::Permanent => {
                return Err(ConsumerError::Permanent(anyhow!(
                    "fixture injected a permanent receive failure"
                )))
            }
            ReceiveFailure::EndOfStream => return Err(ConsumerError::EndOfStream),
        }

        let mut in_flight = Vec::new();
        {
            let mut queue = self.queue.lock().expect("fixture queue poisoned");
            while in_flight.len() < max_messages {
                match queue.ready.pop_front() {
                    Some(message) => in_flight.push(message),
                    None => break,
                }
            }
        }
        if in_flight.is_empty() {
            return Ok(ReceivedBatch::empty());
        }

        let messages = in_flight.clone();
        let shared = Arc::clone(&self.queue);
        let commit_log = queue(&commit_log_queue(&self.name));
        let mut in_flight = InFlight {
            queue: Arc::clone(&self.queue),
            messages: Some(in_flight),
        };
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                // Check before taking, so a miscount leaves the messages for
                // `InFlight::drop` to requeue instead of losing them.
                let pending = in_flight.messages.as_ref().map_or(0, Vec::len);
                if dispositions.len() != pending {
                    return Err(anyhow!(
                        "fixture commit got {} dispositions for {pending} messages",
                        dispositions.len(),
                    ));
                }
                let in_flight = in_flight.messages.take().unwrap_or_default();
                // Requeue at the front so a nack is redelivered before newer
                // messages, the way a broker's unacked redelivery behaves.
                let mut queue = shared.lock().expect("fixture queue poisoned");
                let mut log = commit_log.lock().expect("fixture queue poisoned");
                for (message, disposition) in in_flight.into_iter().zip(dispositions).rev() {
                    let nacked = matches!(disposition, MessageDisposition::Nack);
                    let mut record = message.clone();
                    let label = match &disposition {
                        MessageDisposition::Ack => "ack",
                        MessageDisposition::Nack => "nack",
                        MessageDisposition::Reply(reply) => {
                            record
                                .metadata
                                .insert("reply".to_string(), reply.get_payload_str().into_owned());
                            "reply"
                        }
                    };
                    record
                        .metadata
                        .insert("disposition".to_string(), label.to_string());
                    log.ready.push_back(record);
                    if nacked {
                        queue.ready.push_front(message);
                    }
                }
                Ok(())
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn commit_requires_order(&self) -> bool {
        self.config.commit_requires_order
    }

    async fn status(&self) -> EndpointStatus {
        queue_status(&self.name)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct FixturePublisher {
    queue: SharedQueue,
    name: String,
    config: FixtureConfig,
}

fn queue_status(name: &str) -> EndpointStatus {
    EndpointStatus {
        target: name.to_string(),
        pending: Some(queue_depth(name)),
        details: serde_json::json!({ "fixture": true }),
        ..Default::default()
    }
}

/// The response to one published message, when `respond` is set.
fn response_to(message: &CanonicalMessage) -> CanonicalMessage {
    let mut response = CanonicalMessage::from(format!("re:{}", message.get_payload_str()));
    response.metadata.insert(
        "request_id".to_string(),
        format!("{:x}", message.message_id),
    );
    response
}

impl FixturePublisher {
    fn responses(&self, delivered: &[CanonicalMessage]) -> Option<Vec<CanonicalMessage>> {
        self.config
            .respond
            .then(|| delivered.iter().map(response_to).collect())
    }

    /// Publishes everything but the indices in `fail_send_at`, which come back
    /// as failures — a batch that half landed.
    fn publish_all_but_failed(&self, messages: Vec<CanonicalMessage>) -> SentBatch {
        let mut delivered = Vec::with_capacity(messages.len());
        let mut failed = Vec::new();
        for (index, message) in messages.into_iter().enumerate() {
            if self.config.fail_send_at.contains(&index) {
                let cause = anyhow!("fixture failed message {index} of the batch");
                let error = match self.config.fail_send {
                    SendFailure::Permanent => PublisherError::NonRetryable(cause),
                    SendFailure::None | SendFailure::Retryable => PublisherError::Retryable(cause),
                };
                failed.push((message, error));
            } else {
                delivered.push(message);
            }
        }
        let responses = self.responses(&delivered);
        self.queue
            .lock()
            .expect("fixture queue poisoned")
            .ready
            .extend(delivered);
        SentBatch::Partial { responses, failed }
    }
}

#[async_trait]
impl MessagePublisher for FixturePublisher {
    fn requires_ordered_publish(&self) -> bool {
        self.config.requires_ordered_publish
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if !self.config.fail_send_at.is_empty() {
            return Ok(self.publish_all_but_failed(messages));
        }
        match self.config.fail_send {
            SendFailure::None => {}
            SendFailure::Retryable => {
                return Err(PublisherError::Retryable(anyhow!(
                    "fixture injected a retryable send failure"
                )))
            }
            SendFailure::Permanent => {
                return Err(PublisherError::NonRetryable(anyhow!(
                    "fixture injected a permanent send failure"
                )))
            }
        }
        let responses = self.responses(&messages);
        metrics::counter!("fixture_published_total", "queue" => self.name.clone())
            .increment(messages.len() as u64);
        let mut queue = self.queue.lock().expect("fixture queue poisoned");
        queue.ready.extend(messages);
        Ok(match responses {
            Some(responses) => SentBatch::Partial {
                responses: Some(responses),
                failed: Vec::new(),
            },
            None => SentBatch::Ack,
        })
    }

    async fn status(&self) -> EndpointStatus {
        queue_status(&self.name)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_nacked_message_is_requeued_at_the_front() {
        let factory = FixtureFactory;
        let config = serde_json::json!({ "queue": "unit-nack" });
        let publisher = factory.create_publisher("route", &config).await.unwrap();
        let mut consumer = factory.create_consumer("route", &config).await.unwrap();

        publisher
            .send_batch(vec![
                CanonicalMessage::from("first"),
                CanonicalMessage::from("second"),
            ])
            .await
            .unwrap();

        let batch = consumer.receive_batch(2).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        (batch.commit)(vec![MessageDisposition::Nack, MessageDisposition::Ack])
            .await
            .unwrap();

        let batch = consumer.receive_batch(2).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].get_payload_str(), "first");
    }

    #[tokio::test]
    async fn an_empty_queue_yields_an_empty_batch() {
        let factory = FixtureFactory;
        let config = serde_json::json!({ "queue": "unit-empty" });
        let mut consumer = factory.create_consumer("route", &config).await.unwrap();
        assert!(consumer.receive_batch(4).await.unwrap().messages.is_empty());
    }
}

/// Configuration of the middleware this plugin also exports.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureMiddlewareConfig {
    /// Messages whose payload starts with this are dropped.
    #[serde(default)]
    pub drop_prefix: Option<String>,
    /// Appended to every surviving payload, so a test can tell whether the
    /// middleware ran and on which side.
    #[serde(default)]
    pub suffix: Option<String>,
    /// Return an error instead of filtering.
    #[serde(default)]
    pub fail: bool,
}

#[derive(Debug, Default)]
pub struct FixtureMiddlewareFactory;

#[async_trait]
impl mq_bridge::plugin::sdk::MiddlewareFactory for FixtureMiddlewareFactory {
    fn config_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "title": "Fixture filter",
            "additionalProperties": false,
            "properties": {
                "drop_prefix": { "type": "string" },
                "suffix": { "type": "string" },
                "fail": { "type": "boolean", "default": false },
            },
        }))
    }

    async fn create(
        &self,
        _route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn mq_bridge::plugin::sdk::BatchFilter>> {
        let config: FixtureMiddlewareConfig = serde_json::from_value(config.clone())
            .context("invalid fixture middleware configuration")?;
        Ok(Box::new(FixtureFilter { config }))
    }
}

struct FixtureFilter {
    config: FixtureMiddlewareConfig,
}

impl FixtureFilter {
    fn filter(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        if self.config.fail {
            return Err(anyhow!("fixture middleware was configured to fail"));
        }
        Ok(messages
            .into_iter()
            .map(|message| {
                let payload = message.get_payload_str().to_string();
                if self
                    .config
                    .drop_prefix
                    .as_ref()
                    .is_some_and(|prefix| payload.starts_with(prefix))
                {
                    return None;
                }
                let Some(suffix) = &self.config.suffix else {
                    return Some(message);
                };
                let mut rewritten = CanonicalMessage::from(format!("{payload}{suffix}"));
                rewritten.message_id = message.message_id;
                rewritten.metadata = message.metadata;
                Some(rewritten)
            })
            .collect())
    }
}

#[async_trait]
impl mq_bridge::plugin::sdk::BatchFilter for FixtureFilter {
    async fn on_receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        self.filter(messages)
    }

    async fn on_send(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        self.filter(messages)
    }
}
