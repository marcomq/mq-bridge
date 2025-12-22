//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
use crate::models::MetricsMiddleware;
use crate::traits::{
    ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received, ReceivedBatch,
    SendBatchOutcome, SendOutcome,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::time::Instant;

pub struct MetricsPublisher {
    inner: Box<dyn MessagePublisher>,
    route_name: String,
    endpoint_direction: String,
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
            route_name: route_name.to_string(),
            endpoint_direction: endpoint_direction.to_string(),
        }
    }
}

#[async_trait]
impl MessagePublisher for MetricsPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<SendOutcome, PublisherError> {
        let start = Instant::now();
        let result = self.inner.send(message).await?;
        let duration = start.elapsed();

        metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(1);
        metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(duration.as_secs_f64());

        Ok(result)
    }
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SendBatchOutcome, PublisherError> {
        let total_count = messages.len();
        let start = Instant::now();
        let result = self.inner.send_batch(messages).await?;
        let duration = start.elapsed();

        match &result {
            SendBatchOutcome::Partial { failed, .. } => {
                let successful_count = total_count - failed.len();
                if successful_count > 0 {
                    let avg_duration = duration.as_secs_f64() / successful_count as f64;
                    metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(successful_count as u64);
                    metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(avg_duration);
                }
                // We can add a new metric for failures here if desired
            }
            SendBatchOutcome::Ack => {
                if total_count > 0 {
                    let avg_duration = duration.as_secs_f64() / total_count as f64;
                    metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(total_count as u64);
                    metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(avg_duration);
                }
            }
        }
        Ok(result)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MetricsConsumer {
    inner: Box<dyn MessageConsumer>,
    route_name: String,
    endpoint_direction: String,
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
            route_name: route_name.to_string(),
            endpoint_direction: endpoint_direction.to_string(),
        }
    }
}

#[async_trait]
impl MessageConsumer for MetricsConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let start = Instant::now();
        let result = self.inner.receive().await?;
        let duration = start.elapsed();

        metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(1);
        metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(duration.as_secs_f64());

        Ok(result)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let start = Instant::now();
        let batch = self.inner.receive_batch(max_messages).await?;
        let duration = start.elapsed();

        if !batch.messages.is_empty() {
            let avg_duration = duration.as_secs_f64() / batch.messages.len() as f64;
            metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(batch.messages.len() as u64);
            metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(avg_duration);
        }

        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
