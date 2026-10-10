//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::errors::InvalidConfig;
use crate::extensions::get_middleware_factory;
use crate::models::{Endpoint, Middleware};
use crate::traits::CustomMiddlewareFactory;
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static REJECTED_INPUT_MESSAGES: AtomicU64 = AtomicU64::new(0);

/// Input messages a middleware could not decode and acked (`on_error: drop`), process-wide.
/// A one-shot job reads it to tell a lossy run from a clean one.
pub fn rejected_input_messages() -> u64 {
    REJECTED_INPUT_MESSAGES.load(Ordering::Relaxed)
}

static DEAD_LETTERED_MESSAGES: AtomicU64 = AtomicU64::new(0);

/// Messages a `dlq` middleware delivered to its dead-letter target, process-wide.
pub fn dead_lettered_messages() -> u64 {
    DEAD_LETTERED_MESSAGES.load(Ordering::Relaxed)
}

pub(crate) fn note_dead_lettered(count: usize) {
    DEAD_LETTERED_MESSAGES.fetch_add(count as u64, Ordering::Relaxed);
}

#[allow(dead_code)]
pub(crate) fn note_rejected_input_message() {
    REJECTED_INPUT_MESSAGES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "aggregate")]
pub(crate) mod aggregate;
mod buffer;
#[cfg(feature = "compression")]
pub(crate) mod compression;
mod cookie_jar;
#[cfg(feature = "dedup")]
pub(crate) mod deduplication;
#[cfg(any(feature = "filter", feature = "dedup"))]
mod deferred_commit;
mod delay;
mod dlq;
#[cfg(feature = "encryption")]
pub(crate) mod encryption;
#[cfg(feature = "filter")]
pub(crate) mod filter;
mod id;
mod limiter;
mod lookup;
#[cfg(feature = "metrics")]
mod metrics;
#[cfg(feature = "otel")]
mod otel;
mod pack;
mod random_panic;
mod raw_json;
mod retry;
mod timeout;
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
use pack::{PackPublisher, UnpackConsumer};
use random_panic::{RandomPanicConsumer, RandomPanicPublisher};
use retry::RetryPublisher;
use timeout::TimeoutPublisher;
use transform::{TransformConsumer, TransformPublisher};
use weak_join::WeakJoinConsumer;

/// Leaves the consumer unwrapped unless the host installed a tracer provider, so an unused
/// `otel` middleware costs nothing per message.
fn otel_consumer(
    consumer: Box<dyn MessageConsumer>,
    route_name: &str,
) -> Result<Box<dyn MessageConsumer>> {
    #[cfg(feature = "otel")]
    if otel::tracer_installed() {
        return Ok(Box::new(otel::OtelConsumer::new(consumer, route_name)));
    }
    otel_inactive(route_name)?;
    Ok(consumer)
}

fn otel_publisher(
    publisher: Box<dyn MessagePublisher>,
    route_name: &str,
) -> Result<Box<dyn MessagePublisher>> {
    #[cfg(feature = "otel")]
    if otel::tracer_installed() {
        return Ok(Box::new(otel::OtelPublisher::new(publisher, route_name)));
    }
    otel_inactive(route_name)?;
    Ok(publisher)
}

fn otel_inactive(route_name: &str) -> Result<()> {
    if cfg!(feature = "otel") {
        tracing::debug!(
            "[middleware:{route_name}] no OpenTelemetry tracer installed; otel middleware inactive"
        );
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "[middleware:{route_name}] the otel middleware requires the 'otel' feature"
        ))
    }
}

/// A middleware config that cannot work stops the route instead of reconnecting.
fn invalid<T>(result: Result<T>) -> Result<T> {
    result.map_err(|e| InvalidConfig(e).into())
}

/// Wraps a `MessageConsumer` with the middlewares specified in the endpoint configuration.
///
/// Middlewares are applied in reverse order of the configuration list.
/// This means the first middleware in the config is the outermost layer: a received message
/// passes the last entry first and the first entry last.
pub async fn apply_middlewares_to_consumer(
    mut consumer: Box<dyn MessageConsumer>,
    endpoint: &Endpoint,
    route_name: &str,
) -> Result<Box<dyn MessageConsumer>> {
    for middleware in endpoint.middlewares.iter().rev() {
        consumer = match middleware {
            Middleware::Id(template) => Box::new(invalid(IdConsumer::new(consumer, template))?),
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(cfg) => {
                Box::new(DeduplicationConsumer::new(consumer, cfg, route_name).await?)
            }
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsConsumer::new(consumer, cfg, route_name, "input"))
            }
            Middleware::Otel(_) => otel_consumer(consumer, route_name)?,
            // Output-only. Accepting them here used to warn and do nothing, which left a
            // route without the retries or dead-lettering its config asked for.
            Middleware::Dlq(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `dlq` is an output-only middleware and does nothing on an input endpoint. Move it to the route's output endpoint."
                )).into())
            }
            Middleware::Retry(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `retry` is an output-only middleware and does nothing on an input endpoint. Move it to the route's output endpoint."
                )).into())
            }
            Middleware::Delay(cfg) => Box::new(DelayConsumer::new(consumer, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicConsumer::new(consumer, cfg)),
            Middleware::WeakJoin(cfg) => Box::new(WeakJoinConsumer::new(consumer, cfg)),
            Middleware::Limiter(cfg) => Box::new(invalid(LimiterConsumer::new(consumer, cfg))?),
            Middleware::Buffer(cfg) => Box::new(invalid(BufferConsumer::new(consumer, cfg))?),
            Middleware::CookieJar(cfg) => Box::new(CookieJarConsumer::new(consumer, cfg)),
            Middleware::Transform(cfg) => Box::new(invalid(TransformConsumer::new(consumer, cfg))?),
            #[cfg(feature = "encryption")]
            Middleware::Encryption(cfg) => Box::new(invalid(EncryptionConsumer::new(consumer, cfg))?),
            #[cfg(feature = "compression")]
            Middleware::Compression(cfg) => Box::new(CompressionConsumer::new(consumer, cfg)),
            Middleware::Unpack(cfg) => Box::new(UnpackConsumer::new(consumer, cfg)),
            // Output-only: packing is what a transport writes, not what it reads.
            Middleware::Pack(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `pack` is an output-only middleware. Put `pack` on the route's output endpoint and `unpack` on its input."
                )).into())
            }
            Middleware::Lookup(cfg) => {
                Box::new(lookup::LookupConsumer::new(consumer, cfg, route_name).await?)
            }
            #[cfg(feature = "aggregate")]
            Middleware::Aggregate(cfg) => Box::new(aggregate::AggregateConsumer::new(consumer, cfg, route_name).await?),
            Middleware::Timeout(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `timeout` bounds sends and is output-only. Move it to the route's output endpoint."
                )).into())
            }
            #[cfg(feature = "filter")]
            Middleware::Filter(cfg) => Box::new(invalid(FilterConsumer::new(consumer, &cfg.expression))?.with_on_error(cfg.on_error)),
            Middleware::Custom { name, config } => {
                let factory = custom_middleware_factory(name)?;
                factory.apply_consumer(consumer, route_name, config).await?
            }
            #[allow(unreachable_patterns)]
            other => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{}] Unsupported consumer middleware: {}",
                    route_name,
                    missing_feature(other)
                )).into())
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
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `id` is a consumer-only middleware and does nothing on an output endpoint. Move it to the route's input endpoint."
                )).into())
            }
            Middleware::Dlq(cfg) => Box::new(DlqPublisher::new(publisher, cfg, route_name).await?),
            Middleware::Otel(_) => otel_publisher(publisher, route_name)?,
            Middleware::Lookup(cfg) => {
                Box::new(lookup::LookupPublisher::new(publisher, cfg, route_name).await?)
            }
            #[cfg(feature = "aggregate")]
            Middleware::Aggregate(cfg) => Box::new(aggregate::AggregatePublisher::new(publisher, cfg, route_name).await?),
            #[cfg(feature = "metrics")]
            Middleware::Metrics(cfg) => {
                Box::new(MetricsPublisher::new(publisher, cfg, route_name, "output"))
            }
            // Consumer-only. Accepting it here used to warn and do nothing, which silently
            // left a route un-deduplicated; `weak_join` already fails fast the same way.
            #[cfg(feature = "dedup")]
            Middleware::Deduplication(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] deduplication is a consumer-only middleware and does nothing on an output endpoint. Move it to the route's input endpoint."
                )).into())
            }
            Middleware::Retry(cfg) => Box::new(RetryPublisher::new(publisher, cfg.clone())),
            Middleware::Delay(cfg) => Box::new(DelayPublisher::new(publisher, cfg)),
            Middleware::Timeout(cfg) => Box::new(TimeoutPublisher::new(publisher, cfg)),
            Middleware::RandomPanic(cfg) => Box::new(RandomPanicPublisher::new(publisher, cfg)),
            Middleware::Limiter(cfg) => Box::new(invalid(LimiterPublisher::new(publisher, cfg))?),
            Middleware::Buffer(cfg) => Box::new(invalid(BufferPublisher::new(publisher, cfg))?),
            Middleware::CookieJar(cfg) => Box::new(CookieJarPublisher::new(publisher, cfg)),
            Middleware::Transform(cfg) => Box::new(invalid(TransformPublisher::new(publisher, cfg))?),
            #[cfg(feature = "encryption")]
            Middleware::Encryption(cfg) => Box::new(invalid(EncryptionPublisher::new(publisher, cfg))?),
            #[cfg(feature = "compression")]
            Middleware::Compression(cfg) => Box::new(CompressionPublisher::new(publisher, cfg)),
            Middleware::Pack(cfg) => Box::new(invalid(PackPublisher::new(publisher, cfg))?),
            // Input-only: unpacking is what a transport reads, not what it writes.
            Middleware::Unpack(_) => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{route_name}] `unpack` is an input-only middleware. Put `unpack` on the route's input endpoint and `pack` on its output."
                )).into())
            }
            #[cfg(feature = "filter")]
            Middleware::Filter(cfg) => Box::new(invalid(FilterPublisher::new(publisher, &cfg.expression))?.with_on_error(cfg.on_error)),
            Middleware::Custom { name, config } => {
                let factory = custom_middleware_factory(name)?;
                factory
                    .apply_publisher(publisher, route_name, config)
                    .await?
            }
            #[allow(unreachable_patterns)]
            other => {
                return Err(InvalidConfig(anyhow::anyhow!(
                    "[middleware:{}] Unsupported publisher middleware: {}",
                    route_name,
                    missing_feature(other)
                )).into())
            }
        };
    }
    Ok(publisher.into())
}

/// Says which Cargo feature a middleware this build cannot apply needs.
#[allow(dead_code)]
fn missing_feature(middleware: &Middleware) -> String {
    let (name, feature) = match middleware {
        Middleware::Deduplication(_) => ("deduplication", "dedup"),
        Middleware::Metrics(_) => ("metrics", "metrics"),
        Middleware::Aggregate(_) => ("aggregate", "aggregate"),
        Middleware::Encryption(_) => ("encryption", "encryption"),
        Middleware::Compression(_) => ("compression", "compression"),
        Middleware::Filter(_) => ("filter", "filter"),
        _ => return "not available in this build".to_string(),
    };
    format!("`{name}` needs the `{feature}` Cargo feature, which this build does not include")
}

/// Resolves a `custom` middleware name, loading an installed plugin that
/// provides it when no factory is registered under the name yet.
fn custom_middleware_factory(name: &str) -> Result<Arc<dyn CustomMiddlewareFactory>> {
    if let Some(factory) = get_middleware_factory(name) {
        return Ok(factory);
    }
    #[cfg(feature = "plugin")]
    if crate::plugin::discover_middleware_plugin(name)?.is_some() {
        if let Some(factory) = get_middleware_factory(name) {
            return Ok(factory);
        }
    }
    #[cfg(feature = "plugin")]
    let hint = format!(": {}", crate::plugin::search_path_hint(name));
    #[cfg(not(feature = "plugin"))]
    let hint = String::new();
    Err(anyhow::anyhow!(
        "Custom middleware factory '{name}' not found{hint}"
    ))
}

/// Puts the original payload back into each failed message. `originals` holds
/// `(message_id, original, rewritten)` per sent message; messages sharing an id are
/// told apart by the rewritten payload the failure still carries.
#[cfg(any(feature = "compression", feature = "encryption"))]
pub(crate) fn restore_payloads(
    originals: Vec<(u128, bytes::Bytes, bytes::Bytes)>,
    failed: &mut [(crate::CanonicalMessage, crate::traits::PublisherError)],
) {
    let mut by_id: std::collections::HashMap<u128, Vec<(bytes::Bytes, bytes::Bytes)>> =
        std::collections::HashMap::with_capacity(originals.len());
    for (id, original, rewritten) in originals {
        by_id.entry(id).or_default().push((original, rewritten));
    }
    for (message, _) in failed {
        let Some(candidates) = by_id.get(&message.message_id) else {
            continue;
        };
        let found = candidates
            .iter()
            .find(|(_, rewritten)| candidates.len() == 1 || *rewritten == message.payload);
        if let Some((original, _)) = found {
            message.payload = original.clone();
        }
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::endpoints::memory::{MemoryConsumer, MemoryPublisher};
    use crate::models::EndpointType;

    fn endpoint_with(middleware: serde_json::Value) -> Endpoint {
        let mut endpoint = Endpoint::new(EndpointType::Null);
        endpoint.middlewares = vec![serde_json::from_value(middleware).unwrap()];
        endpoint
    }

    async fn consumer_error(middleware: serde_json::Value) -> anyhow::Error {
        let consumer = Box::new(MemoryConsumer::new_local("placement_in", 1));
        apply_middlewares_to_consumer(consumer, &endpoint_with(middleware), "placement")
            .await
            .err()
            .expect("an output-only middleware on an input must be refused")
    }

    async fn publisher_error(middleware: serde_json::Value) -> anyhow::Error {
        let publisher = Box::new(MemoryPublisher::new_local("placement_out", 1));
        apply_middlewares_to_publisher(publisher, &endpoint_with(middleware), "placement")
            .await
            .err()
            .expect("an input-only middleware on an output must be refused")
    }

    #[tokio::test]
    async fn output_only_middleware_on_an_input_is_an_invalid_config() {
        for (middleware, hint) in [
            (serde_json::json!({"pack": {}}), "output-only"),
            (
                serde_json::json!({"timeout": {"timeout_ms": 5}}),
                "output-only",
            ),
            (
                serde_json::json!({"retry": {"max_attempts": 3}}),
                "output-only",
            ),
            (
                serde_json::json!({"dlq": {"endpoint": {"null": null}}}),
                "output-only",
            ),
        ] {
            let err = consumer_error(middleware).await;
            assert!(err.downcast_ref::<InvalidConfig>().is_some(), "{err}");
            assert!(err.to_string().contains(hint), "{err}");
        }
    }

    #[tokio::test]
    async fn input_only_middleware_on_an_output_is_an_invalid_config() {
        for (middleware, hint) in [
            (serde_json::json!({"unpack": {}}), "input-only"),
            (
                serde_json::json!({"id": "${payload:order_id}"}),
                "consumer-only",
            ),
        ] {
            let err = publisher_error(middleware).await;
            assert!(err.downcast_ref::<InvalidConfig>().is_some(), "{err}");
            assert!(err.to_string().contains(hint), "{err}");
        }
    }

    /// Middlewares that sit on either side and leave the payload as it is.
    fn pass_through() -> Vec<Middleware> {
        let mut list = vec![
            serde_json::json!({"delay": {"delay_ms": 1}}),
            serde_json::json!({"random_panic": {"enabled": false}}),
            serde_json::json!({"limiter": {"messages_per_second": 100000.0}}),
            serde_json::json!({"cookie_jar": {}}),
        ];
        if cfg!(feature = "otel") {
            list.push(serde_json::json!({"otel": {}}));
        }
        if cfg!(feature = "metrics") {
            list.push(serde_json::json!({"metrics": {}}));
        }
        list.into_iter()
            .map(|middleware| serde_json::from_value(middleware).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_message_passes_every_two_sided_middleware_on_both_sides() {
        let mut endpoint = Endpoint::new(EndpointType::Null);
        endpoint.middlewares = pass_through();

        let inner = MemoryConsumer::new_local("wiring_in", 4);
        inner
            .channel()
            .send_message(crate::CanonicalMessage::from("in"))
            .await
            .unwrap();
        let mut consumer = apply_middlewares_to_consumer(Box::new(inner), &endpoint, "wiring")
            .await
            .unwrap();
        assert_eq!(
            &consumer.receive().await.unwrap().message.payload[..],
            b"in"
        );

        endpoint.middlewares.extend([
            serde_json::from_value(serde_json::json!({"retry": {"max_attempts": 2}})).unwrap(),
            serde_json::from_value(serde_json::json!({"timeout": {"timeout_ms": 5000}})).unwrap(),
        ]);
        let inner = MemoryPublisher::new_local("wiring_out", 4);
        let channel = inner.channel();
        let publisher = apply_middlewares_to_publisher(Box::new(inner), &endpoint, "wiring")
            .await
            .unwrap();
        publisher
            .send(crate::CanonicalMessage::from("out"))
            .await
            .unwrap();
        let sent = channel.drain_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0].payload[..], b"out");
    }

    #[tokio::test]
    async fn otel_without_the_feature_is_refused_by_name() {
        if cfg!(feature = "otel") {
            return;
        }
        let err = publisher_error(serde_json::json!({"otel": {}})).await;
        assert!(
            err.to_string().contains("requires the 'otel' feature"),
            "{err}"
        );
        let err = consumer_error(serde_json::json!({"otel": {}})).await;
        assert!(
            err.to_string().contains("requires the 'otel' feature"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_unknown_custom_middleware_is_named_on_both_sides() {
        let custom = serde_json::json!({"custom": {"name": "no_such_middleware", "config": {}}});
        for err in [
            consumer_error(custom.clone()).await,
            publisher_error(custom).await,
        ] {
            let text = err.to_string();
            assert!(text.contains("'no_such_middleware' not found"), "{text}");
        }
    }
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
            replay_response: false,
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
