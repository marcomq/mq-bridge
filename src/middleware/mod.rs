//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::models::{Endpoint, Middleware};
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::Result;
use std::sync::Arc;

#[cfg(feature = "dedup")]
mod deduplication;
mod delay;
mod dlq;
#[cfg(feature = "metrics")]
mod metrics;
mod random_panic;
mod retry;

#[cfg(feature = "dedup")]
use deduplication::DeduplicationConsumer;
use delay::{DelayConsumer, DelayPublisher};
use dlq::DlqPublisher;
#[cfg(feature = "metrics")]
use metrics::{MetricsConsumer, MetricsPublisher};
use random_panic::{RandomPanicConsumer, RandomPanicPublisher};
use retry::RetryPublisher;

/// Wraps a `MessageConsumer` with the middlewares specified in the endpoint configuration.
///
/// Middlewares are applied in reverse order of the configuration list.
/// This means the first middleware in the config is the outermost layer, executed first.
pub async fn apply_middlewares_to_consumer(
    mut consumer: Box<dyn MessageConsumer>,
    endpoint: &Endpoint,
    route_name: &str,
) -> Result<Box<dyn MessageConsumer>> {
    for middleware in endpoint.middlewares.iter().rev() {
        consumer = match middleware {
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(cfg) => {
                Box::new(DeduplicationConsumer::new(consumer, cfg, route_name)?)
            }
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsConsumer::new(consumer, cfg, route_name, "input"))
            }
            Middleware::Dlq(_) => consumer, // DLQ is a publisher-only middleware
            Middleware::Retry(_) => consumer, // Retry is currently publisher-only
            Middleware::CommitConcurrency(_) => consumer, // Configuration only, read by Route
            Middleware::Delay(cfg) => Box::new(DelayConsumer::new(consumer, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicConsumer::new(consumer, cfg)),
            Middleware::Custom(factory) => factory.apply_consumer(consumer, route_name).await?,
            #[allow(unreachable_patterns)]
            _ => {
                return Err(anyhow::anyhow!(
                    "[middleware:{}] Unsupported consumer middleware",
                    route_name
                ))
            }
        };
    }
    Ok(consumer)
}

/// Wraps a `MessagePublisher` with the middlewares specified in the endpoint configuration.
///
/// Middlewares are applied in the order of the configuration list.
/// This means the first middleware in the config is the outermost layer, executed first.
pub async fn apply_middlewares_to_publisher(
    mut publisher: Box<dyn MessagePublisher>,
    endpoint: &Endpoint,
    route_name: &str,
) -> Result<Arc<dyn MessagePublisher>> {
    for middleware in &endpoint.middlewares {
        publisher = match middleware {
            Middleware::Dlq(cfg) => Box::new(DlqPublisher::new(publisher, cfg, route_name).await?),
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsPublisher::new(publisher, cfg, route_name, "output"))
            }
            // This middleware is consumer-only
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(_) => publisher,
            Middleware::CommitConcurrency(_) => {
                tracing::warn!("CommitConcurrency middleware is ignored on publishers (output endpoints). It should be configured on the input endpoint.");
                publisher
            }
            Middleware::Retry(cfg) => Box::new(RetryPublisher::new(publisher, cfg.clone())),
            Middleware::Delay(cfg) => Box::new(DelayPublisher::new(publisher, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicPublisher::new(publisher, cfg)),
            Middleware::Custom(factory) => factory.apply_publisher(publisher, route_name).await?,
            #[allow(unreachable_patterns)]
            _ => {
                return Err(anyhow::anyhow!(
                    "[middleware:{}] Unsupported publisher middleware",
                    route_name
                ))
            }
        };
    }
    Ok(publisher.into())
}
