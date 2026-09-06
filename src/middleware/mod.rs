//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::extensions::get_middleware_factory;
use crate::models::{Endpoint, Middleware};
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::Result;
use std::sync::Arc;

mod buffer;
#[cfg(feature = "compression")]
mod compression;
mod cookie_jar;
#[cfg(feature = "dedup")]
pub(crate) mod deduplication;
#[cfg(any(feature = "filter", feature = "dedup"))]
mod deferred_commit;
mod delay;
mod dlq;
#[cfg(feature = "encryption")]
mod encryption;
#[cfg(feature = "filter")]
pub(crate) mod filter;
mod id;
mod limiter;
#[cfg(feature = "metrics")]
mod metrics;
mod random_panic;
mod raw_json;
mod retry;
pub(crate) mod transform;
mod weak_join;

use buffer::{BufferConsumer, BufferPublisher};
#[cfg(feature = "compression")]
use compression::{CompressionConsumer, CompressionPublisher};
use cookie_jar::{CookieJarConsumer, CookieJarPublisher};
#[cfg(feature = "dedup")]
use deduplication::DeduplicationConsumer;
use delay::{DelayConsumer, DelayPublisher};
use dlq::DlqPublisher;
#[cfg(feature = "encryption")]
use encryption::{EncryptionConsumer, EncryptionPublisher};
#[cfg(feature = "filter")]
use filter::{FilterConsumer, FilterPublisher};
use id::IdConsumer;
use limiter::{LimiterConsumer, LimiterPublisher};
#[cfg(feature = "metrics")]
use metrics::{MetricsConsumer, MetricsPublisher};
use random_panic::{RandomPanicConsumer, RandomPanicPublisher};
use retry::RetryPublisher;
use transform::{TransformConsumer, TransformPublisher};
use weak_join::WeakJoinConsumer;

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
            Middleware::Id(template) => Box::new(IdConsumer::new(consumer, template)?),
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(cfg) => {
                Box::new(DeduplicationConsumer::new(consumer, cfg, route_name).await?)
            }
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsConsumer::new(consumer, cfg, route_name, "input"))
            }
            Middleware::Dlq(_) => {
                tracing::warn!("Dlq middleware is ignored on consumers (input endpoints). It is currently publisher-only.");
                consumer
            }
            Middleware::Retry(_) => {
                tracing::warn!("Retry middleware is ignored on consumers (input endpoints). It is currently publisher-only.");
                consumer
            }
            Middleware::Delay(cfg) => Box::new(DelayConsumer::new(consumer, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicConsumer::new(consumer, cfg)),
            Middleware::WeakJoin(cfg) => Box::new(WeakJoinConsumer::new(consumer, cfg)),
            Middleware::Limiter(cfg) => Box::new(LimiterConsumer::new(consumer, cfg)?),
            Middleware::Buffer(cfg) => Box::new(BufferConsumer::new(consumer, cfg)?),
            Middleware::CookieJar(cfg) => Box::new(CookieJarConsumer::new(consumer, cfg)),
            Middleware::Transform(cfg) => Box::new(TransformConsumer::new(consumer, cfg)?),
            #[cfg(feature = "encryption")]
            Middleware::Encryption(cfg) => Box::new(EncryptionConsumer::new(consumer, cfg)?),
            #[cfg(feature = "compression")]
            Middleware::Compression(cfg) => Box::new(CompressionConsumer::new(consumer, cfg)),
            #[cfg(feature = "filter")]
            Middleware::Filter(expression) => Box::new(FilterConsumer::new(consumer, expression)?),
            Middleware::Custom { name, config } => {
                let factory = get_middleware_factory(name).ok_or_else(|| {
                    anyhow::anyhow!("Custom middleware factory '{}' not found", name)
                })?;
                factory.apply_consumer(consumer, route_name, config).await?
            }
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
/// The list is walked front to back, each entry wrapping the publisher built so far. This
/// means the **last** middleware in the config is the outermost layer and runs first on an
/// outgoing message — the opposite of [`apply_middlewares_to_consumer`], which iterates in
/// reverse so its *first* entry is outermost.
///
/// Practically: a middleware must be listed **after** the ones whose failures it should see.
/// `retry` then `dlq` gives "retry the send, dead-letter it once attempts are exhausted":
///
/// ```yaml
/// middlewares:
///   - retry: { max_attempts: 3 }
///   - dlq: { endpoint: { file: { path: "failed.jsonl" } } }
/// ```
///
/// Reversing those two would put `retry` outside `dlq`, so the DLQ would never see an
/// exhausted-retry failure. See `docs/REFERENCE.md` for the full ordering rules.
///
/// A route handler is wrapped *around* the result of this function (see `wrap_handler` in
/// `endpoints/mod.rs`), so it runs once per message and nothing here re-invokes it.
pub async fn apply_middlewares_to_publisher(
    mut publisher: Box<dyn MessagePublisher>,
    endpoint: &Endpoint,
    route_name: &str,
) -> Result<Arc<dyn MessagePublisher>> {
    for middleware in &endpoint.middlewares {
        publisher = match middleware {
            // Consumer-only: identity is derived where a record enters the pipeline, so that
            // everything downstream — dedup, sink keying, handlers — sees the same value.
            Middleware::Id(_) => {
                return Err(anyhow::anyhow!(
                    "[middleware:{route_name}] `id` is a consumer-only middleware and does nothing on an output endpoint. Move it to the route's input endpoint."
                ))
            }
            Middleware::Dlq(cfg) => Box::new(DlqPublisher::new(publisher, cfg, route_name).await?),
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsPublisher::new(publisher, cfg, route_name, "output"))
            }
            // Consumer-only. Accepting it here used to warn and do nothing, which silently
            // left a route un-deduplicated; `weak_join` already fails fast the same way.
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(_) => {
                return Err(anyhow::anyhow!(
                    "[middleware:{route_name}] deduplication is a consumer-only middleware and does nothing on an output endpoint. Move it to the route's input endpoint."
                ))
            }
            Middleware::Retry(cfg) => Box::new(RetryPublisher::new(publisher, cfg.clone())),
            Middleware::Delay(cfg) => Box::new(DelayPublisher::new(publisher, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicPublisher::new(publisher, cfg)),
            Middleware::Limiter(cfg) => Box::new(LimiterPublisher::new(publisher, cfg)?),
            Middleware::Buffer(cfg) => Box::new(BufferPublisher::new(publisher, cfg)?),
            Middleware::CookieJar(cfg) => Box::new(CookieJarPublisher::new(publisher, cfg)),
            Middleware::Transform(cfg) => Box::new(TransformPublisher::new(publisher, cfg)?),
            #[cfg(feature = "encryption")]
            Middleware::Encryption(cfg) => Box::new(EncryptionPublisher::new(publisher, cfg)?),
            #[cfg(feature = "compression")]
            Middleware::Compression(cfg) => Box::new(CompressionPublisher::new(publisher, cfg)),
            #[cfg(feature = "filter")]
            Middleware::Filter(expression) => Box::new(FilterPublisher::new(publisher, expression)?),
            Middleware::Custom { name, config } => {
                let factory = get_middleware_factory(name).ok_or_else(|| {
                    anyhow::anyhow!("Custom middleware factory '{}' not found", name)
                })?;
                factory
                    .apply_publisher(publisher, route_name, config)
                    .await?
            }
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

#[cfg(all(test, feature = "dedup"))]
mod tests {
    use super::*;
    use crate::models::{DeduplicationMiddleware, EndpointType};

    /// Deduplication cannot work on the publish side. It used to warn and no-op, which left a
    /// route running with no deduplication at all and no way to notice.
    #[tokio::test]
    async fn deduplication_on_an_output_endpoint_fails_fast() {
        let mut endpoint = Endpoint::new(EndpointType::Null);
        endpoint.middlewares = vec![Middleware::Deduplication(DeduplicationMiddleware {
            store: None,
            sled_path: None,
            ttl_seconds: 60,
            key: None,
        })];

        let result = apply_middlewares_to_publisher(
            Box::new(crate::endpoints::structural::null::NullPublisher),
            &endpoint,
            "dedup_output",
        )
        .await;
        let err = match result {
            Ok(_) => panic!("deduplication on an output must not be silently accepted"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("consumer-only"),
            "error should say why, got {err}"
        );
    }
}
