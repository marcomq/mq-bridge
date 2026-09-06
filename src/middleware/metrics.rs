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
