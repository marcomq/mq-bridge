//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Call a route like a function: `users.call([("path", key)]).await` returns the route's answer.
//!
//! A [`Caller`] is a normal deployed [`Route`] whose input is an in-process `memory`
//! topic. Each call puts one request on that topic and waits for its reply, so everything
//! a route does applies unchanged: input and output middlewares (`lookup`, `parallel`,
//! `retry`, plugin middlewares), the handler, `batch_size`, `concurrency`, ordered sinks
//! and graceful shutdown. Calls made at the same time are read as one batch, so a batched
//! `lookup` queries once for all of them.
//!
//! ```no_run
//! use mq_bridge::Caller;
//!
//! #[derive(serde::Deserialize)]
//! struct User { id: i64, name: String }
//!
//! # async fn example() -> anyhow::Result<()> {
//! // The route `users` from `routes.yaml`. Without an `input`, the caller reads from its
//! // own memory topic; without an `output`, the route answers with the message.
//! let users = Caller::from_file("routes.yaml", "users").await?;
//! let user: Option<User> = users.call([("path", "/u/7")]).await?;
//! let found: Vec<anyhow::Result<Option<User>>> =
//!     users.call_many([[("path", "/u/7")], [("path", "/u/8")]]).await;
//!
//! // Or built in code: the caller adds the memory input and the answering output itself.
//! let echo = Caller::builder("echo").with_batch_size(64).deploy().await?;
//! assert_eq!(echo.call_message("hi".into()).await?.get_payload_str(), "hi");
//! # Ok(())
//! # }
//! ```
//!
//! How a call ends:
//! - The output answers (`response: {}`, `http`, `request`, an answering plugin endpoint):
//!   [`Caller::call`] returns that answer, parsed as JSON into the type you ask for.
//! - The output only accepts the message (a plain sink): [`Caller::send`] returns
//!   [`Sent::Ack`]; [`Caller::call`] returns an error.
//! - The message fails: both return the error, as soon as the route gives up on it
//!   (after its `retry` middleware, if any). The route keeps running.
//! - No answer within the input's `request_timeout_ms` (default 30 s): a timeout error.

use crate::endpoints::memory::MemoryPublisher;
use crate::models::{Endpoint, EndpointType, MemoryConfig, Route};
use crate::traits::{MessagePublisher, PublisherError};
use crate::{CanonicalMessage, Sent};
use anyhow::{anyhow, bail, Context};
use serde::de::DeserializeOwned;
use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;
use std::path::Path;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// The fields of one call, sent as a JSON object: `[("path", key)]`, a `Vec` of pairs,
/// a `HashMap` / `BTreeMap`, or a `serde_json::Value` (e.g. `json!({"id": 7, "day": d})`
/// for values of different types, or `serde_json::to_value(&my_struct)?`).
pub trait CallFields {
    /// Writes the fields as one JSON value.
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()>;
}

fn write_pairs<K: AsRef<str>, V: Serialize>(
    pairs: impl IntoIterator<Item = (K, V)>,
    out: &mut Vec<u8>,
) -> serde_json::Result<()> {
    let mut serializer = serde_json::Serializer::new(out);
    let mut map = serializer.serialize_map(None)?;
    for (key, value) in pairs {
        map.serialize_entry(key.as_ref(), &value)?;
    }
    map.end()
}

impl<K: AsRef<str>, V: Serialize, const N: usize> CallFields for [(K, V); N] {
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        write_pairs(self, out)
    }
}

impl<K: AsRef<str>, V: Serialize> CallFields for Vec<(K, V)> {
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        write_pairs(self, out)
    }
}

impl<K: AsRef<str>, V: Serialize, S: BuildHasher> CallFields for HashMap<K, V, S> {
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        write_pairs(self, out)
    }
}

impl<K: AsRef<str>, V: Serialize> CallFields for BTreeMap<K, V> {
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        write_pairs(self, out)
    }
}

impl CallFields for serde_json::Value {
    fn write_json(self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        serde_json::to_writer(out, &self)
    }
}

/// Waiting calls a caller's input holds when its config sets no `capacity`. With the memory
/// default of 100, thousands of concurrent callers contend for a free slot and batches stay
/// small: 4,096 callers on a batched Postgres lookup went from 143k to 193k calls/s.
const CALLER_CAPACITY: usize = 4096;

/// The memory topic a caller reads from when its route names no input.
pub(crate) fn default_topic(name: &str) -> String {
    format!("mqb-caller-{name}")
}

/// A deployed route that answers calls. Cheap to clone; all clones call the same route.
#[derive(Clone)]
pub struct Caller {
    name: String,
    requests: Arc<MemoryPublisher>,
}

static CALLER_REGISTRY: OnceLock<RwLock<HashMap<String, Caller>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Caller>> {
    CALLER_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

impl Caller {
    /// Deploys `route` under `name` and returns a caller for it.
    ///
    /// The input must be an in-process `memory` endpoint (not `ipc://`, not
    /// `subscribe_mode`). Its `request_reply` is switched on, so a failed message is
    /// answered with its error instead of stopping the route. Without a `capacity`, it
    /// holds 4,096 waiting calls instead of 100. The caller is also
    /// registered under `name` for [`Caller::get`].
    pub async fn deploy(name: &str, mut route: Route) -> anyhow::Result<Self> {
        let EndpointType::Memory(input) = &mut route.input.endpoint_type else {
            bail!("Caller '{name}': the route's input must be a `memory` endpoint");
        };
        if input.subscribe_mode {
            bail!("Caller '{name}': the memory input cannot use `subscribe_mode`");
        }
        input.request_reply = true;
        input.capacity.get_or_insert(CALLER_CAPACITY);
        let requests = MemoryPublisher::new(input)?;
        if !requests.answers_requests() {
            bail!("Caller '{name}': the memory input must be in-process, not a transport URL");
        }
        route.deploy(name).await?;
        let caller = Self {
            name: name.to_string(),
            requests: Arc::new(requests),
        };
        let mut map = registry().write().unwrap_or_else(PoisonError::into_inner);
        map.insert(name.to_string(), caller.clone());
        Ok(caller)
    }

    /// Deploys a route from an internal memory topic to `output` and returns its caller.
    /// Use [`Caller::builder`] for a handler, input middlewares or route options.
    pub async fn new(name: &str, output: Endpoint) -> anyhow::Result<Self> {
        Self::builder(name).with_output(output).deploy().await
    }

    /// Starts a caller route that reads from an internal memory topic and answers with the
    /// message; add middlewares and options, then [`CallerBuilder::deploy`].
    pub fn builder(name: &str) -> CallerBuilder {
        let input = Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: default_topic(name),
            ..Default::default()
        }));
        CallerBuilder {
            name: name.to_string(),
            route: Route::new(input, Endpoint::new_response()),
        }
    }

    /// Deploys the route `name` from a YAML or JSON config file (a `routes:` document, a
    /// bare `{name: route}` map, or one route body).
    ///
    /// Unlike [`Route::from_file`], the route may leave out its `input` (or give only its
    /// `middlewares`): the caller then reads from an internal memory topic. A missing
    /// `output` becomes `response: {}`, so the route answers with the message.
    pub async fn from_file(path: impl AsRef<Path>, name: &str) -> anyhow::Result<Self> {
        let config = crate::config_file::read_file(path.as_ref())?;
        Self::from_config(config, name).await
    }

    /// Like [`Caller::from_file`], but parses the config from a string, e.g. one route body
    /// written inline next to the code that calls it.
    pub async fn from_str(text: &str, name: &str) -> anyhow::Result<Self> {
        Self::from_config(crate::config_file::parse_str(text)?, name).await
    }

    /// Like [`Caller::from_file`], but takes an already-parsed config document.
    pub async fn from_config(config: serde_json::Value, name: &str) -> anyhow::Result<Self> {
        Self::deploy(name, Self::route_from_config(config, name)?).await
    }

    /// The route [`Caller::from_config`] would deploy, with the caller's defaults filled
    /// in, to add a handler before [`Caller::deploy`].
    pub fn route_from_config(config: serde_json::Value, name: &str) -> anyhow::Result<Route> {
        crate::config_file::caller_route(config, name)
    }

    /// Sends `fields` as one JSON object through the route and parses its answer as `T`.
    ///
    /// Ask for an `Option<T>` when the route may answer `null`, as a `lookup` does for a key
    /// without a row. Fails if the message fails, times out, the output accepts it without
    /// an answer, or the answer is not JSON of that shape.
    pub async fn call<T: DeserializeOwned>(&self, fields: impl CallFields) -> anyhow::Result<T> {
        let answer = self.call_message(self.request(fields)?).await?;
        self.parse(&answer)
    }

    /// Like [`Caller::call`] for several requests: they reach the route as one batch, and
    /// the answers come back in their order, each failing or succeeding on its own.
    pub async fn call_many<T: DeserializeOwned, F: CallFields>(
        &self,
        requests: impl IntoIterator<Item = F>,
    ) -> Vec<anyhow::Result<T>> {
        let mut messages = Vec::new();
        let encode_errors: Vec<Option<anyhow::Error>> = requests
            .into_iter()
            .map(|fields| self.request(fields).map(|m| messages.push(m)).err())
            .collect();
        let mut answers = self.call_many_messages(messages).await.into_iter();
        encode_errors
            .into_iter()
            .map(|error| match error {
                Some(error) => Err(error),
                None => answers
                    .next()
                    .unwrap_or_else(|| Err(anyhow!("Caller '{}': no answer", self.name)))
                    .and_then(|answer| self.parse(&answer)),
            })
            .collect()
    }

    fn request(&self, fields: impl CallFields) -> anyhow::Result<CanonicalMessage> {
        let mut payload = Vec::with_capacity(64);
        fields
            .write_json(&mut payload)
            .with_context(|| format!("Caller '{}': cannot encode the request", self.name))?;
        Ok(CanonicalMessage::new(payload, None))
    }

    fn parse<T: DeserializeOwned>(&self, answer: &CanonicalMessage) -> anyhow::Result<T> {
        serde_json::from_slice(&answer.payload).with_context(|| {
            format!(
                "Caller '{}': the answer does not parse as the requested type: {}",
                self.name,
                answer.get_payload_str()
            )
        })
    }

    /// Sends `message` through the route and returns its answer as a message, e.g. to
    /// forward it with its metadata.
    ///
    /// Fails if the message fails, times out, or the output accepts it without an answer.
    pub async fn call_message(
        &self,
        message: CanonicalMessage,
    ) -> anyhow::Result<CanonicalMessage> {
        match self.send(message).await? {
            Sent::Response(response) => Ok(response),
            Sent::Ack => Err(self.no_answer()),
        }
    }

    /// Sends `message` through the route and returns how it ended: [`Sent::Response`]
    /// with the answer, or [`Sent::Ack`] when the output only accepted it.
    pub async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Sent> {
        self.requests.send(message).await.map_err(|e| self.named(e))
    }

    /// Adds the caller's name to `error`, keeping its retryable/connection/permanent kind.
    fn named(&self, error: PublisherError) -> anyhow::Error {
        let context = format!("Caller '{}'", self.name);
        anyhow::Error::from(match error {
            PublisherError::Retryable(e) => PublisherError::Retryable(e.context(context)),
            PublisherError::NonRetryable(e) => PublisherError::NonRetryable(e.context(context)),
            PublisherError::Connection(e) => PublisherError::Connection(e.context(context)),
        })
    }

    /// Like [`Caller::call_message`] for several messages: they reach the route as one
    /// batch, and the answers come back in their order, each failing or succeeding on its own.
    pub async fn call_many_messages(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Vec<anyhow::Result<CanonicalMessage>> {
        self.send_many(messages)
            .await
            .into_iter()
            .map(|sent| match sent? {
                Sent::Response(response) => Ok(response),
                Sent::Ack => Err(self.no_answer()),
            })
            .collect()
    }

    /// Like [`Caller::send`] for several messages, sent as one batch; results keep their order.
    pub async fn send_many(&self, messages: Vec<CanonicalMessage>) -> Vec<anyhow::Result<Sent>> {
        self.requests
            .request_many(messages)
            .await
            .into_iter()
            .map(|sent| sent.map_err(|e| self.named(e)))
            .collect()
    }

    fn no_answer(&self) -> anyhow::Error {
        anyhow!(
            "Caller '{}': the route accepted the message without an answer; end its \
             output with `response: {{}}` or an endpoint that replies",
            self.name
        )
    }

    /// The route's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The caller deployed under `name`, while its route is running.
    pub fn get(name: &str) -> Option<Self> {
        crate::route::get_route(name)?;
        let map = registry().read().unwrap_or_else(PoisonError::into_inner);
        map.get(name).cloned()
    }

    /// Stops the route; calls still waiting fail. Returns false if it was not running.
    pub async fn stop(&self) -> bool {
        registry()
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.name);
        Route::stop(&self.name).await
    }
}

/// A caller route under construction, from [`Caller::builder`]. Middlewares go on its
/// input, in the order written; the output answers with the message unless replaced.
#[derive(Debug)]
#[must_use = "a CallerBuilder does nothing until `deploy` is awaited"]
pub struct CallerBuilder {
    name: String,
    route: Route,
}

impl CallerBuilder {
    fn memory_input(&mut self) -> &mut MemoryConfig {
        match &mut self.route.input.endpoint_type {
            EndpointType::Memory(config) => config,
            _ => unreachable!("Caller::builder always reads from a memory input"),
        }
    }

    /// How long a call waits for its answer before it fails; defaults to 30 s.
    pub fn with_request_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.memory_input().request_timeout_ms = Some(timeout_ms);
        self
    }

    /// How many calls may wait to be read; defaults to 4,096.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.memory_input().capacity = Some(capacity);
        self
    }

    /// How many batches the route sends to its output at once; see [`Route::with_concurrency`].
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.route = self.route.with_concurrency(concurrency);
        self
    }

    /// Appends a `lookup` to the input; see [`Endpoint::with_lookup`].
    pub fn with_lookup(mut self, lookup: crate::models::LookupMiddleware) -> Self {
        self.route.input = self.route.input.with_lookup(lookup);
        self
    }

    /// Appends `parallel` to the input: the middlewares before it run on up to
    /// `max_in_flight` batches at once.
    pub fn with_parallel(mut self, max_in_flight: usize) -> Self {
        self.route.input = self.route.input.with_parallel(max_in_flight);
        self
    }

    /// Appends any middleware to the input.
    pub fn add_middleware(mut self, middleware: crate::models::Middleware) -> Self {
        self.route.input = self.route.input.add_middleware(middleware);
        self
    }

    /// The most calls read as one batch; see [`Route::with_batch_size`].
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.route = self.route.with_batch_size(batch_size);
        self
    }

    /// Runs `handler` on each message before the output.
    pub fn with_handler(mut self, handler: impl crate::traits::Handler + 'static) -> Self {
        self.route = self.route.with_handler(handler);
        self
    }

    /// Replaces the answering `response` output, e.g. with an endpoint that replies itself.
    pub fn with_output(mut self, output: Endpoint) -> Self {
        self.route.output = output;
        self
    }

    /// Deploys the route and returns its caller; see [`Caller::deploy`].
    pub async fn deploy(self) -> anyhow::Result<Caller> {
        Caller::deploy(&self.name, self.route).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::HandlerError;
    use crate::Handled;

    fn unique(prefix: &str) -> String {
        format!("{prefix}_{}", fast_uuid_v7::gen_id_str())
    }

    #[tokio::test]
    async fn a_call_returns_the_routes_answer_and_errors_fast() {
        let name = unique("caller_answer");
        let route = Route::new(
            Endpoint::new_memory(&unique("caller_in"), 64),
            Endpoint::new_response(),
        )
        .with_handler(|mut msg: CanonicalMessage| async move {
            match msg.get_payload_str().as_ref() {
                "bad" => Err(HandlerError::NonRetryable(anyhow!("cannot enrich"))),
                payload => {
                    let answer = format!("enriched {payload}");
                    msg.set_payload_str(answer);
                    Ok(Handled::Publish(msg))
                }
            }
        });
        let caller = Caller::deploy(&name, route).await.unwrap();

        let answer = caller.call_message("a".into()).await.unwrap();
        assert_eq!(answer.get_payload_str(), "enriched a");
        let started = std::time::Instant::now();
        let err = caller.call_message("bad".into()).await.unwrap_err();
        assert!(err.to_string().contains("cannot enrich"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let again = Caller::get(&name)
            .unwrap()
            .call_message("b".into())
            .await
            .unwrap();
        assert_eq!(again.get_payload_str(), "enriched b");

        let many = caller
            .call_many_messages(vec!["c".into(), "bad".into(), "d".into()])
            .await;
        assert_eq!(many[0].as_ref().unwrap().get_payload_str(), "enriched c");
        assert!(many[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("cannot enrich"));
        assert_eq!(many[2].as_ref().unwrap().get_payload_str(), "enriched d");

        assert!(caller.stop().await);
        assert!(Caller::get(&name).is_none());
    }

    #[tokio::test]
    async fn a_sink_without_answer_acks_and_call_says_so() {
        let caller = Caller::new(&unique("caller_sink"), Endpoint::new(EndpointType::Null))
            .await
            .unwrap();
        assert!(matches!(caller.send("x".into()).await.unwrap(), Sent::Ack));
        let err = caller.call_message("x".into()).await.unwrap_err();
        assert!(err.to_string().contains("without an answer"), "{err}");
        caller.stop().await;
    }

    /// Answers every message like an inference plugin endpoint, recording batch sizes.
    #[derive(Debug, Default)]
    struct Answering(Arc<std::sync::Mutex<Vec<usize>>>);

    struct AnsweringSink(Arc<std::sync::Mutex<Vec<usize>>>);

    #[async_trait::async_trait]
    impl MessagePublisher for AnsweringSink {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<crate::SentBatch, crate::traits::PublisherError> {
            self.0.lock().unwrap().push(messages.len());
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let responses = messages
                .into_iter()
                .map(|mut m| {
                    let answer = format!("label for {}", m.get_payload_str());
                    m.set_payload_str(answer);
                    m
                })
                .collect();
            Ok(crate::SentBatch::Partial {
                responses: Some(responses),
                failed: Vec::new(),
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl crate::traits::CustomEndpointFactory for Answering {
        async fn create_publisher(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(AnsweringSink(self.0.clone())))
        }
    }

    #[tokio::test]
    async fn concurrent_calls_to_an_answering_endpoint_are_batched() {
        let endpoint = unique("answering");
        let factory = Answering::default();
        let batches = factory.0.clone();
        crate::extensions::register_endpoint_factory(&endpoint, Arc::new(factory)).unwrap();
        let output = Endpoint::new(EndpointType::Custom {
            name: endpoint,
            config: serde_json::Value::Null,
        });
        let route = Route::new(Endpoint::new_memory(&unique("caller_batch"), 256), output)
            .with_batch_size(64);
        let caller = Caller::deploy(&unique("caller_batch"), route)
            .await
            .unwrap();

        let calls: Vec<_> = (0..200)
            .map(|i| {
                let caller = caller.clone();
                tokio::spawn(async move { caller.call_message(format!("{i}").into()).await })
            })
            .collect();
        for (i, call) in calls.into_iter().enumerate() {
            let answer = call.await.unwrap().unwrap();
            assert_eq!(answer.get_payload_str(), format!("label for {i}"));
        }
        let batches_so_far = {
            let batches = batches.lock().unwrap();
            assert_eq!(batches.iter().sum::<usize>(), 200);
            assert!(batches.iter().any(|&n| n > 1), "{batches:?}");
            batches.len()
        };

        let many = caller
            .call_many_messages((0..50).map(|i| format!("m{i}").into()).collect())
            .await;
        for (i, answer) in many.into_iter().enumerate() {
            assert_eq!(answer.unwrap().get_payload_str(), format!("label for m{i}"));
        }
        assert_eq!(batches.lock().unwrap()[batches_so_far..], [50]);
        caller.stop().await;
    }

    fn config_file(dir: &tempfile::TempDir, routes: serde_json::Value) -> std::path::PathBuf {
        let path = dir.path().join("routes.json");
        let config = serde_json::json!({ "routes": routes });
        std::fs::write(&path, config.to_string()).unwrap();
        path
    }

    #[tokio::test]
    async fn a_route_from_a_file_enriches_with_lookup_and_answers() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique("caller_lookup");
        let path = config_file(
            &dir,
            serde_json::json!({ &name: {
                "input": {
                    "memory": { "topic": unique("caller_lookup_in") },
                    "middlewares": [
                        { "lookup": {
                            "from": { "static": { "body": r#"{"tier":"gold"}"#, "raw": true } },
                            "into": "customer"
                        } },
                        { "parallel": { "max_in_flight": 4 } }
                    ]
                },
                "output": { "response": {} }
            }}),
        );
        let caller = Caller::from_file(&path, &name).await.unwrap();
        let answer: serde_json::Value = caller.call([("id", 7)]).await.unwrap();
        assert_eq!(
            answer,
            serde_json::json!({"id": 7, "customer": {"tier": "gold"}})
        );
        caller.stop().await;
    }

    #[tokio::test]
    async fn a_built_caller_enriches_with_lookup_and_answers() {
        let tier: Endpoint = serde_json::from_value(serde_json::json!(
            { "static": { "body": r#"{"tier":"gold"}"#, "raw": true } }
        ))
        .unwrap();
        let caller = Caller::builder(&unique("caller_built"))
            .with_lookup(crate::models::LookupMiddleware::new(tier).with_into("customer"))
            .with_parallel(4)
            .with_batch_size(16)
            .deploy()
            .await
            .unwrap();
        let answers: Vec<anyhow::Result<serde_json::Value>> =
            caller.call_many([[("id", 7)], [("id", 8)]]).await;
        for (answer, id) in answers.into_iter().zip([7, 8]) {
            assert_eq!(
                answer.unwrap(),
                serde_json::json!({"id": id, "customer": {"tier": "gold"}})
            );
        }
        caller.stop().await;
    }

    #[tokio::test]
    async fn a_built_caller_times_out_after_its_request_timeout() {
        let caller = Caller::builder(&unique("caller_timeout"))
            .with_request_timeout_ms(50)
            .with_capacity(8)
            .with_handler(|msg: CanonicalMessage| async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(Handled::Publish(msg))
            })
            .deploy()
            .await
            .unwrap();
        let started = std::time::Instant::now();
        assert!(caller.call_message("slow".into()).await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        caller.stop().await;
    }

    #[tokio::test]
    async fn calls_into_an_ordered_sink_with_output_parallel_are_acked() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique("caller_file");
        let out = dir.path().join("out.jsonl");
        let path = config_file(
            &dir,
            serde_json::json!({ &name: {
                "concurrency": 4,
                "input": { "memory": { "topic": unique("caller_file_in") } },
                "output": {
                    "file": { "path": out.to_string_lossy() },
                    "middlewares": [
                        { "lookup": {
                            "from": { "static": { "body": r#"{"tier":"gold"}"#, "raw": true } },
                            "into": "customer"
                        } },
                        { "parallel": {} }
                    ]
                }
            }}),
        );
        let caller = Caller::from_file(&path, &name).await.unwrap();
        let calls: Vec<_> = (0..50)
            .map(|i| {
                let caller = caller.clone();
                tokio::spawn(async move { caller.send(format!(r#"{{"id":{i}}}"#).into()).await })
            })
            .collect();
        for call in calls {
            assert!(matches!(call.await.unwrap().unwrap(), Sent::Ack));
        }
        caller.stop().await;
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(written.lines().count(), 50);
        assert!(written.lines().all(|l| l.contains("gold")), "{written}");
    }

    #[tokio::test]
    async fn a_route_without_input_and_output_answers_with_typed_fields() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Customer {
            tier: String,
        }
        let route = r#"{
            "batch_size": 16,
            "input": { "middlewares": [ { "lookup": {
                "from": { "static": { "body": "{\"tier\":\"${payload:tier}\"}", "raw": true } },
                "into": "$"
            } } ] }
        }"#;
        let caller = Caller::from_str(route, &unique("caller_defaults"))
            .await
            .unwrap();

        let gold: Customer = caller.call([("tier", "gold")]).await.unwrap();
        assert_eq!(gold.tier, "gold");
        let mixed: serde_json::Value = caller
            .call(serde_json::json!({"tier": "silver", "id": 7}))
            .await
            .unwrap();
        assert_eq!(mixed, serde_json::json!({"tier": "silver"}));

        let many: Vec<anyhow::Result<Customer>> =
            caller.call_many([[("tier", "a")], [("tier", "b")]]).await;
        let tiers: Vec<String> = many.into_iter().map(|c| c.unwrap().tier).collect();
        assert_eq!(tiers, ["a", "b"]);

        let wrong = caller.call::<u64>([("tier", "x")]).await.unwrap_err();
        assert!(wrong.to_string().contains("does not parse"), "{wrong}");
        caller.stop().await;
    }

    #[test]
    fn caller_defaults_apply_only_where_nothing_is_given() {
        let route = |body: serde_json::Value| {
            let route = Caller::route_from_config(body, "c").unwrap();
            (route.input.endpoint_type, route.output.endpoint_type)
        };
        let (input, output) = route(serde_json::json!({ "batch_size": 4 }));
        assert!(matches!(input, EndpointType::Memory(m) if m.topic == "mqb-caller-c"));
        assert!(matches!(output, EndpointType::Response(_)));

        for empty in [
            serde_json::json!({}),
            serde_json::Value::Null,
            serde_json::json!({ "c": null }),
            serde_json::json!({ "routes": { "c": { "input": null } } }),
        ] {
            let (input, output) = route(empty);
            assert!(matches!(input, EndpointType::Memory(_)));
            assert!(matches!(output, EndpointType::Response(_)));
        }

        let (_, output) = route(serde_json::json!({ "c": { "output": "null" } }));
        assert!(matches!(output, EndpointType::Null));

        let (input, _) = route(serde_json::json!({
            "routes": { "c": { "input": { "memory": { "topic": "own" } } } }
        }));
        assert!(matches!(input, EndpointType::Memory(m) if m.topic == "own"));

        let strict = Route::from_config(serde_json::json!({ "c": { "batch_size": 4 } }), "c");
        assert!(strict.is_err(), "Route keeps requiring an input");
    }

    #[tokio::test]
    async fn only_an_in_process_memory_input_is_accepted() {
        let route = Route::new(Endpoint::new(EndpointType::Null), Endpoint::new_response());
        let err = Caller::deploy(&unique("caller_bad"), route)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("`memory` endpoint"), "{err}");
    }

    #[tokio::test]
    async fn the_input_holds_many_calls_unless_capacity_is_set() {
        let roomy = Caller::new(&unique("caller_cap"), Endpoint::new_response())
            .await
            .unwrap();
        assert_eq!(
            roomy.requests.status().await.capacity,
            Some(CALLER_CAPACITY)
        );

        let input = Endpoint::new_memory(&unique("caller_cap_in"), 8);
        let route = Route::new(input, Endpoint::new_response());
        let own = Caller::deploy(&unique("caller_cap_own"), route)
            .await
            .unwrap();
        assert_eq!(own.requests.status().await.capacity, Some(8));
        roomy.stop().await;
        own.stop().await;
    }
}
