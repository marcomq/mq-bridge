//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::models::MetricsMiddleware;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::time::{Duration, Instant};

/// Metric handles resolved once per wrapped endpoint.
///
/// The `counter!`/`histogram!` macros allocate both label `String`s and re-resolve the
/// registry on every call, which is per *message* on this path. Handles are the
/// crate's intended way to avoid that.
///
/// This resolves against the global recorder at construction, i.e. when the route is
/// built, so the host application must install its recorder before starting routes —
/// otherwise these stay no-ops.
struct Handles {
    processed: metrics::Counter,
    duration: metrics::Histogram,
}

impl Handles {
    fn new(route_name: &str, endpoint_direction: &str) -> Self {
        Self {
            processed: metrics::counter!(
                "queue_messages_processed_total",
                "route" => route_name.to_string(),
                "endpoint" => endpoint_direction.to_string()
            ),
            duration: metrics::histogram!(
                "queue_message_processing_duration_seconds",
                "route" => route_name.to_string(),
                "endpoint" => endpoint_direction.to_string()
            ),
        }
    }

    /// Records `count` messages taking `elapsed` in total; the histogram gets the average.
    fn record(&self, count: u64, elapsed: Duration) {
        self.processed.increment(count);
        self.duration.record(elapsed.as_secs_f64() / count as f64);
    }
}

pub struct MetricsPublisher {
    inner: Box<dyn MessagePublisher>,
    handles: Handles,
}

impl MetricsPublisher {
    pub fn new(
        inner: Box<dyn MessagePublisher>,
        _config: &MetricsMiddleware,
        route_name: &str,
        endpoint_direction: &str,
    ) -> Self {
        Self {
            inner,
            handles: Handles::new(route_name, endpoint_direction),
        }
    }
}

#[async_trait]
impl MessagePublisher for MetricsPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let start = Instant::now();
        let result = self.inner.send(message).await?;
        let duration = start.elapsed();

        self.handles.record(1, duration);

        Ok(result)
    }
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let total_count = messages.len();
        let start = Instant::now();
        let result = self.inner.send_batch(messages).await?;
        let duration = start.elapsed();

        match &result {
            SentBatch::Partial { failed, .. } => {
                let successful_count = total_count - failed.len();
                if successful_count > 0 {
                    self.handles.record(successful_count as u64, duration);
                }
                // We can add a new metric for failures here if desired
            }
            SentBatch::Ack => {
                if total_count > 0 {
                    self.handles.record(total_count as u64, duration);
                }
            }
        }
        Ok(result)
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MetricsConsumer {
    inner: Box<dyn MessageConsumer>,
    handles: Handles,
}

impl MetricsConsumer {
    pub fn new(
        inner: Box<dyn MessageConsumer>,
        _config: &MetricsMiddleware,
        route_name: &str,
        endpoint_direction: &str,
    ) -> Self {
        Self {
            inner,
            handles: Handles::new(route_name, endpoint_direction),
        }
    }
}

#[async_trait]
impl MessageConsumer for MetricsConsumer {
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

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let start = Instant::now();
        let result = self.inner.receive().await?;
        let duration = start.elapsed();

        self.handles.record(1, duration);

        Ok(result)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let start = Instant::now();
        let batch = self.inner.receive_batch(max_messages).await?;
        let duration = start.elapsed();

        if !batch.messages.is_empty() {
            self.handles.record(batch.messages.len() as u64, duration);
        }

        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

    /// Processed-message count recorded for `endpoint`, and how many durations.
    fn recorded(snapshotter: &Snapshotter, endpoint: &str) -> (u64, usize) {
        let mut result = (0, 0);
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let labels: Vec<_> = key.key().labels().map(|l| l.value().to_string()).collect();
            if !labels.iter().any(|label| label == endpoint) {
                continue;
            }
            assert!(labels.iter().any(|label| label == "metrics_route"));
            match value {
                DebugValue::Counter(count) => result.0 = count,
                DebugValue::Histogram(samples) => result.1 = samples.len(),
                DebugValue::Gauge(_) => unreachable!(),
            }
        }
        result
    }

    #[tokio::test]
    async fn the_publisher_counts_each_sent_message_and_one_duration_per_call() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let publisher = metrics::with_local_recorder(&recorder, || {
            let inner = Box::new(MemoryPublisher::new_local("metrics_out", 10));
            MetricsPublisher::new(inner, &MetricsMiddleware {}, "metrics_route", "output")
        });

        publisher.send(CanonicalMessage::from("a")).await.unwrap();
        let batch = vec![CanonicalMessage::from("b"), CanonicalMessage::from("c")];
        publisher.send_batch(batch).await.unwrap();
        publisher.send_batch(Vec::new()).await.unwrap();

        assert_eq!(recorded(&snapshotter, "output"), (3, 2));
        assert!(!publisher.requires_ordered_publish());
    }

    #[tokio::test]
    async fn the_consumer_counts_each_received_message() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let inner = MemoryConsumer::new_local("metrics_in", 10);
        let channel = inner.channel();
        for body in ["a", "b", "c"] {
            channel
                .send_message(CanonicalMessage::from(body))
                .await
                .unwrap();
        }
        let mut consumer = metrics::with_local_recorder(&recorder, || {
            MetricsConsumer::new(
                Box::new(inner),
                &MetricsMiddleware {},
                "metrics_route",
                "input",
            )
        });

        consumer.receive().await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();

        assert_eq!(batch.messages.len(), 2);
        assert_eq!(recorded(&snapshotter, "input"), (3, 2));
    }
}
