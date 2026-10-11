use crate::traits::{BoxFuture, EndpointStatus, MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Once};
use tracing::{debug, warn};

/// Structural publisher that delivers every message to all of its destinations.
///
/// If a destination replies (`response`, `static`, a `request` whose `forward_to` replies, ...)
/// that reply is returned to the caller. When several destinations reply to the same message
/// the **first one in list order** wins and the rest are dropped — a caller can only receive
/// one answer. Give the other legs a non-replying `forward_to` to silence it.
pub struct FanoutPublisher {
    publishers: Vec<Arc<dyn MessagePublisher>>,
    /// Latches the "several destinations replied" warning: it reports a misconfiguration that
    /// repeats on every message, so warning per drop would flood the log at any real rate.
    extra_response_warned: Once,
}

impl FanoutPublisher {
    pub fn new(publishers: Vec<Arc<dyn MessagePublisher>>) -> Self {
        Self {
            publishers,
            extra_response_warned: Once::new(),
        }
    }

    /// Reports a dropped duplicate reply: `warn!` for the first one this route sees, `debug!`
    /// for the rest so individual messages stay diagnosable when logging is turned up.
    fn warn_extra_response(&self, response: &CanonicalMessage) {
        let mut warned = false;
        self.extra_response_warned.call_once(|| {
            warned = true;
            warn!(
                message_id = %format!("{:032x}", response.message_id),
                "fanout: more than one destination replied; keeping the first in list order and \
                 dropping the rest. Give the other destinations a non-replying `forward_to`. \
                 Logged once per route; further drops are logged at debug level."
            );
        });
        if !warned {
            debug!(
                message_id = %format!("{:032x}", response.message_id),
                "fanout: dropped a duplicate destination reply"
            );
        }
    }
}

#[async_trait]
impl MessagePublisher for FanoutPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            for publisher in &self.publishers {
                if let Some(hook) = publisher.on_connect_hook() {
                    hook.await?;
                }
            }
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            // Every destination gets torn down even if an earlier one fails, so one bad leg
            // cannot skip another's durable teardown. The first error is reported.
            let mut first_error = None;
            for publisher in &self.publishers {
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
        let mut response: Option<CanonicalMessage> = None;
        for publisher in &self.publishers {
            // We must clone the message for each publisher.
            if let Sent::Response(r) = publisher.send(message.clone()).await? {
                if response.is_none() {
                    response = Some(r);
                } else {
                    self.warn_extra_response(&r);
                }
            }
        }
        Ok(response.map_or(Sent::Ack, Sent::Response))
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        use futures::future::join_all;

        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        // Send the batch to all publishers concurrently.
        let batch_sends = self.publishers.iter().map(|p| {
            // Each publisher gets a clone of the entire batch. This can be memory-intensive.
            p.send_batch(messages.clone())
        });

        let results = join_all(batch_sends).await;

        // A hard error from any publisher propagates. Otherwise per-destination failures merge:
        // a message that failed at any destination is nacked for the whole fan-out, listed once
        // with its most recoverable error. Responses are collected in destination order, keeping
        // the first reply per message: the caller has one reply channel.
        let mut failed: Vec<(CanonicalMessage, PublisherError)> = Vec::new();
        // Keyed by id and occurrence within a leg, so same-id messages of a batch stay apart.
        let mut failed_at: HashMap<(u128, usize), usize> = HashMap::new();
        let mut responses: Vec<CanonicalMessage> = Vec::new();
        let mut responded: HashSet<u128> = HashSet::new();
        for result in results {
            match result? {
                SentBatch::Ack => {}
                SentBatch::Partial {
                    responses: child_responses,
                    failed: child_failed,
                } => {
                    let mut seen: HashMap<u128, usize> = HashMap::new();
                    for (message, error) in child_failed {
                        let occurrence = seen.entry(message.message_id).or_default();
                        let key = (message.message_id, *occurrence);
                        *occurrence += 1;
                        match failed_at.get(&key) {
                            Some(&at) if recoverability(&error) > recoverability(&failed[at].1) => {
                                failed[at].1 = error;
                            }
                            Some(_) => {}
                            None => {
                                failed_at.insert(key, failed.len());
                                failed.push((message, error));
                            }
                        }
                    }
                    for response in child_responses.into_iter().flatten() {
                        if responded.insert(response.message_id) {
                            responses.push(response);
                        } else {
                            self.warn_extra_response(&response);
                        }
                    }
                }
            }
        }

        if failed.is_empty() && responses.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: (!responses.is_empty()).then_some(responses),
                failed,
            })
        }
    }

    async fn status(&self) -> EndpointStatus {
        use futures::future::join_all;

        let status_futs = self.publishers.iter().map(|p| p.status());
        let results = join_all(status_futs).await;

        let mut healthy = true;
        let mut pending = 0;
        let mut capacity = 0;
        let mut error: Option<String> = None;
        let mut details = Vec::new();

        for status in results {
            if !status.healthy {
                healthy = false;
                if error.is_none() {
                    error = status.error.clone();
                }
            }
            pending += status.pending.unwrap_or(0);
            capacity += status.capacity.unwrap_or(0);
            details.push(status);
        }

        EndpointStatus {
            healthy,
            pending: Some(pending),
            capacity: Some(capacity),
            error,
            details: serde_json::json!({ "destinations": details }),
            ..Default::default()
        }
    }

    /// Ordered if any leg is: a fanout is only as order-tolerant as its strictest sink.
    fn requires_ordered_publish(&self) -> bool {
        self.publishers.iter().any(|p| p.requires_ordered_publish())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Which of two errors for one message wins: the one that keeps it from being dropped.
fn recoverability(error: &PublisherError) -> u8 {
    match error {
        PublisherError::NonRetryable(_) => 0,
        PublisherError::Retryable(_) => 1,
        PublisherError::Connection(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ProcessingError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingPublisher {
        single_payloads: Mutex<Vec<String>>,
        batch_payloads: Mutex<Vec<Vec<String>>>,
        status: EndpointStatus,
        batch_error: Option<String>,
        /// Payloads this publisher reports as failed instead of erroring the whole batch.
        batch_partial_failures: Vec<String>,
        /// Report those failures as `NonRetryable` instead of `Retryable`.
        partial_failures_permanent: bool,
    }

    #[async_trait]
    impl MessagePublisher for RecordingPublisher {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            self.single_payloads
                .lock()
                .unwrap()
                .push(message.get_payload_str().to_string());
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.batch_payloads.lock().unwrap().push(
                messages
                    .iter()
                    .map(|message| message.get_payload_str().to_string())
                    .collect(),
            );

            if let Some(message) = &self.batch_error {
                return Err(ProcessingError::NonRetryable(anyhow::anyhow!(
                    message.clone()
                )));
            }

            if !self.batch_partial_failures.is_empty() {
                let failed: Vec<_> = messages
                    .into_iter()
                    .filter(|m| {
                        self.batch_partial_failures
                            .contains(&m.get_payload_str().to_string())
                    })
                    .map(|m| {
                        let e = anyhow::anyhow!("destination rejected");
                        if self.partial_failures_permanent {
                            (m, ProcessingError::NonRetryable(e))
                        } else {
                            (m, ProcessingError::Retryable(e))
                        }
                    })
                    .collect();
                if !failed.is_empty() {
                    return Ok(SentBatch::Partial {
                        responses: None,
                        failed,
                    });
                }
            }

            Ok(SentBatch::Ack)
        }

        async fn status(&self) -> EndpointStatus {
            self.status.clone()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_fanout_send_delivers_message_to_all_publishers() {
        let left = Arc::new(RecordingPublisher::default());
        let right = Arc::new(RecordingPublisher::default());
        let fanout = FanoutPublisher::new(vec![left.clone(), right.clone()]);

        let result = fanout.send(CanonicalMessage::from("hello")).await.unwrap();
        assert!(matches!(result, Sent::Ack));
        assert_eq!(left.single_payloads.lock().unwrap().as_slice(), ["hello"]);
        assert_eq!(right.single_payloads.lock().unwrap().as_slice(), ["hello"]);
    }

    #[tokio::test]
    async fn test_fanout_send_batch_propagates_errors() {
        let ok = Arc::new(RecordingPublisher::default());
        let failing = Arc::new(RecordingPublisher {
            batch_error: Some("fanout failure".to_string()),
            ..Default::default()
        });
        let fanout = FanoutPublisher::new(vec![ok.clone(), failing.clone()]);

        let err = fanout
            .send_batch(vec![
                CanonicalMessage::from("one"),
                CanonicalMessage::from("two"),
            ])
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessingError::NonRetryable(_)));
        assert_eq!(ok.batch_payloads.lock().unwrap().len(), 1);
        assert_eq!(failing.batch_payloads.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fanout_send_batch_preserves_child_partial_failures() {
        let ok = Arc::new(RecordingPublisher::default());
        let partial = Arc::new(RecordingPublisher {
            batch_partial_failures: vec!["two".to_string()],
            ..Default::default()
        });
        let fanout = FanoutPublisher::new(vec![ok.clone(), partial.clone()]);

        let sent = fanout
            .send_batch(vec![
                CanonicalMessage::from("one"),
                CanonicalMessage::from("two"),
            ])
            .await
            .unwrap();

        match sent {
            SentBatch::Partial { responses, failed } => {
                assert!(responses.is_none());
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0.get_payload_str(), "two");
            }
            SentBatch::Ack => panic!("expected partial failure to be preserved"),
        }
    }

    /// STRUCT-02/03: a message that fails on two legs is reported once, with the error
    /// class that keeps it alive, whatever the order of the legs.
    #[tokio::test]
    async fn test_fanout_merges_one_message_failing_on_several_legs() {
        for permanent_first in [true, false] {
            let leg = |permanent| {
                Arc::new(RecordingPublisher {
                    batch_partial_failures: vec!["two".to_string()],
                    partial_failures_permanent: permanent,
                    ..Default::default()
                }) as Arc<dyn MessagePublisher>
            };
            let fanout = FanoutPublisher::new(vec![leg(permanent_first), leg(!permanent_first)]);
            let sent = fanout
                .send_batch(vec![
                    CanonicalMessage::from("one"),
                    CanonicalMessage::from("two"),
                ])
                .await
                .unwrap();
            let SentBatch::Partial { failed, .. } = sent else {
                panic!("expected a partial failure");
            };
            assert_eq!(failed.len(), 1);
            assert!(matches!(failed[0].1, ProcessingError::Retryable(_)));
        }
    }

    /// A publisher that always replies, standing in for `response` / `static` legs.
    struct ReplyingPublisher {
        suffix: &'static str,
    }

    #[async_trait]
    impl MessagePublisher for ReplyingPublisher {
        async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
            message.payload = [&message.payload[..], self.suffix.as_bytes()]
                .concat()
                .into();
            Ok(Sent::Response(message))
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_fanout_send_returns_the_replying_legs_response() {
        let sink = Arc::new(RecordingPublisher::default());
        let fanout = FanoutPublisher::new(vec![
            sink.clone(),
            Arc::new(ReplyingPublisher { suffix: "-reply" }),
        ]);

        match fanout.send(CanonicalMessage::from("hello")).await.unwrap() {
            Sent::Response(reply) => assert_eq!(reply.get_payload_str(), "hello-reply"),
            Sent::Ack => panic!("expected the replying leg's response"),
        }
        // The non-replying leg still received the message.
        assert_eq!(sink.single_payloads.lock().unwrap().as_slice(), ["hello"]);
    }

    #[tokio::test]
    async fn test_fanout_send_keeps_first_response_when_several_legs_reply() {
        let fanout = FanoutPublisher::new(vec![
            Arc::new(ReplyingPublisher { suffix: "-first" }),
            Arc::new(ReplyingPublisher { suffix: "-second" }),
        ]);

        assert!(!fanout.extra_response_warned.is_completed());

        match fanout.send(CanonicalMessage::from("hello")).await.unwrap() {
            Sent::Response(reply) => assert_eq!(reply.get_payload_str(), "hello-first"),
            Sent::Ack => panic!("expected a response"),
        }
        assert!(
            fanout.extra_response_warned.is_completed(),
            "the first dropped reply must warn"
        );

        // The latch holds for the rest of the route's life: later drops log at debug level,
        // and first-in-list-order still decides the reply.
        match fanout.send(CanonicalMessage::from("again")).await.unwrap() {
            Sent::Response(reply) => assert_eq!(reply.get_payload_str(), "again-first"),
            Sent::Ack => panic!("expected a response"),
        }
    }

    #[tokio::test]
    async fn test_fanout_send_batch_returns_responses_keyed_by_message() {
        let sink = Arc::new(RecordingPublisher::default());
        let fanout = FanoutPublisher::new(vec![
            sink.clone(),
            Arc::new(ReplyingPublisher { suffix: "-reply" }),
        ]);

        let messages = vec![CanonicalMessage::from("one"), CanonicalMessage::from("two")];
        let ids: Vec<_> = messages.iter().map(|m| m.message_id).collect();

        match fanout.send_batch(messages).await.unwrap() {
            SentBatch::Partial { responses, failed } => {
                assert!(failed.is_empty());
                let responses = responses.expect("replying leg produced responses");
                assert_eq!(responses.len(), 2);
                assert_eq!(responses[0].message_id, ids[0]);
                assert_eq!(responses[0].get_payload_str(), "one-reply");
                assert_eq!(responses[1].message_id, ids[1]);
                assert_eq!(responses[1].get_payload_str(), "two-reply");
            }
            SentBatch::Ack => panic!("expected responses to be forwarded"),
        }
        assert_eq!(sink.batch_payloads.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fanout_send_batch_keeps_one_response_per_message() {
        let fanout = FanoutPublisher::new(vec![
            Arc::new(ReplyingPublisher { suffix: "-first" }),
            Arc::new(ReplyingPublisher { suffix: "-second" }),
        ]);

        match fanout
            .send_batch(vec![CanonicalMessage::from("one")])
            .await
            .unwrap()
        {
            SentBatch::Partial { responses, .. } => {
                let responses = responses.expect("expected a response");
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].get_payload_str(), "one-first");
            }
            SentBatch::Ack => panic!("expected a response"),
        }
    }

    /// End-to-end: a request/reply input fans out to a plain sink and a `response` leg.
    /// The sink still gets its copy, and the caller gets the reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fanout_replies_to_caller_while_sibling_gets_a_copy() {
        use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
        use crate::models::{Endpoint, EndpointType, MemoryConfig};
        use crate::route::Route;
        use crate::traits::MessageConsumer;

        let inbox = format!("fanout_rr_in_{}", fast_uuid_v7::gen_id_str());
        let mirror = format!("fanout_rr_mirror_{}", fast_uuid_v7::gen_id_str());

        // Subscribe before the route starts so the mirrored copy is not dropped.
        let mut mirror_consumer = MemoryConsumer::new(&MemoryConfig {
            topic: mirror.clone(),
            capacity: Some(10),
            ..Default::default()
        })
        .unwrap();

        let output = Endpoint::new(EndpointType::Fanout(vec![
            Endpoint::new_memory(&mirror, 10),
            Endpoint::new_response(),
        ]));
        let route = Route::new(Endpoint::new_memory(&inbox, 10), output);
        route.deploy("fanout_rr_test").await.unwrap();

        let publisher = MemoryPublisher::new(&MemoryConfig {
            topic: inbox.clone(),
            capacity: Some(10),
            request_reply: true,
            request_timeout_ms: Some(2000),
            ..Default::default()
        })
        .unwrap();

        match publisher.send("ping".into()).await.unwrap() {
            Sent::Response(reply) => assert_eq!(reply.get_payload_str(), "ping"),
            Sent::Ack => panic!("fanout should have replied through its response leg"),
        }

        let mirrored = mirror_consumer.receive().await.unwrap();
        assert_eq!(mirrored.message.get_payload_str(), "ping");

        Route::stop("fanout_rr_test").await;
    }

    #[tokio::test]
    async fn test_fanout_status_aggregates_destination_status() {
        let healthy = Arc::new(RecordingPublisher {
            status: EndpointStatus {
                healthy: true,
                target: "a".to_string(),
                pending: Some(2),
                capacity: Some(5),
                error: None,
                details: serde_json::json!({"id": "a"}),
            },
            ..Default::default()
        });
        let unhealthy = Arc::new(RecordingPublisher {
            status: EndpointStatus {
                healthy: false,
                target: "b".to_string(),
                pending: Some(3),
                capacity: Some(7),
                error: Some("down".to_string()),
                details: serde_json::json!({"id": "b"}),
            },
            ..Default::default()
        });
        let fanout = FanoutPublisher::new(vec![healthy, unhealthy]);

        let status = fanout.status().await;
        assert!(!status.healthy);
        assert_eq!(status.pending, Some(5));
        assert_eq!(status.capacity, Some(12));
        assert_eq!(status.error.as_deref(), Some("down"));
        assert_eq!(status.details["destinations"].as_array().unwrap().len(), 2);
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

    /// A nested `request` branch has its own connect hook; the fan-out must run it, and must
    /// tear every branch down even when one fails.
    #[tokio::test]
    async fn test_fanout_forwards_hooks_to_every_destination() {
        let first = HookPublisher::new(false);
        let failing = HookPublisher::new(true);
        let last = HookPublisher::new(false);
        let fanout = FanoutPublisher::new(vec![
            first.clone() as Arc<dyn MessagePublisher>,
            failing.clone(),
            last.clone(),
        ]);

        fanout.on_connect_hook().unwrap().await.unwrap();
        assert_eq!(first.counts().0, 1);
        assert_eq!(failing.counts().0, 1);
        assert_eq!(last.counts().0, 1);

        let err = fanout.on_disconnect_hook().unwrap().await.unwrap_err();
        assert!(err.to_string().contains("teardown failed"));
        assert_eq!(first.counts().1, 1);
        assert_eq!(failing.counts().1, 1);
        // Reached despite the earlier failure.
        assert_eq!(last.counts().1, 1);
    }
}
