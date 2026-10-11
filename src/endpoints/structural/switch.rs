use crate::traits::{BoxFuture, EndpointStatus, MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::warn;

/// How a `switch` picks a destination.
///
/// The two modes differ in cost, which is why both exist: a lookup is a HashMap
/// get on metadata, while a predicate may have to parse the payload.
enum Routing {
    /// Exact match on a metadata value.
    Lookup {
        metadata_key: String,
        cases: HashMap<String, Arc<dyn MessagePublisher>>,
    },
    /// Ordered expression predicates, first match wins.
    #[cfg(feature = "filter")]
    Predicate(
        Vec<(
            crate::middleware::filter::CompiledFilter,
            Arc<dyn MessagePublisher>,
        )>,
    ),
}

pub struct SwitchPublisher {
    routing: Routing,
    default: Option<Arc<dyn MessagePublisher>>,
    /// Messages that matched no case while no `default` was set.
    unmatched: AtomicU64,
}

impl SwitchPublisher {
    pub fn new(
        metadata_key: String,
        cases: HashMap<String, Arc<dyn MessagePublisher>>,
        default: Option<Arc<dyn MessagePublisher>>,
    ) -> Self {
        Self {
            routing: Routing::Lookup {
                metadata_key,
                cases,
            },
            default,
            unmatched: AtomicU64::new(0),
        }
    }

    /// Builds the predicate mode from already-compiled expressions, in order.
    #[cfg(feature = "filter")]
    pub(crate) fn new_predicate(
        cases: Vec<(
            crate::middleware::filter::CompiledFilter,
            Arc<dyn MessagePublisher>,
        )>,
        default: Option<Arc<dyn MessagePublisher>>,
    ) -> Self {
        Self {
            routing: Routing::Predicate(cases),
            default,
            unmatched: AtomicU64::new(0),
        }
    }

    /// Resolves the destination, or `None` to drop the message.
    ///
    /// Only predicate mode can fail: an expression pointed at a payload it
    /// cannot read is a configuration error, and silently dropping every
    /// message would hide it.
    fn get_publisher(
        &self,
        message: &CanonicalMessage,
    ) -> Result<Option<&Arc<dyn MessagePublisher>>, PublisherError> {
        match &self.routing {
            Routing::Lookup {
                metadata_key,
                cases,
            } => {
                if let Some(val) = message.metadata.get(metadata_key) {
                    if let Some(publisher) = cases.get(val) {
                        return Ok(Some(publisher));
                    }
                }
            }
            #[cfg(feature = "filter")]
            Routing::Predicate(cases) => {
                let mut context = crate::middleware::filter::FilterContext::new();
                for (filter, publisher) in cases {
                    if filter
                        .matches_with_context(message, &mut context)
                        .map_err(PublisherError::NonRetryable)?
                    {
                        return Ok(Some(publisher));
                    }
                }
            }
        }
        Ok(self.default.as_ref())
    }

    /// Names the routing rule in the drop warning.
    fn dropped_reason(&self) -> String {
        match &self.routing {
            Routing::Lookup { metadata_key, .. } => {
                format!("metadata key '{metadata_key}' not found or no matching case/default")
            }
            #[cfg(feature = "filter")]
            Routing::Predicate(_) => "no `when` case matched and no default is set".to_string(),
        }
    }

    /// Every destination this switch can reach, cases and `default` alike.
    fn destinations(&self) -> impl Iterator<Item = &Arc<dyn MessagePublisher>> {
        let routed: Box<dyn Iterator<Item = &Arc<dyn MessagePublisher>> + Send + '_> =
            match &self.routing {
                Routing::Lookup { cases, .. } => Box::new(cases.values()),
                #[cfg(feature = "filter")]
                Routing::Predicate(cases) => Box::new(cases.iter().map(|(_, publisher)| publisher)),
            };
        routed.chain(self.default.iter())
    }
}

#[async_trait]
impl MessagePublisher for SwitchPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            for publisher in self.destinations() {
                if let Some(hook) = publisher.on_connect_hook() {
                    hook.await?;
                }
            }
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            // Every destination gets torn down even if an earlier one fails, so one bad case
            // cannot skip another's durable teardown. The first error is reported.
            let mut first_error = None;
            for publisher in self.destinations() {
                if let Some(hook) = publisher.on_disconnect_hook() {
                    if let Err(e) = hook.await {
                        first_error.get_or_insert(e);
                    }
                }
            }
            first_error.map_or(Ok(()), Err)
        }))
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if let Some(publisher) = self.get_publisher(&message)? {
            publisher.send(message).await
        } else {
            self.unmatched.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Switch publisher dropped message with id {:032x}: {}.",
                message.message_id,
                self.dropped_reason()
            );
            Ok(Sent::Ack)
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        use futures::future::join_all;
        use std::collections::HashMap;

        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        // Group by the resolved publisher's identity, so every message headed for the same
        // destination lands in one `send_batch` call — including distinct metadata values
        // that both fall through to `default`.
        let mut grouped_messages: HashMap<
            usize,
            (Arc<dyn MessagePublisher>, Vec<CanonicalMessage>),
        > = HashMap::new();
        let mut dropped = 0usize;
        // A message whose predicate cannot read it fails on its own: failing the whole
        // batch would hand `retry`/`dlq` — or the drop report — every sibling message too.
        let mut failed_routing = Vec::new();

        for message in messages {
            match self.get_publisher(&message) {
                Ok(Some(publisher)) => {
                    let key = Arc::as_ptr(publisher) as *const () as usize;
                    grouped_messages
                        .entry(key)
                        .or_insert_with(|| (publisher.clone(), Vec::new()))
                        .1
                        .push(message);
                }
                Ok(None) => dropped += 1,
                Err(e) => failed_routing.push((message, e)),
            }
        }
        if dropped > 0 {
            self.unmatched.fetch_add(dropped as u64, Ordering::Relaxed);
            warn!(
                "Switch publisher dropped {dropped} messages: {}.",
                self.dropped_reason()
            );
        }

        // Create futures for sending each group as a batch.
        let batch_sends = grouped_messages
            .into_values()
            .map(|(publisher, batch)| async move { publisher.send_batch(batch).await });

        let results = join_all(batch_sends).await;

        // Aggregate results from all the batch sends.
        let mut all_responses = Vec::new();
        let mut all_failed = failed_routing;

        for result in results {
            match result {
                Ok(SentBatch::Ack) => {}
                Ok(SentBatch::Partial { responses, failed }) => {
                    if let Some(resps) = responses {
                        all_responses.extend(resps);
                    }
                    all_failed.extend(failed);
                }
                Err(e) => {
                    // If a whole sub-batch fails, we can't easily recover the messages that were part of it.
                    // Propagating the error is the safest and simplest option. The caller (e.g., retry middleware)
                    // will have to re-process the original, larger batch.
                    return Err(e);
                }
            }
        }

        if all_failed.is_empty() && all_responses.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: if all_responses.is_empty() {
                    None
                } else {
                    Some(all_responses)
                },
                failed: all_failed,
            })
        }
    }

    /// Unhealthy if any destination is, as for `fanout`. Each destination is listed with
    /// its case, the cases in a stable order and `default` last. `unmatched` counts the
    /// messages dropped for matching no case; it does not affect `healthy`.
    async fn status(&self) -> EndpointStatus {
        let mut labelled: Vec<(serde_json::Value, &Arc<dyn MessagePublisher>)> = match &self.routing
        {
            Routing::Lookup { cases, .. } => {
                let mut cases: Vec<_> = cases.iter().collect();
                cases.sort_unstable_by_key(|(key, _)| *key);
                cases
                    .into_iter()
                    .map(|(key, publisher)| (serde_json::json!({ "case": key }), publisher))
                    .collect()
            }
            #[cfg(feature = "filter")]
            Routing::Predicate(cases) => cases
                .iter()
                .enumerate()
                .map(|(index, (_, publisher))| {
                    (
                        serde_json::json!({ "case": format!("when[{index}]") }),
                        publisher,
                    )
                })
                .collect(),
        };
        if let Some(default) = &self.default {
            labelled.push((serde_json::json!({ "default": true }), default));
        }
        let results = futures::future::join_all(labelled.iter().map(|(_, p)| p.status())).await;
        let healthy = results.iter().all(|status| status.healthy);
        let error = results
            .iter()
            .find(|status| !status.healthy)
            .and_then(|status| status.error.clone());
        let destinations: Vec<_> = labelled
            .into_iter()
            .zip(&results)
            .map(|((mut entry, _), status)| {
                entry["status"] = serde_json::json!(status);
                entry
            })
            .collect();
        EndpointStatus {
            healthy,
            error,
            details: serde_json::json!({
                "destinations": destinations,
                "unmatched": self.unmatched.load(Ordering::Relaxed),
            }),
            ..Default::default()
        }
    }

    /// Ordered if any branch is: batches for a given branch must stay in source order.
    fn requires_ordered_publish(&self) -> bool {
        self.destinations().any(|p| p.requires_ordered_publish())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;
    use std::sync::atomic::AtomicUsize;

    struct Unhealthy;

    #[async_trait]
    impl MessagePublisher for Unhealthy {
        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }
        async fn status(&self) -> EndpointStatus {
            EndpointStatus {
                healthy: false,
                error: Some("down".to_string()),
                ..Default::default()
            }
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_switch_status_reports_an_unhealthy_destination() {
        let mut cases = HashMap::new();
        cases.insert(
            "A".to_string(),
            Arc::new(MemoryPublisher::new_local("switch_status_topic_a", 10))
                as Arc<dyn MessagePublisher>,
        );
        let healthy = SwitchPublisher::new("k".to_string(), cases.clone(), None);
        assert!(healthy.status().await.healthy);

        let switch = SwitchPublisher::new(
            "k".to_string(),
            cases,
            Some(Arc::new(Unhealthy) as Arc<dyn MessagePublisher>),
        );
        let status = switch.status().await;
        assert!(!status.healthy);
        assert_eq!(status.error.as_deref(), Some("down"));

        let destinations = status.details["destinations"].as_array().unwrap();
        assert_eq!(destinations[0]["case"], "A");
        assert_eq!(destinations[0]["status"]["healthy"], true);
        assert_eq!(destinations[1]["default"], true);
        assert_eq!(destinations[1]["status"]["healthy"], false);
    }

    #[tokio::test]
    async fn test_switch_status_lists_cases_in_key_order() {
        let mut cases = HashMap::new();
        for key in ["c", "a", "d", "b"] {
            cases.insert(
                key.to_string(),
                Arc::new(Unhealthy) as Arc<dyn MessagePublisher>,
            );
        }
        let status = SwitchPublisher::new("k".to_string(), cases, None)
            .status()
            .await;
        let keys: Vec<_> = status.details["destinations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["case"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(keys, ["a", "b", "c", "d"]);
    }

    /// Unmatched messages are counted in the status without making the switch unhealthy.
    #[tokio::test]
    async fn test_switch_status_counts_unmatched_messages() {
        let mut cases = HashMap::new();
        cases.insert(
            "A".to_string(),
            Arc::new(RecordingPublisher::default()) as Arc<dyn MessagePublisher>,
        );
        let switch = SwitchPublisher::new("k".to_string(), cases, None);
        assert_eq!(switch.status().await.details["unmatched"], 0);

        switch.send(CanonicalMessage::from("single")).await.unwrap();
        switch
            .send_batch(vec![
                CanonicalMessage::from("a").with_metadata_kv("k", "A"),
                CanonicalMessage::from("b").with_metadata_kv("k", "B"),
                CanonicalMessage::from("c"),
            ])
            .await
            .unwrap();

        let status = switch.status().await;
        assert_eq!(status.details["unmatched"], 3);
        assert!(status.healthy);
        assert!(status.error.is_none());
    }

    #[tokio::test]
    async fn test_switch_publisher_routing() {
        // Unique topic names: memory topics resolve through a process-global registry,
        // so shared names would collide with parallel tests.
        let pub_a = MemoryPublisher::new_local("switch_routing_topic_a", 10);
        let pub_b = MemoryPublisher::new_local("switch_routing_topic_b", 10);
        let pub_default = MemoryPublisher::new_local("switch_routing_topic_default", 10);

        let chan_a = pub_a.channel();
        let chan_b = pub_b.channel();
        let chan_default = pub_default.channel();

        let mut cases = HashMap::new();
        cases.insert(
            "A".to_string(),
            Arc::new(pub_a) as Arc<dyn MessagePublisher>,
        );
        cases.insert(
            "B".to_string(),
            Arc::new(pub_b) as Arc<dyn MessagePublisher>,
        );

        let switch = SwitchPublisher::new(
            "route_key".to_string(),
            cases,
            Some(Arc::new(pub_default) as Arc<dyn MessagePublisher>),
        );

        // Test Case A
        let msg_a = CanonicalMessage::from("payload_a").with_metadata_kv("route_key", "A");
        switch.send(msg_a).await.unwrap();
        assert_eq!(chan_a.len(), 1);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 0);
        chan_a.drain_messages();

        // Test Case B
        let msg_b = CanonicalMessage::from("payload_b").with_metadata_kv("route_key", "B");
        switch.send(msg_b).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 1);
        assert_eq!(chan_default.len(), 0);
        chan_b.drain_messages();

        // Test Default (Unknown Key)
        let msg_c =
            CanonicalMessage::new(b"payload_c".to_vec(), None).with_metadata_kv("route_key", "C");
        switch.send(msg_c).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 1);
        chan_default.drain_messages();

        // Test Default (Missing Key)
        let msg_d = CanonicalMessage::new(b"payload_d".to_vec(), None);
        switch.send(msg_d).await.unwrap();
        assert_eq!(chan_a.len(), 0);
        assert_eq!(chan_b.len(), 0);
        assert_eq!(chan_default.len(), 1);
    }

    /// Records the size of every `send_batch` call it receives.
    #[derive(Default)]
    struct RecordingPublisher {
        batches: std::sync::Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl MessagePublisher for RecordingPublisher {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.batches.lock().unwrap().push(messages.len());
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_switch_batch_merges_default_routed_groups() {
        let pub_default = Arc::new(RecordingPublisher::default());
        // Memory topics resolve through a process-global channel registry, so this needs a
        // name no other test uses or the parallel suite sees the other test's messages.
        let pub_a = MemoryPublisher::new_local("switch_merge_topic_a", 10);
        let chan_a = pub_a.channel();

        let mut cases = HashMap::new();
        cases.insert(
            "A".to_string(),
            Arc::new(pub_a) as Arc<dyn MessagePublisher>,
        );

        let switch = SwitchPublisher::new(
            "route_key".to_string(),
            cases,
            Some(pub_default.clone() as Arc<dyn MessagePublisher>),
        );

        // An unmatched value, an explicitly empty value and an absent key all route to
        // `default`, and must arrive there as a single batch.
        let messages = vec![
            CanonicalMessage::from("a").with_metadata_kv("route_key", "A"),
            CanonicalMessage::from("unmatched").with_metadata_kv("route_key", "C"),
            CanonicalMessage::from("empty").with_metadata_kv("route_key", ""),
            CanonicalMessage::from("absent"),
        ];
        switch.send_batch(messages).await.unwrap();

        assert_eq!(chan_a.len(), 1);
        assert_eq!(*pub_default.batches.lock().unwrap(), vec![3]);
    }

    /// Counts hook invocations; `fail_disconnect` makes teardown fail so the "keep going"
    /// behaviour is observable.
    #[derive(Default)]
    struct HookPublisher {
        connects: AtomicUsize,
        disconnects: AtomicUsize,
        fail_disconnect: bool,
    }

    impl HookPublisher {
        fn new(fail_disconnect: bool) -> Arc<Self> {
            Arc::new(Self {
                fail_disconnect,
                ..Default::default()
            })
        }

        fn counts(&self) -> (usize, usize) {
            (
                self.connects.load(Ordering::SeqCst),
                self.disconnects.load(Ordering::SeqCst),
            )
        }
    }

    #[async_trait]
    impl MessagePublisher for HookPublisher {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.connects.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            Some(Box::pin(async move {
                self.disconnects.fetch_add(1, Ordering::SeqCst);
                if self.fail_disconnect {
                    anyhow::bail!("teardown failed");
                }
                Ok(())
            }))
        }

        async fn send(&self, _message: CanonicalMessage) -> Result<Sent, PublisherError> {
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Cases and `default` alike are connected and torn down; one failing teardown does not
    /// skip the others.
    #[tokio::test]
    async fn test_switch_forwards_hooks_to_cases_and_default() {
        let ok_case = HookPublisher::new(false);
        let failing_case = HookPublisher::new(true);
        let fallback = HookPublisher::new(false);

        let mut cases: HashMap<String, Arc<dyn MessagePublisher>> = HashMap::new();
        cases.insert("a".into(), ok_case.clone());
        cases.insert("b".into(), failing_case.clone());
        let switch = SwitchPublisher::new("k".into(), cases, Some(fallback.clone()));

        switch.on_connect_hook().unwrap().await.unwrap();
        assert_eq!(ok_case.counts().0, 1);
        assert_eq!(failing_case.counts().0, 1);
        assert_eq!(fallback.counts().0, 1);

        let err = switch.on_disconnect_hook().unwrap().await.unwrap_err();
        assert!(err.to_string().contains("teardown failed"));
        assert_eq!(ok_case.counts().1, 1);
        assert_eq!(failing_case.counts().1, 1);
        assert_eq!(fallback.counts().1, 1);
    }

    #[cfg(feature = "filter")]
    mod predicate {
        use super::*;
        use crate::middleware::filter::CompiledFilter;

        fn switch_on(
            cases: Vec<(&str, Arc<dyn MessagePublisher>)>,
            default: Option<Arc<dyn MessagePublisher>>,
        ) -> SwitchPublisher {
            let compiled = cases
                .into_iter()
                .map(|(expression, publisher)| {
                    (CompiledFilter::new(expression).unwrap(), publisher)
                })
                .collect();
            SwitchPublisher::new_predicate(compiled, default)
        }

        fn json(amount: i64) -> CanonicalMessage {
            CanonicalMessage::from(format!(r#"{{"amount":{amount}}}"#).as_str())
        }

        /// The point of the mode: a threshold splits the stream across two destinations.
        #[tokio::test]
        async fn a_threshold_routes_to_either_branch() {
            let big = Arc::new(RecordingPublisher::default());
            let small = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![
                    ("amount > 100", big.clone() as Arc<dyn MessagePublisher>),
                    ("amount <= 100", small.clone() as Arc<dyn MessagePublisher>),
                ],
                None,
            );

            switch
                .send_batch(vec![json(500), json(7), json(101), json(100)])
                .await
                .unwrap();

            assert_eq!(*big.batches.lock().unwrap(), vec![2]);
            assert_eq!(*small.batches.lock().unwrap(), vec![2]);
        }

        /// Cases are ordered, so an overlapping later case never steals a message.
        #[tokio::test]
        async fn the_first_matching_case_wins() {
            let first = Arc::new(RecordingPublisher::default());
            let second = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![
                    ("amount > 10", first.clone() as Arc<dyn MessagePublisher>),
                    ("amount > 100", second.clone() as Arc<dyn MessagePublisher>),
                ],
                None,
            );

            switch.send(json(500)).await.unwrap();

            assert_eq!(*first.batches.lock().unwrap(), vec![1]);
            assert!(second.batches.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_unmatched_message_falls_through_to_default() {
            let matched = Arc::new(RecordingPublisher::default());
            let default = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![("amount > 100", matched.clone() as Arc<dyn MessagePublisher>)],
                Some(default.clone() as Arc<dyn MessagePublisher>),
            );

            switch.send(json(1)).await.unwrap();

            assert!(matched.batches.lock().unwrap().is_empty());
            assert_eq!(*default.batches.lock().unwrap(), vec![1]);
        }

        /// Without a default an unmatched message is dropped, not failed.
        #[tokio::test]
        async fn an_unmatched_message_without_a_default_is_dropped() {
            let matched = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![("amount > 100", matched.clone() as Arc<dyn MessagePublisher>)],
                None,
            );

            switch.send(json(1)).await.unwrap();

            assert!(matched.batches.lock().unwrap().is_empty());
        }

        /// A payload the expression cannot read is a config error, not a silent drop.
        #[tokio::test]
        async fn an_unreadable_payload_surfaces_as_an_error() {
            let matched = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![("amount > 100", matched as Arc<dyn MessagePublisher>)],
                None,
            );

            let result = switch.send(CanonicalMessage::from("not json")).await;

            assert!(matches!(result, Err(PublisherError::NonRetryable(_))));
        }

        /// One unreadable message must not take its whole batch down with it: the route
        /// acks a failed batch wholesale when no `dlq` is configured.
        #[tokio::test]
        async fn an_unreadable_message_fails_alone_in_a_batch() {
            let matched = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![("amount > 100", matched.clone() as Arc<dyn MessagePublisher>)],
                None,
            );

            let result = switch
                .send_batch(vec![
                    json(150),
                    CanonicalMessage::from("not json"),
                    json(200),
                ])
                .await
                .unwrap();

            match result {
                SentBatch::Partial { failed, .. } => assert_eq!(failed.len(), 1),
                SentBatch::Ack => panic!("the unreadable message should have been reported"),
            }
            assert_eq!(*matched.batches.lock().unwrap(), vec![2]);
        }

        /// Metadata routing needs no payload parse at all.
        #[tokio::test]
        async fn metadata_routes_through_the_meta_prefix() {
            let urgent = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![(
                    r#"meta.priority == "high""#,
                    urgent.clone() as Arc<dyn MessagePublisher>,
                )],
                None,
            );

            switch
                .send(CanonicalMessage::from("not json").with_metadata_kv("priority", "high"))
                .await
                .unwrap();

            assert_eq!(*urgent.batches.lock().unwrap(), vec![1]);
        }

        #[tokio::test]
        async fn an_early_metadata_match_does_not_parse_the_payload() {
            let urgent = Arc::new(RecordingPublisher::default());
            let fallback = Arc::new(RecordingPublisher::default());
            let switch = switch_on(
                vec![
                    (
                        r#"meta.priority == "high""#,
                        urgent.clone() as Arc<dyn MessagePublisher>,
                    ),
                    ("amount > 100", fallback as Arc<dyn MessagePublisher>),
                ],
                None,
            );

            switch
                .send(CanonicalMessage::from("not json").with_metadata_kv("priority", "high"))
                .await
                .unwrap();

            assert_eq!(*urgent.batches.lock().unwrap(), vec![1]);
        }
    }
}
