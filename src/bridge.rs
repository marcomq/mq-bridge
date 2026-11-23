//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Named instances for applications that embed mq-bridge.
//!
//! A [`Bridge`] gives routes, publishers, callers, `ref` endpoints and in-process
//! `memory` and `stream_buffer` topics their own namespace, so two parts of an
//! application can use the same names. The top-level API stays the default,
//! unnamespaced instance.
//!
//! An optional [`PolicyHook`] approves every endpoint before it is used, and an optional
//! [`AuditSink`] receives what the instance did.
//!
//! ```
//! use mq_bridge::{bridge::Bridge, models::Endpoint, Route};
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let first = Bridge::new("first").unwrap();
//! let second = Bridge::new("second").unwrap();
//! let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
//!
//! first.deploy("orders", &route).await.unwrap();
//! second.deploy("orders", &route).await.unwrap();
//! assert_eq!(first.routes(), ["orders"]);
//!
//! first.stop_all().await;
//! second.stop_all().await;
//! # });
//! ```
//!
//! This separates names, it is not a security boundary. Shared HTTP and gRPC servers,
//! plugins, custom factories, dedup, cookie and event stores stay process-wide, and code
//! in the same process can still reach a namespaced entry through the top-level API.
//! `ipc://`, `unix://` and `pipe://` topics are real transports and keep their address.
//! A `custom` endpoint's config is opaque, so names inside it are not rewritten.

use crate::endpoints::memory::{get_or_create_channel, transport::TransportUrl, MemoryChannel};
use crate::models::{Endpoint, EndpointType, MemoryConfig, Middleware};
use crate::route::RouteOutcome;
use crate::traits::EndpointStatus;
use crate::{Caller, Publisher, Route};
use anyhow::{anyhow, bail};
use std::collections::HashSet;
use std::sync::Arc;

const SEPARATOR: &str = "::";

/// Approves or rejects endpoints before a [`Bridge`] uses them.
pub trait PolicyHook: Send + Sync {
    /// Called for every endpoint in a configuration, including nested ones such as
    /// `fanout` legs and `dlq` targets. An `Err` stops the operation with that reason.
    fn check(&self, request: &PolicyRequest<'_>) -> Result<(), String>;
}

/// One endpoint a [`Bridge`] is about to use.
#[derive(Debug)]
#[non_exhaustive]
pub struct PolicyRequest<'a> {
    pub namespace: &'a str,
    /// The route, publisher, caller or `ref` name the endpoint belongs to.
    pub name: &'a str,
    /// The endpoint as it will run: `ref` names and in-process topics read `namespace::name`.
    pub endpoint: &'a Endpoint,
}

/// Receives what a [`Bridge`] did. Runs inline, so it must return quickly.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: &AuditEvent);
}

/// One recorded action of a [`Bridge`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditEvent {
    pub namespace: String,
    pub name: String,
    pub action: AuditAction,
    /// The policy's reason or the error, for `Rejected` and `DeployFailed`.
    pub detail: Option<String>,
}

/// What an [`AuditEvent`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditAction {
    Rejected,
    RouteDeployed,
    DeployFailed,
    RouteStopped,
    CallerDeployed,
    EndpointRegistered,
    EndpointRemoved,
    PublisherRegistered,
    PublisherRemoved,
}

/// A named instance. Cheap to clone; clones and instances with the same namespace
/// share their routes, publishers and topics.
#[derive(Clone)]
pub struct Bridge {
    namespace: Arc<str>,
    prefix: Arc<str>,
    policy: Option<Arc<dyn PolicyHook>>,
    audit: Option<Arc<dyn AuditSink>>,
}

impl Bridge {
    /// Creates an instance. The namespace may use ASCII letters, digits, `-` and `_`.
    pub fn new(namespace: &str) -> anyhow::Result<Self> {
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if namespace.is_empty() || !namespace.chars().all(valid) {
            bail!("Invalid bridge namespace '{namespace}': use ASCII letters, digits, '-' or '_'");
        }
        Ok(Self {
            namespace: namespace.into(),
            prefix: format!("{namespace}{SEPARATOR}").into(),
            policy: None,
            audit: None,
        })
    }

    /// Checks every endpoint this handle uses with `policy`.
    pub fn with_policy(mut self, policy: Arc<dyn PolicyHook>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Reports this handle's actions to `audit`.
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Deploys `route` under `name`, replacing a route of that name in this instance.
    pub async fn deploy(&self, name: &str, route: &Route) -> anyhow::Result<()> {
        self.authorize(name, [&route.input, &route.output])?;
        let result = self.scoped_route(route).deploy(&self.key(name)).await;
        self.record_result(name, AuditAction::RouteDeployed, &result);
        result
    }

    /// Stops a route of this instance. Returns false if it was not running.
    pub async fn stop(&self, name: &str) -> bool {
        let key = self.key(name);
        let stopped = match Caller::get(&key) {
            Some(caller) => caller.stop().await,
            None => Route::stop(&key).await,
        };
        if stopped {
            self.record(name, AuditAction::RouteStopped, None);
        }
        stopped
    }

    /// Stops every route of this instance and returns their names.
    pub async fn stop_all(&self) -> Vec<String> {
        let names = self.routes();
        futures::future::join_all(names.iter().map(|name| self.stop(name))).await;
        names
    }

    /// The names of this instance's deployed routes.
    pub fn routes(&self) -> Vec<String> {
        self.own_names(crate::route::list_routes())
    }

    /// The deployed route, with its `ref` names and topics in their namespaced form.
    pub fn route(&self, name: &str) -> Option<Route> {
        crate::route::get_route(&self.key(name))
    }

    pub fn route_status(&self, name: &str) -> Option<EndpointStatus> {
        crate::route::route_status(&self.key(name))
    }

    pub fn route_outcome(&self, name: &str) -> Option<RouteOutcome> {
        crate::route::route_outcome(&self.key(name))
    }

    /// Registers `endpoint` for `ref: name` inside this instance.
    pub fn register_endpoint(&self, name: &str, endpoint: Endpoint) -> anyhow::Result<()> {
        self.authorize(name, [&endpoint])?;
        crate::route::register_endpoint(&self.key(name), self.scoped_endpoint(endpoint));
        self.record(name, AuditAction::EndpointRegistered, None);
        Ok(())
    }

    pub fn get_endpoint(&self, name: &str) -> Option<Endpoint> {
        crate::route::get_endpoint(&self.key(name))
    }

    pub fn unregister_endpoint(&self, name: &str) -> Option<Endpoint> {
        let removed = crate::route::unregister_endpoint(&self.key(name));
        if removed.is_some() {
            self.record(name, AuditAction::EndpointRemoved, None);
        }
        removed
    }

    /// Creates a publisher for `endpoint` and registers it under `name` in this instance.
    pub async fn create_publisher(
        &self,
        name: &str,
        endpoint: Endpoint,
    ) -> anyhow::Result<Publisher> {
        self.authorize(name, [&endpoint])?;
        let publisher = Publisher::new(self.scoped_endpoint(endpoint)).await?;
        publisher.register(&self.key(name));
        self.record(name, AuditAction::PublisherRegistered, None);
        Ok(publisher)
    }

    pub fn get_publisher(&self, name: &str) -> Option<Publisher> {
        Publisher::get(&self.key(name))
    }

    pub fn unregister_publisher(&self, name: &str) -> Option<Publisher> {
        let removed = Publisher::unregister(&self.key(name));
        if removed.is_some() {
            self.record(name, AuditAction::PublisherRemoved, None);
        }
        removed
    }

    /// The names of this instance's registered publishers.
    pub fn publishers(&self) -> Vec<String> {
        self.own_names(crate::publisher::list_publishers())
    }

    /// Deploys `route` as a [`Caller`] under `name` in this instance.
    pub async fn deploy_caller(&self, name: &str, route: Route) -> anyhow::Result<Caller> {
        let key = self.key(name);
        let route = match Caller::prepare(&key, route) {
            Ok(route) => route,
            Err(error) => {
                self.record(name, AuditAction::DeployFailed, Some(format!("{error:#}")));
                return Err(error);
            }
        };
        self.authorize(name, [&route.input, &route.output])?;
        let result = Caller::deploy(&key, self.scoped_route(&route)).await;
        match &result {
            Ok(_) => self.record(name, AuditAction::CallerDeployed, None),
            Err(error) => self.record(name, AuditAction::DeployFailed, Some(format!("{error:#}"))),
        }
        result
    }

    pub fn get_caller(&self, name: &str) -> Option<Caller> {
        Caller::get(&self.key(name))
    }

    /// The in-process channel of `config`'s topic in this instance.
    pub fn channel(&self, config: &MemoryConfig) -> MemoryChannel {
        let mut config = config.clone();
        self.scope_memory(&mut config);
        get_or_create_channel(&config)
    }

    fn key(&self, name: &str) -> String {
        if name.starts_with(&*self.prefix) {
            name.to_string()
        } else {
            format!("{}{name}", self.prefix)
        }
    }

    fn own_names(&self, names: Vec<String>) -> Vec<String> {
        names
            .into_iter()
            .filter_map(|name| name.strip_prefix(&*self.prefix).map(str::to_string))
            .collect()
    }

    fn scoped_route(&self, route: &Route) -> Route {
        let mut route = route.clone();
        route.input = self.scoped_endpoint(route.input);
        route.output = self.scoped_endpoint(route.output);
        route
    }

    fn scoped_endpoint(&self, mut endpoint: Endpoint) -> Endpoint {
        for_each_endpoint(
            &mut endpoint,
            &mut |endpoint| match &mut endpoint.endpoint_type {
                EndpointType::Ref(name) => *name = self.key(name),
                EndpointType::Memory(config) => self.scope_memory(config),
                EndpointType::StreamBuffer(config) => config.topic = self.key(&config.topic),
                _ => {}
            },
        );
        endpoint
    }

    fn scope_memory(&self, config: &mut MemoryConfig) {
        let Ok(identifier) = config.get_transport_identifier() else {
            return;
        };
        if let Ok(TransportUrl::Memory { namespace }) = TransportUrl::parse(&identifier) {
            config.topic = self.key(&namespace);
            config.url = None;
        }
    }

    fn authorize<'a>(
        &self,
        name: &str,
        endpoints: impl IntoIterator<Item = &'a Endpoint>,
    ) -> anyhow::Result<()> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        let mut followed = HashSet::new();
        let verdict = endpoints.into_iter().try_for_each(|endpoint| {
            let mut endpoint = self.scoped_endpoint(endpoint.clone());
            self.check_policy(policy.as_ref(), name, &mut endpoint, &mut followed)
        });
        verdict.map_err(|reason| {
            self.record(name, AuditAction::Rejected, Some(reason.clone()));
            anyhow!(
                "[bridge:{}] policy rejected '{name}': {reason}",
                self.namespace
            )
        })
    }

    /// Checks `endpoint` and everything nested in it, following each `ref` to its target.
    fn check_policy(
        &self,
        policy: &dyn PolicyHook,
        name: &str,
        endpoint: &mut Endpoint,
        followed: &mut HashSet<String>,
    ) -> Result<(), String> {
        let mut verdict = Ok(());
        let mut refs = Vec::new();
        for_each_endpoint(endpoint, &mut |endpoint| {
            if verdict.is_ok() {
                verdict = policy.check(&PolicyRequest {
                    namespace: &self.namespace,
                    name,
                    endpoint,
                });
            }
            if let EndpointType::Ref(target) = &endpoint.endpoint_type {
                refs.push(target.clone());
            }
        });
        verdict?;
        for target in refs {
            if followed.insert(target.clone()) {
                if let Some(mut resolved) = crate::route::get_endpoint(&target) {
                    self.check_policy(policy, name, &mut resolved, followed)?;
                }
            }
        }
        Ok(())
    }

    fn record_result(&self, name: &str, success: AuditAction, result: &anyhow::Result<()>) {
        match result {
            Ok(()) => self.record(name, success, None),
            Err(error) => self.record(name, AuditAction::DeployFailed, Some(format!("{error:#}"))),
        }
    }

    fn record(&self, name: &str, action: AuditAction, detail: Option<String>) {
        if let Some(audit) = &self.audit {
            audit.record(&AuditEvent {
                namespace: self.namespace.to_string(),
                name: name.to_string(),
                action,
                detail,
            });
        }
    }
}

/// Calls `f` on `endpoint` and every endpoint nested in its config or middlewares.
fn for_each_endpoint(endpoint: &mut Endpoint, f: &mut dyn FnMut(&mut Endpoint)) {
    f(endpoint);
    for middleware in &mut endpoint.middlewares {
        match middleware {
            Middleware::Dlq(config) => for_each_endpoint(&mut config.endpoint, f),
            Middleware::Lookup(config) => {
                if let Some(from) = &mut config.from {
                    for_each_endpoint(from, f);
                }
                for entry in &mut config.entries {
                    for_each_endpoint(&mut entry.from, f);
                }
            }
            _ => {}
        }
    }
    let mut visit_optional = |nested: &mut Option<Box<Endpoint>>| {
        if let Some(nested) = nested {
            for_each_endpoint(nested, f);
        }
    };
    match &mut endpoint.endpoint_type {
        EndpointType::Http(config) => visit_optional(&mut config.stream_response_to),
        EndpointType::HttpBulk(config) => {
            if let Some(query) = &mut config.query {
                visit_optional(&mut query.stream_to);
                visit_optional(&mut query.cancel_from);
            }
        }
        EndpointType::OpenAiChat(config) => {
            visit_optional(&mut config.stream_to);
            visit_optional(&mut config.cancel_from);
        }
        EndpointType::Fanout(outputs) => outputs.iter_mut().for_each(|o| for_each_endpoint(o, f)),
        EndpointType::Sequence(config) => config
            .endpoints
            .iter_mut()
            .for_each(|o| for_each_endpoint(o, f)),
        EndpointType::Switch(config) => {
            config
                .cases
                .values_mut()
                .for_each(|o| for_each_endpoint(o, f));
            config
                .when
                .iter_mut()
                .for_each(|case| for_each_endpoint(&mut case.to, f));
            if let Some(default) = config.default.as_deref_mut() {
                for_each_endpoint(default, f);
            }
        }
        EndpointType::Reader(inner) => for_each_endpoint(inner, f),
        EndpointType::Request(config) => {
            for_each_endpoint(&mut config.to, f);
            for_each_endpoint(&mut config.forward_to, f);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Duration;

    fn unique(label: &str) -> String {
        format!("{label}-{}", fast_uuid_v7::gen_id_string())
    }

    fn bridge(label: &str) -> Bridge {
        Bridge::new(&unique(label)).unwrap()
    }

    fn memory_route() -> Route {
        Route::new(
            Endpoint::new_memory("in", 10),
            Endpoint::new_memory("out", 10),
        )
    }

    fn nested() -> Endpoint {
        serde_json::from_value(json!({
            "middlewares": [
                { "dlq": { "endpoint": { "memory": { "topic": "dead" } } } },
                { "lookup": {
                    "from": { "ref": "lookup-one" },
                    "into": "a",
                    "entries": []
                } }
            ],
            "fanout": [
                { "ref": "leg" },
                { "switch": {
                    "metadata_key": "kind",
                    "cases": { "x": { "memory": { "topic": "case" } } },
                    "default": { "memory": { "topic": "memory://fallback" } }
                } },
                { "reader": { "memory": { "topic": "read" } } },
                { "request": {
                    "to": { "stream_buffer": { "topic": "buffer" } },
                    "forward_to": { "memory": { "topic": "ipc://shared" } }
                } }
            ]
        }))
        .unwrap()
    }

    fn names(endpoint: &Endpoint) -> Vec<String> {
        let mut endpoint = endpoint.clone();
        let mut found = Vec::new();
        for_each_endpoint(
            &mut endpoint,
            &mut |endpoint| match &endpoint.endpoint_type {
                EndpointType::Ref(name) => found.push(name.clone()),
                EndpointType::Memory(config) => found.push(config.topic.clone()),
                EndpointType::StreamBuffer(config) => found.push(config.topic.clone()),
                _ => {}
            },
        );
        found.sort();
        found
    }

    #[tokio::test]
    async fn a_ref_is_checked_by_what_it_points_to() {
        let open = bridge("ref-policy");
        open.register_endpoint("secret", Endpoint::new_memory("secret", 10))
            .unwrap();
        open.register_endpoint("public", Endpoint::new_memory("public", 10))
            .unwrap();
        let guarded = open.with_policy(Arc::new(DenyTopic("secret")));
        let to =
            |name: &str| -> Endpoint { serde_json::from_value(json!({ "ref": name })).unwrap() };

        let denied = Route::new(Endpoint::new_memory("in", 10), to("secret"));
        assert!(guarded.deploy("denied", &denied).await.is_err());
        let allowed = Route::new(Endpoint::new_memory("in", 10), to("public"));
        guarded.deploy("allowed", &allowed).await.unwrap();
        guarded.stop_all().await;
    }

    #[tokio::test]
    async fn a_route_that_replaces_a_caller_ends_its_registration() {
        let instance = bridge("replaced-caller");
        let input = Endpoint::new_memory("calls", 10);
        instance
            .deploy_caller("answers", Route::new(input, Endpoint::new_response()))
            .await
            .unwrap();
        assert!(instance.get_caller("answers").is_some());

        instance.deploy("answers", &memory_route()).await.unwrap();

        assert!(instance.get_caller("answers").is_none());
        instance.stop_all().await;
    }

    #[tokio::test]
    async fn the_same_names_stay_apart_between_instances() {
        let (first, second) = (bridge("first"), bridge("second"));
        first.deploy("orders", &memory_route()).await.unwrap();
        second.deploy("orders", &memory_route()).await.unwrap();

        let config = |topic| MemoryConfig::new(topic, Some(10));
        first
            .channel(&config("in"))
            .send_message(CanonicalMessage::from("for first"))
            .await
            .unwrap();
        let received = tokio::time::timeout(
            Duration::from_secs(5),
            first.channel(&config("out")).receiver.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received[0].get_payload_str(), "for first");
        assert!(second.channel(&config("out")).is_empty());

        assert!(first.stop("orders").await);
        assert_eq!(second.routes(), ["orders"]);
        assert_eq!(second.stop_all().await, ["orders"]);
    }

    #[test]
    fn every_nested_name_is_scoped_except_real_transports() {
        let bridge = Bridge::new("tenant").unwrap();
        let scoped = bridge.scoped_endpoint(nested());
        assert_eq!(
            names(&scoped),
            [
                "ipc://shared",
                "tenant::buffer",
                "tenant::case",
                "tenant::dead",
                "tenant::fallback",
                "tenant::leg",
                "tenant::lookup-one",
                "tenant::read",
            ]
        );
        let again = bridge.scoped_endpoint(scoped.clone());
        assert_eq!(names(&again), names(&scoped));
    }

    #[test]
    fn a_name_cannot_reach_into_another_namespace() {
        let scoped = Bridge::new("tenant")
            .unwrap()
            .scoped_endpoint(Endpoint::new(EndpointType::Ref("other::secret".into())));
        assert_eq!(names(&scoped), ["tenant::other::secret"]);
    }

    #[test]
    fn a_namespace_with_a_separator_is_rejected() {
        for invalid in ["", "a::b", "a b", "a/b"] {
            assert!(Bridge::new(invalid).is_err(), "{invalid:?}");
        }
    }

    struct DenyTopic(&'static str);
    impl PolicyHook for DenyTopic {
        fn check(&self, request: &PolicyRequest<'_>) -> Result<(), String> {
            match &request.endpoint.endpoint_type {
                EndpointType::Memory(config)
                    if config.topic == format!("{}::{}", request.namespace, self.0) =>
                {
                    Err(format!("{} is not allowed", self.0))
                }
                _ => Ok(()),
            }
        }
    }

    #[derive(Default)]
    struct Recorded(Mutex<Vec<AuditEvent>>);
    impl AuditSink for Recorded {
        fn record(&self, event: &AuditEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    impl Recorded {
        fn actions(&self) -> Vec<AuditAction> {
            self.0.lock().unwrap().iter().map(|e| e.action).collect()
        }
    }

    #[tokio::test]
    async fn the_policy_rejects_a_nested_endpoint_before_anything_starts() {
        let audit = Arc::new(Recorded::default());
        let bridge = bridge("policy")
            .with_policy(Arc::new(DenyTopic("dead")))
            .with_audit(audit.clone());
        let route = Route::new(Endpoint::new_memory("in", 10), nested());

        let error = bridge.deploy("denied", &route).await.unwrap_err();

        assert!(format!("{error}").contains("dead is not allowed"));
        assert!(bridge.routes().is_empty());
        assert_eq!(audit.actions(), [AuditAction::Rejected]);
    }

    #[tokio::test]
    async fn registering_an_endpoint_also_passes_the_policy() {
        let bridge = bridge("register").with_policy(Arc::new(DenyTopic("blocked")));
        assert!(bridge
            .register_endpoint("target", Endpoint::new_memory("blocked", 10))
            .is_err());
        assert!(bridge.get_endpoint("target").is_none());
    }

    #[test]
    fn another_spelling_of_a_topic_does_not_pass_the_policy() {
        let bridge = bridge("spelling").with_policy(Arc::new(DenyTopic("blocked")));
        let qualified = format!("{}::blocked", bridge.namespace());
        for topic in ["blocked", "memory://blocked", qualified.as_str()] {
            assert!(
                bridge
                    .register_endpoint("target", Endpoint::new_memory(topic, 10))
                    .is_err(),
                "{topic}"
            );
        }
    }

    #[tokio::test]
    async fn the_audit_sink_sees_the_lifecycle() {
        let audit = Arc::new(Recorded::default());
        let bridge = bridge("audit").with_audit(audit.clone());
        bridge
            .register_endpoint("target", Endpoint::new_memory("t", 10))
            .unwrap();
        bridge.deploy("route", &memory_route()).await.unwrap();
        bridge.stop("route").await;
        bridge.unregister_endpoint("target");

        assert_eq!(
            audit.actions(),
            [
                AuditAction::EndpointRegistered,
                AuditAction::RouteDeployed,
                AuditAction::RouteStopped,
                AuditAction::EndpointRemoved,
            ]
        );
    }

    #[tokio::test]
    async fn publishers_are_per_instance() {
        let (first, second) = (bridge("pub-a"), bridge("pub-b"));
        first
            .create_publisher("out", Endpoint::new_memory("t", 10))
            .await
            .unwrap();
        assert!(first.get_publisher("out").is_some());
        assert!(second.get_publisher("out").is_none());
        assert_eq!(first.publishers(), ["out"]);
        assert!(first.unregister_publisher("out").is_some());
    }

    #[tokio::test]
    async fn a_caller_answers_inside_its_instance() {
        let (first, second) = (bridge("call-a"), bridge("call-b"));
        let route = Route::new(Endpoint::new_memory("calls", 10), Endpoint::new_response());
        let caller = first.deploy_caller("echo", route).await.unwrap();

        let answer: serde_json::Value = caller.call(json!({ "n": 1 })).await.unwrap();

        assert_eq!(answer, json!({ "n": 1 }));
        assert!(first.get_caller("echo").is_some());
        assert!(second.get_caller("echo").is_none());
        assert_eq!(first.stop_all().await, ["echo"]);
        assert!(first.get_caller("echo").is_none());
    }
}
