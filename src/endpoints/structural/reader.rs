use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ReaderPublisher {
    consumer: Arc<Mutex<Box<dyn MessageConsumer>>>,
}

impl ReaderPublisher {
    pub fn new(consumer: Box<dyn MessageConsumer>) -> Self {
        Self {
            consumer: Arc::new(Mutex::new(consumer)),
        }
    }
}

/// How long a lifecycle hook waits for the consumer lock.
///
/// `send` holds the lock across `receive()`, which parks indefinitely on an idle source.
/// An unbounded wait here would let that park block shutdown, so the hook is skipped
/// rather than allowed to hang.
const HOOK_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[async_trait]
impl MessagePublisher for ReaderPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let Ok(consumer) = tokio::time::timeout(HOOK_LOCK_TIMEOUT, self.consumer.lock()).await
            else {
                tracing::warn!("ReaderPublisher: consumer busy, skipping connect hook");
                return Ok(());
            };
            if let Some(hook) = consumer.on_connect_hook() {
                hook.await?;
            }
            Ok(())
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let Ok(consumer) = tokio::time::timeout(HOOK_LOCK_TIMEOUT, self.consumer.lock()).await
            else {
                tracing::warn!("ReaderPublisher: consumer busy, skipping disconnect hook");
                return Ok(());
            };
            if let Some(hook) = consumer.on_disconnect_hook() {
                hook.await?;
            }
            Ok(())
        }))
    }

    async fn send(&self, trigger: CanonicalMessage) -> Result<Sent, PublisherError> {
        let mut consumer = self.consumer.lock().await;
        // We ignore the incoming message payload and just read from the consumer.
        // The incoming message acts purely as a trigger.
        match consumer.receive().await {
            Ok(received) => {
                // We must commit the message immediately because the Publisher interface
                // doesn't support passing the commit responsibility back to the caller
                // in a way that aligns with the input's commit lifecycle.
                if let Err(e) = (received.commit)(MessageDisposition::Ack).await {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "Failed to commit message in ReaderPublisher: {}",
                        e
                    )));
                }
                Ok(Sent::Response(as_reply_to(
                    received.message,
                    trigger.message_id,
                )))
            }
            Err(e) => match e {
                ConsumerError::EndOfStream | ConsumerError::Permanent(_) => {
                    Err(PublisherError::NonRetryable(anyhow::anyhow!(e)))
                }
                _ => Err(PublisherError::Retryable(anyhow::anyhow!(e))),
            },
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let count = messages.len();
        if count == 0 {
            return Ok(SentBatch::Ack);
        }
        let mut consumer = self.consumer.lock().await;
        match consumer.receive_batch(count).await {
            Ok(batch) => {
                let received_count = batch.messages.len();
                if received_count == 0 {
                    return Ok(SentBatch::Partial {
                        responses: None,
                        failed: messages.into_iter().map(nothing_to_read).collect(),
                    });
                }

                // Same reasoning as `send`: the read must be committed here, because the
                // publisher interface cannot hand the inner consumer's commit lifecycle
                // back to the route.
                if let Err(e) = (batch.commit)(vec![MessageDisposition::Ack; received_count]).await
                {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "Failed to commit batch in ReaderPublisher: {}",
                        e
                    )));
                }

                // Surface the read messages as responses, mirroring `send`'s
                // `Sent::Response`, so the route can dispatch them instead of dropping them.
                let mut triggers = messages.into_iter();
                let responses = batch
                    .messages
                    .into_iter()
                    .zip(triggers.by_ref())
                    .map(|(message, trigger)| as_reply_to(message, trigger.message_id))
                    .collect();
                // A trigger the source had no message for is not answered with an empty ack.
                Ok(SentBatch::Partial {
                    responses: Some(responses),
                    failed: triggers.map(nothing_to_read).collect(),
                })
            }
            Err(e) => match e {
                ConsumerError::EndOfStream | ConsumerError::Permanent(_) => {
                    Err(PublisherError::NonRetryable(anyhow::anyhow!(e)))
                }
                _ => Err(PublisherError::Retryable(anyhow::anyhow!(e))),
            },
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn nothing_to_read(trigger: CanonicalMessage) -> (CanonicalMessage, PublisherError) {
    let error = PublisherError::Retryable(anyhow::anyhow!(
        "reader: the source had no message for this trigger"
    ));
    (trigger, error)
}

/// Metadata key holding the id the read message had before it became a reply.
pub const READER_MESSAGE_ID_KEY: &str = "mqb.reader.message_id";

/// A route matches replies to requests by id, so the read message takes the request's
/// id and keeps its own in metadata.
fn as_reply_to(mut message: CanonicalMessage, request_id: u128) -> CanonicalMessage {
    message.metadata.insert(
        READER_MESSAGE_ID_KEY.to_string(),
        format!("{:032x}", message.message_id),
    );
    message.message_id = request_id;
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcomes::{Received, ReceivedBatch};
    use crate::traits::{BatchCommitFunc, CommitFunc, EndpointStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    struct MockConsumer {
        single_result: Option<Result<CanonicalMessage, ConsumerError>>,
        batch_result: Option<Result<Vec<CanonicalMessage>, ConsumerError>>,
        commit_log: StdArc<StdMutex<Vec<Vec<MessageDisposition>>>>,
        commit_error: Option<String>,
        connect_calls: StdArc<AtomicUsize>,
        disconnect_calls: StdArc<AtomicUsize>,
    }

    impl MockConsumer {
        fn new_single(
            result: Result<CanonicalMessage, ConsumerError>,
            commit_log: StdArc<StdMutex<Vec<Vec<MessageDisposition>>>>,
        ) -> Self {
            Self {
                single_result: Some(result),
                batch_result: None,
                commit_log,
                commit_error: None,
                connect_calls: StdArc::new(AtomicUsize::new(0)),
                disconnect_calls: StdArc::new(AtomicUsize::new(0)),
            }
        }

        fn new_batch(
            result: Result<Vec<CanonicalMessage>, ConsumerError>,
            commit_log: StdArc<StdMutex<Vec<Vec<MessageDisposition>>>>,
        ) -> Self {
            Self {
                single_result: None,
                batch_result: Some(result),
                commit_log,
                commit_error: None,
                connect_calls: StdArc::new(AtomicUsize::new(0)),
                disconnect_calls: StdArc::new(AtomicUsize::new(0)),
            }
        }

        fn with_commit_error(mut self, message: &str) -> Self {
            self.commit_error = Some(message.to_string());
            self
        }

        fn commit_func(&self) -> CommitFunc {
            let log = self.commit_log.clone();
            let error = self.commit_error.clone();
            Box::new(move |disposition| {
                let log = log.clone();
                let error = error.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(vec![disposition]);
                    if let Some(message) = error {
                        Err(anyhow::anyhow!(message))
                    } else {
                        Ok(())
                    }
                })
            })
        }

        fn batch_commit_func(&self) -> BatchCommitFunc {
            let log = self.commit_log.clone();
            let error = self.commit_error.clone();
            Box::new(move |dispositions| {
                let log = log.clone();
                let error = error.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(dispositions);
                    if let Some(message) = error {
                        Err(anyhow::anyhow!(message))
                    } else {
                        Ok(())
                    }
                })
            })
        }
    }

    #[async_trait]
    impl MessageConsumer for MockConsumer {
        fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            let calls = self.connect_calls.clone();
            Some(Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        }

        fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
            let calls = self.disconnect_calls.clone();
            Some(Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        }

        async fn receive(&mut self) -> Result<Received, ConsumerError> {
            match self
                .single_result
                .take()
                .expect("single_result should be configured for this test")
            {
                Ok(message) => Ok(Received {
                    message,
                    commit: self.commit_func(),
                }),
                Err(err) => Err(err),
            }
        }

        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            match self
                .batch_result
                .take()
                .expect("batch_result should be configured for this test")
            {
                Ok(messages) => Ok(ReceivedBatch {
                    messages,
                    commit: self.batch_commit_func(),
                }),
                Err(err) => Err(err),
            }
        }

        async fn status(&self) -> EndpointStatus {
            EndpointStatus::default()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn test_reader_publisher_send_returns_response_and_commits_ack() {
        let commit_log = StdArc::new(StdMutex::new(Vec::new()));
        let publisher = ReaderPublisher::new(Box::new(MockConsumer::new_single(
            Ok(CanonicalMessage::from("from-reader")),
            commit_log.clone(),
        )));

        let sent = publisher
            .send(CanonicalMessage::from("trigger"))
            .await
            .unwrap();
        match sent {
            Sent::Response(message) => assert_eq!(message.get_payload_str(), "from-reader"),
            Sent::Ack => panic!("expected response"),
        }

        assert_eq!(commit_log.lock().unwrap().len(), 1);
        assert!(matches!(
            commit_log.lock().unwrap()[0].as_slice(),
            [MessageDisposition::Ack]
        ));
    }

    #[tokio::test]
    async fn test_reader_publisher_send_maps_end_of_stream_to_non_retryable_error() {
        let publisher = ReaderPublisher::new(Box::new(MockConsumer::new_single(
            Err(ConsumerError::EndOfStream),
            StdArc::new(StdMutex::new(Vec::new())),
        )));

        let err = publisher
            .send(CanonicalMessage::from("trigger"))
            .await
            .unwrap_err();
        assert!(matches!(err, PublisherError::NonRetryable(_)));
    }

    #[tokio::test]
    async fn test_reader_publisher_send_batch_commits_all_received_messages() {
        let commit_log = StdArc::new(StdMutex::new(Vec::new()));
        let publisher = ReaderPublisher::new(Box::new(MockConsumer::new_batch(
            Ok(vec![
                CanonicalMessage::from("one"),
                CanonicalMessage::from("two"),
            ]),
            commit_log.clone(),
        )));

        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::from("trigger-1"),
                CanonicalMessage::from("trigger-2"),
            ])
            .await
            .unwrap();
        match sent {
            SentBatch::Partial { responses, failed } => {
                assert!(failed.is_empty());
                let responses = responses.expect("read messages should be returned as responses");
                assert_eq!(
                    responses
                        .iter()
                        .map(|m| m.get_payload_str().to_string())
                        .collect::<Vec<_>>(),
                    ["one", "two"]
                );
            }
            SentBatch::Ack => panic!("expected read messages to be returned as responses"),
        }
        assert_eq!(commit_log.lock().unwrap().len(), 1);
        assert!(commit_log.lock().unwrap()[0]
            .iter()
            .all(|disposition| matches!(disposition, MessageDisposition::Ack)));
    }

    /// STRUCT-05: a trigger without a message fails as retryable instead of being acked empty.
    #[tokio::test]
    async fn test_reader_publisher_send_batch_fails_triggers_without_a_message() {
        for (read, answered) in [(vec![CanonicalMessage::from("one")], 1), (Vec::new(), 0)] {
            let publisher = ReaderPublisher::new(Box::new(MockConsumer::new_batch(
                Ok(read),
                StdArc::new(StdMutex::new(Vec::new())),
            )));
            let triggers = vec![
                CanonicalMessage::from("trigger-1"),
                CanonicalMessage::from("trigger-2"),
            ];
            let unanswered: Vec<u128> = triggers[answered..].iter().map(|m| m.message_id).collect();

            let SentBatch::Partial { responses, failed } =
                publisher.send_batch(triggers).await.unwrap()
            else {
                panic!("a trigger without a message must not be acked");
            };
            assert_eq!(responses.map_or(0, |r| r.len()), answered);
            assert_eq!(
                failed.iter().map(|(m, _)| m.message_id).collect::<Vec<_>>(),
                unanswered
            );
            assert!(failed
                .iter()
                .all(|(_, e)| matches!(e, PublisherError::Retryable(_))));
        }
    }

    #[tokio::test]
    async fn test_reader_publisher_send_batch_commit_failure_is_retryable() {
        let publisher = ReaderPublisher::new(Box::new(
            MockConsumer::new_batch(
                Ok(vec![CanonicalMessage::from("one")]),
                StdArc::new(StdMutex::new(Vec::new())),
            )
            .with_commit_error("commit failed"),
        ));

        let err = publisher
            .send_batch(vec![CanonicalMessage::from("trigger")])
            .await
            .unwrap_err();
        assert!(matches!(err, PublisherError::Retryable(_)));
    }

    #[tokio::test]
    async fn test_reader_publisher_runs_consumer_hooks() {
        let consumer =
            MockConsumer::new_batch(Ok(Vec::new()), StdArc::new(StdMutex::new(Vec::new())));
        let connect_calls = consumer.connect_calls.clone();
        let disconnect_calls = consumer.disconnect_calls.clone();
        let publisher = ReaderPublisher::new(Box::new(consumer));

        publisher
            .on_connect_hook()
            .unwrap()
            .await
            .expect("connect hook should succeed");
        publisher
            .on_disconnect_hook()
            .unwrap()
            .await
            .expect("disconnect hook should succeed");

        assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(disconnect_calls.load(Ordering::SeqCst), 1);
        assert!(publisher.as_any().is::<ReaderPublisher>());
    }
}
