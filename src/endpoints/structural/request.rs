use crate::traits::{BoxFuture, MessagePublisher, PublisherError, Sent, SentBatch};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tracing::warn;

/// Structural publisher that sends each message to a request-capable endpoint and forwards
/// the response to another endpoint.
///
/// On success the response is forwarded verbatim (it already carries the original
/// `message_id`, and for HTTP the `http_status_code`). On request error/timeout the original
/// message is forwarded unchanged instead, so a downstream `switch` can route success vs.
/// failure on the transport-native status. This endpoint adds no metadata of its own.
///
/// That fallback never answers the caller: if the forward leg would turn the original into a
/// reply, the request error is returned instead, so a failed upstream cannot look like a
/// successful response.
///
/// Whatever `forward_to` itself returns is passed back up, so `forward_to: { response: {} }`
/// replies to the origin of the current request; a `forward_to` that is a plain sink (or
/// `null`, to discard) acks instead.
///
/// Retry, if configured, wraps the whole `send`: a `forward_to` failure after a successful
/// request re-issues the request on the next attempt. The `to` endpoint must therefore be
/// idempotent — delivery is at-least-once.
pub struct RequestForwardPublisher {
    request: Arc<dyn MessagePublisher>,
    forward: Arc<dyn MessagePublisher>,
}

impl RequestForwardPublisher {
    pub fn new(request: Arc<dyn MessagePublisher>, forward: Arc<dyn MessagePublisher>) -> Self {
        Self { request, forward }
    }
}

#[async_trait]
impl MessagePublisher for RequestForwardPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            if let Some(hook) = self.request.on_connect_hook() {
                hook.await?;
            }
            if let Some(hook) = self.forward.on_connect_hook() {
                hook.await?;
            }
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            // Both legs are torn down even if the first fails; the first error is reported.
            let mut first_error = None;
            for publisher in [&self.request, &self.forward] {
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
        // Keep the original for the error fallback (preserves its message_id).
        let original = message.clone();
        match self.request.send(message).await {
            // Pass the forward leg's own outcome back up: a `response` forward turns this
            // into the route's reply, an ordinary sink acks.
            Ok(Sent::Response(response)) => self.forward.send(response).await,
            Ok(Sent::Ack) => {
                // The inner endpoint produced no response (e.g. request_reply disabled), so
                // there is nothing to forward.
                warn!(
                    message_id = %format!("{:032x}", original.message_id),
                    "request endpoint: inner returned Ack with no response, nothing forwarded"
                );
                Ok(Sent::Ack)
            }
            Err(e) => {
                // Forward the original unchanged; downstream (a switch on the transport
                // status) decides how to handle it.
                warn!(
                    message_id = %format!("{:032x}", original.message_id),
                    error = %e,
                    "request endpoint: inner send failed, forwarding original message"
                );
                match self.forward.send(original).await? {
                    // Routing a failure to a sink is the documented fallback, but replying it
                    // to the caller is not: the echoed request reads as a successful answer.
                    // Surface the request error so `retry`/`dlq` and the route see it.
                    Sent::Response(_) => Err(e),
                    Sent::Ack => Ok(Sent::Ack),
                }
            }
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        use futures::StreamExt;

        struct ForwardCandidate {
            index: usize,
            original: CanonicalMessage,
            forwarded_id: u128,
            request_error: Option<PublisherError>,
        }

        let request = &self.request;
        let request_concurrency = if request.requires_ordered_publish() {
            1
        } else {
            crate::traits::SEND_BATCH_CONCURRENCY
        };
        let mut requests = futures::stream::iter(messages.into_iter().enumerate().map(
            |(index, original)| async move {
                let result = request.send(original.clone()).await;
                (index, original, result)
            },
        ))
        .buffer_unordered(request_concurrency);

        let mut candidates = Vec::new();
        let mut forwarded = Vec::new();
        while let Some((index, original, result)) = requests.next().await {
            let (message, request_error) = match result {
                Ok(Sent::Response(response)) => (response, None),
                Ok(Sent::Ack) => {
                    warn!(
                        message_id = %format!("{:032x}", original.message_id),
                        "request endpoint: inner returned Ack with no response, nothing forwarded"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(
                        message_id = %format!("{:032x}", original.message_id),
                        error = %error,
                        "request endpoint: inner send failed, forwarding original message"
                    );
                    (original.clone(), Some(error))
                }
            };
            candidates.push(ForwardCandidate {
                index,
                original,
                forwarded_id: message.message_id,
                request_error,
            });
            forwarded.push(message);
        }

        let mut ordered: Vec<_> = candidates.into_iter().zip(forwarded).collect();
        ordered.sort_by_key(|(candidate, _)| candidate.index);
        let (candidates, forwarded): (Vec<_>, Vec<_>) = ordered.into_iter().unzip();

        if forwarded.is_empty() {
            return Ok(SentBatch::Ack);
        }

        let result = self.forward.send_batch(forwarded).await?;
        let SentBatch::Partial { responses, failed } = result else {
            return Ok(SentBatch::Ack);
        };

        // Queues per id, so two forwarded messages that share one each get their own result.
        use std::collections::{HashMap, VecDeque};
        let mut response_by_id: HashMap<u128, VecDeque<CanonicalMessage>> = HashMap::new();
        for response in responses.unwrap_or_default() {
            response_by_id
                .entry(response.message_id)
                .or_default()
                .push_back(response);
        }
        let mut failure_by_id: HashMap<u128, VecDeque<PublisherError>> = HashMap::new();
        for (message, error) in failed {
            failure_by_id
                .entry(message.message_id)
                .or_default()
                .push_back(error);
        }
        let mut outcomes = Vec::new();
        let mut unresolved = Vec::new();

        for candidate in candidates {
            let id = candidate.forwarded_id;
            if let Some(error) = failure_by_id.get_mut(&id).and_then(VecDeque::pop_front) {
                outcomes.push((candidate.index, None, Some((candidate.original, error))));
            } else if let Some(response) = response_by_id.get_mut(&id).and_then(VecDeque::pop_front)
            {
                if let Some(error) = candidate.request_error {
                    outcomes.push((candidate.index, None, Some((candidate.original, error))));
                } else {
                    outcomes.push((candidate.index, Some(response), None));
                }
            } else {
                unresolved.push(candidate);
            }
        }

        // A failure under an id nothing was forwarded with cannot be attributed, so every
        // message without a result of its own is failed rather than acked.
        let unmatched: usize = failure_by_id.values().map(VecDeque::len).sum();
        if unmatched > 0 {
            warn!(
                unmatched,
                "request endpoint: forward_to reported failures for unknown message ids; failing the unconfirmed messages of the batch"
            );
            for candidate in unresolved {
                let error = PublisherError::Retryable(anyhow::anyhow!(
                    "forward_to reported a failure that matches no forwarded message"
                ));
                outcomes.push((candidate.index, None, Some((candidate.original, error))));
            }
        }

        outcomes.sort_by_key(|(index, _, _)| *index);
        let responses: Vec<_> = outcomes
            .iter_mut()
            .filter_map(|(_, response, _)| response.take())
            .collect();
        let failed: Vec<_> = outcomes
            .into_iter()
            .filter_map(|(_, _, failure)| failure)
            .collect();

        if responses.is_empty() && failed.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: (!responses.is_empty()).then_some(responses),
                failed,
            })
        }
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        // Health follows the request-capable `to` endpoint (where the actual I/O happens).
        self.request.status().await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.request.requires_ordered_publish() || self.forward.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::EndpointStatus;
    use std::sync::Mutex as StdMutex;

    /// A request-capable publisher whose `send` returns a preconfigured result once.
    struct MockRequest {
        result: StdMutex<Option<Result<Sent, PublisherError>>>,
    }

    impl MockRequest {
        fn new(result: Result<Sent, PublisherError>) -> Arc<Self> {
            Arc::new(Self {
                result: StdMutex::new(Some(result)),
            })
        }
    }

    #[async_trait]
    impl MessagePublisher for MockRequest {
        async fn send(&self, _message: CanonicalMessage) -> Result<Sent, PublisherError> {
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("MockRequest send called more than configured")
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
        }

        async fn status(&self) -> EndpointStatus {
            EndpointStatus::default()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// A sink that records every message it receives, and can be told to fail.
    struct RecordingSink {
        received: StdArc,
        fail: bool,
    }

    type StdArc = std::sync::Arc<StdMutex<Vec<CanonicalMessage>>>;

    impl RecordingSink {
        fn new() -> (Arc<Self>, StdArc) {
            let received: StdArc = std::sync::Arc::new(StdMutex::new(Vec::new()));
            (
                Arc::new(Self {
                    received: received.clone(),
                    fail: false,
                }),
                received,
            )
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                received: std::sync::Arc::new(StdMutex::new(Vec::new())),
                fail: true,
            })
        }
    }

    #[async_trait]
    impl MessagePublisher for RecordingSink {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            if self.fail {
                return Err(PublisherError::Retryable(anyhow::anyhow!("sink failed")));
            }
            self.received.lock().unwrap().push(message);
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
        }

        async fn status(&self) -> EndpointStatus {
            EndpointStatus::default()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Replies to every request with the request itself.
    struct EchoRequest;

    #[async_trait]
    impl MessagePublisher for EchoRequest {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
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

    /// A forward leg that fails the first message of a batch and reports it under a new id.
    struct RenamingFailSink;

    #[async_trait]
    impl MessagePublisher for RenamingFailSink {
        async fn send(&self, _message: CanonicalMessage) -> Result<Sent, PublisherError> {
            Ok(Sent::Ack)
        }

        async fn send_batch(
            &self,
            mut messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            let mut renamed = messages.remove(0);
            renamed.message_id = renamed.message_id.wrapping_add(1_000_000);
            Ok(SentBatch::Partial {
                responses: None,
                failed: vec![(renamed, PublisherError::Retryable(anyhow::anyhow!("no")))],
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// STRUCT-04: a forward failure that matches no forwarded id must not be acked away.
    #[tokio::test]
    async fn an_unmatched_forward_failure_fails_the_batch() {
        let publisher =
            RequestForwardPublisher::new(Arc::new(EchoRequest), Arc::new(RenamingFailSink));
        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::from("one"),
                CanonicalMessage::from("two"),
            ])
            .await
            .unwrap();
        let SentBatch::Partial { failed, .. } = sent else {
            panic!("the failed forward was acknowledged");
        };
        assert_eq!(failed.len(), 2);
    }

    struct MixedRequest;

    #[async_trait]
    impl MessagePublisher for MixedRequest {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            match message.get_payload_str().as_ref() {
                "success" => {
                    // Complete after the failure case so batch tests verify that
                    // forwarding restores input order after concurrent requests.
                    tokio::task::yield_now().await;
                    let mut response = message;
                    response.payload = "success-response".into();
                    Ok(Sent::Response(response))
                }
                "failure" => Err(PublisherError::Retryable(anyhow::anyhow!("request failed"))),
                "ack" => Ok(Sent::Ack),
                other => panic!("unexpected request payload: {other}"),
            }
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            crate::traits::send_batch_helper(self, messages, |p, message| Box::pin(p.send(message)))
                .await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct BatchRecordingSink {
        batches: std::sync::Arc<StdMutex<Vec<Vec<CanonicalMessage>>>>,
    }

    struct OrderedRequest {
        calls: std::sync::Arc<StdMutex<Vec<String>>>,
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        max_active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl MessagePublisher for OrderedRequest {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            use std::sync::atomic::Ordering;

            self.calls
                .lock()
                .unwrap()
                .push(message.get_payload_str().into_owned());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Sent::Response(message))
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            crate::traits::send_batch_helper(self, messages, |publisher, message| {
                Box::pin(publisher.send(message))
            })
            .await
        }

        fn requires_ordered_publish(&self) -> bool {
            true
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait]
    impl MessagePublisher for BatchRecordingSink {
        async fn send(&self, _message: CanonicalMessage) -> Result<Sent, PublisherError> {
            panic!("forward_to must receive one batch, not individual sends")
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.batches.lock().unwrap().push(messages);
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn batch_requests_are_collected_into_one_forward_batch() {
        let batches = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let publisher = RequestForwardPublisher::new(
            Arc::new(MixedRequest),
            Arc::new(BatchRecordingSink {
                batches: batches.clone(),
            }),
        );

        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::from("success"),
                CanonicalMessage::from("failure"),
                CanonicalMessage::from("ack"),
            ])
            .await
            .unwrap();
        assert!(matches!(sent, SentBatch::Ack));

        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        let payloads: Vec<_> = batches[0]
            .iter()
            .map(|message| message.get_payload_str().into_owned())
            .collect();
        assert_eq!(payloads, ["success-response", "failure"]);
    }

    #[tokio::test]
    async fn ordered_request_publisher_is_sent_one_at_a_time_in_input_order() {
        let calls = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let batches = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let publisher = RequestForwardPublisher::new(
            Arc::new(OrderedRequest {
                calls: calls.clone(),
                active,
                max_active: max_active.clone(),
            }),
            Arc::new(BatchRecordingSink { batches }),
        );

        publisher
            .send_batch(vec!["first".into(), "second".into(), "third".into()])
            .await
            .unwrap();

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["first", "second", "third"]
        );
        assert_eq!(
            max_active.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ordered request publishers must never overlap sends"
        );
    }

    #[tokio::test]
    async fn batch_fallback_response_surfaces_original_request_error() {
        use crate::endpoints::structural::response::ResponsePublisher;

        let success = CanonicalMessage::from("success");
        let success_id = success.message_id;
        let failure = CanonicalMessage::from("failure");
        let failure_id = failure.message_id;
        let publisher =
            RequestForwardPublisher::new(Arc::new(MixedRequest), Arc::new(ResponsePublisher));

        match publisher.send_batch(vec![success, failure]).await.unwrap() {
            SentBatch::Partial {
                responses: Some(responses),
                failed,
            } => {
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].message_id, success_id);
                assert_eq!(responses[0].get_payload_str(), "success-response");
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0.message_id, failure_id);
                assert!(failed[0].1.to_string().contains("request failed"));
            }
            other => panic!("expected mixed batch outcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forwards_response_verbatim() {
        let mut response = CanonicalMessage::from("http-body");
        response
            .metadata
            .insert("http_status_code".into(), "200".into());
        let response_id = response.message_id;

        let request = MockRequest::new(Ok(Sent::Response(response)));
        let (sink, received) = RecordingSink::new();
        let publisher = RequestForwardPublisher::new(request, sink);

        let sent = publisher.send(CanonicalMessage::from("in")).await.unwrap();
        assert!(matches!(sent, Sent::Ack));

        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].get_payload_str(), "http-body");
        assert_eq!(
            received[0]
                .metadata
                .get("http_status_code")
                .map(String::as_str),
            Some("200")
        );
        assert_eq!(received[0].message_id, response_id);
    }

    #[tokio::test]
    async fn forwards_original_unchanged_on_error() {
        let request = MockRequest::new(Err(PublisherError::Retryable(anyhow::anyhow!("boom"))));
        let (sink, received) = RecordingSink::new();
        let publisher = RequestForwardPublisher::new(request, sink);

        let original = CanonicalMessage::from("original-payload");
        let original_id = original.message_id;

        let sent = publisher.send(original).await.unwrap();
        assert!(matches!(sent, Sent::Ack));

        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].get_payload_str(), "original-payload");
        assert_eq!(received[0].message_id, original_id);
        // No status tag was invented on the fallback.
        assert!(!received[0].metadata.contains_key("http_status_code"));
    }

    /// A failed request must not answer the caller with its own echoed payload: the error
    /// surfaces instead, so `retry`/`dlq` and the route can act on it.
    #[tokio::test]
    async fn error_with_response_forward_surfaces_the_error() {
        use crate::endpoints::structural::response::ResponsePublisher;

        let request = MockRequest::new(Err(PublisherError::Retryable(anyhow::anyhow!(
            "upstream down"
        ))));
        let publisher = RequestForwardPublisher::new(request, Arc::new(ResponsePublisher));

        let err = publisher
            .send(CanonicalMessage::from("original-payload"))
            .await
            .expect_err("a failed request must not reply to the caller");
        assert!(matches!(err, PublisherError::Retryable(_)));
        assert!(err.to_string().contains("upstream down"));
    }

    #[tokio::test]
    async fn inner_ack_forwards_nothing() {
        let request = MockRequest::new(Ok(Sent::Ack));
        let (sink, received) = RecordingSink::new();
        let publisher = RequestForwardPublisher::new(request, sink);

        let sent = publisher.send(CanonicalMessage::from("in")).await.unwrap();
        assert!(matches!(sent, Sent::Ack));
        assert_eq!(received.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn response_forward_replies_to_caller() {
        let mut response = CanonicalMessage::from("http-body");
        response
            .metadata
            .insert("http_status_code".into(), "200".into());
        let response_id = response.message_id;

        let request = MockRequest::new(Ok(Sent::Response(response)));
        let publisher = RequestForwardPublisher::new(
            request,
            Arc::new(crate::endpoints::structural::response::ResponsePublisher),
        );

        match publisher.send(CanonicalMessage::from("in")).await.unwrap() {
            Sent::Response(reply) => {
                assert_eq!(reply.get_payload_str(), "http-body");
                assert_eq!(reply.message_id, response_id);
            }
            Sent::Ack => panic!("expected the response forward to reply"),
        }
    }

    #[tokio::test]
    async fn null_forward_discards_response() {
        let request = MockRequest::new(Ok(Sent::Response(CanonicalMessage::from("staging"))));
        let publisher = RequestForwardPublisher::new(
            request,
            Arc::new(crate::endpoints::structural::null::NullPublisher),
        );

        let sent = publisher.send(CanonicalMessage::from("in")).await.unwrap();
        assert!(matches!(sent, Sent::Ack));
    }

    #[tokio::test]
    async fn forward_failure_propagates() {
        let mut response = CanonicalMessage::from("body");
        response
            .metadata
            .insert("http_status_code".into(), "200".into());
        let request = MockRequest::new(Ok(Sent::Response(response)));
        let publisher = RequestForwardPublisher::new(request, RecordingSink::failing());

        let err = publisher
            .send(CanonicalMessage::from("in"))
            .await
            .unwrap_err();
        assert!(matches!(err, PublisherError::Retryable(_)));
    }
}
