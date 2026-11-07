//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Call a route like a function: `caller.call(message).await` returns the route's answer.
//!
//! A [`Caller`] is a normal deployed [`Route`] whose input is an in-process `memory`
//! topic. Each call puts one request on that topic and waits for its reply, so everything
//! a route does applies unchanged: input and output middlewares (`lookup`, `parallel`,
//! `retry`, plugin middlewares), the handler, `batch_size`, `concurrency`, ordered sinks
//! and graceful shutdown. Calls made at the same time are read as one batch, so a batched
//! `lookup` queries once for all of them.
//!
//! ```no_run
//! use mq_bridge::models::Endpoint;
//! use mq_bridge::{CanonicalMessage, Caller, Route};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // The route from `routes.yaml`; its input is `memory: { topic: enrich }` and its
//! // output ends with `response: {}`.
//! let enrich = Caller::from_file("routes.yaml", "enrich").await?;
//! let answer = enrich.call(CanonicalMessage::from(r#"{"id": 7}"#)).await?;
//!
//! // Or built in code: the caller adds the memory input itself.
//! let echo = Caller::new("echo", Endpoint::new_response()).await?;
//! assert_eq!(echo.call("hi".into()).await?.get_payload_str(), "hi");
//! # Ok(())
//! # }
//! ```
//!
//! How a call ends:
//! - The output answers (`response: {}`, `http`, `request`, an answering plugin endpoint):
//!   [`Caller::call`] returns that message.
//! - The output only accepts the message (a plain sink): [`Caller::send`] returns
//!   [`Sent::Ack`]; [`Caller::call`] returns an error.
//! - The message fails: both return the error, as soon as the route gives up on it
//!   (after its `retry` middleware, if any). The route keeps running.
//! - No answer within the input's `request_timeout_ms` (default 30 s): a timeout error.

use crate::endpoints::memory::MemoryPublisher;
use crate::models::{Endpoint, EndpointType, MemoryConfig, Route};
use crate::traits::MessagePublisher;
use crate::{CanonicalMessage, Sent};
use anyhow::{anyhow, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

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
    /// answered with its error instead of stopping the route. The caller is also
    /// registered under `name` for [`Caller::get`].
    pub async fn deploy(name: &str, mut route: Route) -> anyhow::Result<Self> {
        let EndpointType::Memory(input) = &mut route.input.endpoint_type else {
            bail!("Caller '{name}': the route's input must be a `memory` endpoint");
        };
        if input.subscribe_mode {
            bail!("Caller '{name}': the memory input cannot use `subscribe_mode`");
        }
        input.request_reply = true;
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
    /// Use [`Caller::deploy`] for a handler, input middlewares or route options.
    pub async fn new(name: &str, output: Endpoint) -> anyhow::Result<Self> {
        let input = Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: format!("mqb-caller-{name}"),
            ..Default::default()
        }));
        Self::deploy(name, Route::new(input, output)).await
    }

    /// Deploys the route `name` from the `routes:` section of a config file.
    pub async fn from_file(path: impl AsRef<Path>, name: &str) -> anyhow::Result<Self> {
        Self::deploy(name, Route::from_file(path, name)?).await
    }

    /// Sends `message` through the route and returns its answer.
    ///
    /// Fails if the message fails, times out, or the output accepts it without an answer.
    pub async fn call(&self, message: CanonicalMessage) -> anyhow::Result<CanonicalMessage> {
        match self.send(message).await? {
            Sent::Response(response) => Ok(response),
            Sent::Ack => Err(self.no_answer()),
        }
    }

    /// Sends `message` through the route and returns how it ended: [`Sent::Response`]
    /// with the answer, or [`Sent::Ack`] when the output only accepted it.
    pub async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Sent> {
        self.requests
            .send(message)
            .await
            .map_err(|e| anyhow!("Caller '{}': {e}", self.name))
    }

    /// Like [`Caller::call`] for several messages: they reach the route as one batch, and
    /// the answers come back in their order, each failing or succeeding on its own.
    pub async fn call_many(
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
            .map(|sent| sent.map_err(|e| anyhow!("Caller '{}': {e}", self.name)))
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

        let answer = caller.call("a".into()).await.unwrap();
        assert_eq!(answer.get_payload_str(), "enriched a");
        let started = std::time::Instant::now();
        let err = caller.call("bad".into()).await.unwrap_err();
        assert!(err.to_string().contains("cannot enrich"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let again = Caller::get(&name).unwrap().call("b".into()).await.unwrap();
        assert_eq!(again.get_payload_str(), "enriched b");

        let many = caller
            .call_many(vec!["c".into(), "bad".into(), "d".into()])
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
        let err = caller.call("x".into()).await.unwrap_err();
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
                tokio::spawn(async move { caller.call(format!("{i}").into()).await })
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
            .call_many((0..50).map(|i| format!("m{i}").into()).collect())
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
        let answer = caller.call(r#"{"id":7}"#.into()).await.unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&answer.payload).unwrap();
        assert_eq!(
            answer,
            serde_json::json!({"id": 7, "customer": {"tier": "gold"}})
        );
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
    async fn only_an_in_process_memory_input_is_accepted() {
        let route = Route::new(Endpoint::new(EndpointType::Null), Endpoint::new_response());
        let err = Caller::deploy(&unique("caller_bad"), route)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("`memory` endpoint"), "{err}");
    }
}
