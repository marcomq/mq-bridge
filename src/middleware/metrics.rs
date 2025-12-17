//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue
use crate::models::MetricsMiddleware;
use crate::traits::MessagePublisher;
use crate::traits::{BatchCommitFunc, CommitFunc, MessageConsumer};
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
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let start = Instant::now();
        let result = self.inner.send(message).await;
        let duration = start.elapsed();

        if result.is_ok() {
            metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(1);
            metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(duration.as_secs_f64());
        }

        result
    }
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Option<Vec<CanonicalMessage>>, Vec<CanonicalMessage>)> {
        let total_count = messages.len();
        let start = Instant::now();
        let result = self.inner.send_batch(messages).await;
        let duration = start.elapsed();

        match &result {
            Ok((_, failed)) => {
                let successful_count = total_count - failed.len();
                if successful_count > 0 {
                    let avg_duration = duration.as_secs_f64() / successful_count as f64;
                    metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(successful_count as u64);
                    metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(avg_duration);
                }
                // We can add a new metric for failures here if desired
            }
            Err(_) => {
                // On a total failure, we could increment an error counter for the whole batch
                // For now, we just don't record success, which is implicitly correct.
            }
        }
        result
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
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        let start = Instant::now();
        let result = self.inner.receive().await;
        let duration = start.elapsed();

        if result.is_ok() {
            metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(1);
            metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(duration.as_secs_f64());
        }

        result
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let start = Instant::now();
        let result = self.inner.receive_batch(max_messages).await;
        let duration = start.elapsed();

        if let Ok((messages, _)) = &result {
            if !messages.is_empty() {
                let avg_duration = duration.as_secs_f64() / messages.len() as f64;
                metrics::counter!("queue_messages_processed_total", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).increment(messages.len() as u64);
                metrics::histogram!("queue_message_processing_duration_seconds", "route" => self.route_name.clone(), "endpoint" => self.endpoint_direction.clone()).record(avg_duration);
            }
        }

        result
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
