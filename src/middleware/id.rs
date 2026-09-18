//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::canonical_message::MESSAGE_IDENTITY_KEY;
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, Received, ReceivedBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use std::any::Any;
use tracing::{debug, warn};

/// Stamps a rendered business identity into the `mqb.id` metadata key.
///
/// Sources that carry no stable id of their own (files, object stores, ClickHouse, MQTT, SQS)
/// mint a fresh `message_id` per read, which identifies the *delivery* rather than the record.
/// This middleware derives an identity from the message itself instead, so a re-read of the same
/// record produces the same key.
pub struct IdConsumer {
    inner: Box<dyn MessageConsumer>,
    template: CompiledTemplate,
    /// A source missing the field misses it for every record, so the warning is logged once
    /// and the rest are demoted to `debug` rather than flooding a million-row backfill.
    warned_unresolved: bool,
}

impl IdConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, template: &str) -> anyhow::Result<Self> {
        let template = CompiledTemplate::compile(template, None)
            .context("invalid `id` middleware template")?;
        if !template.is_dynamic() {
            anyhow::bail!(
                "`id` middleware template has no `${{namespace:selector}}` token, so every message would be given the same identity"
            );
        }
        if !template.has_only_replay_stable_tokens() {
            anyhow::bail!(
                "`id` middleware template may only use replay-stable `payload` or `metadata` tokens"
            );
        }
        Ok(Self {
            inner,
            template,
            warned_unresolved: false,
        })
    }

    /// Render the template and store it. A template is only an identity if *every* token
    /// resolves: a partial render gives each message missing that field the same value —
    /// worse than none at all — so the key is left unset and the message passes untouched.
    fn stamp(&mut self, message: &mut CanonicalMessage) {
        match self.template.render_resolved(Some(message)) {
            Some(rendered) if !rendered.is_empty() => {
                // Every segment is built from a `String`, so the render is always valid UTF-8.
                let id = String::from_utf8(rendered)
                    .expect("interpolation renders UTF-8 by construction");
                message
                    .metadata
                    .insert(MESSAGE_IDENTITY_KEY.to_string(), id);
            }
            _ => {
                message.metadata.remove(MESSAGE_IDENTITY_KEY);
                self.warn_unresolved(message);
            }
        }
    }

    fn warn_unresolved(&mut self, message: &CanonicalMessage) {
        let message_id = format!("{:032x}", message.message_id);
        if self.warned_unresolved {
            debug!(%message_id, "`id` template resolved to nothing; {MESSAGE_IDENTITY_KEY} unset");
        } else {
            self.warned_unresolved = true;
            warn!(
                %message_id,
                "`id` template resolved to nothing; leaving {MESSAGE_IDENTITY_KEY} unset (further occurrences at debug level)"
            );
        }
    }
}

#[async_trait]
impl MessageConsumer for IdConsumer {
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
        let mut received = self.inner.receive().await?;
        self.stamp(&mut received.message);
        Ok(received)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        for message in batch.messages.iter_mut() {
            self.stamp(message);
        }
        Ok(batch)
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.inner.close().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryConsumer;
    use crate::models::MemoryConfig;
    use crate::traits::MessagePublisher;

    async fn consumer_over(payloads: &[&str], template: &str) -> IdConsumer {
        let config = MemoryConfig::new(format!("id_mw_{}", fast_uuid_v7::gen_id()), None);
        let publisher = crate::endpoints::memory::MemoryPublisher::new(&config).unwrap();
        for payload in payloads {
            publisher
                .send(CanonicalMessage::from(*payload))
                .await
                .unwrap();
        }
        let inner = MemoryConsumer::new(&config).unwrap();
        IdConsumer::new(Box::new(inner), template).unwrap()
    }

    #[tokio::test]
    async fn renders_a_payload_field_into_the_identity_key() {
        let mut consumer = consumer_over(&[r#"{"order_id":"A-1"}"#], "${payload:order_id}").await;
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(
            batch.messages[0].metadata.get(MESSAGE_IDENTITY_KEY),
            Some(&"A-1".to_string())
        );
    }

    /// The whole point is stability across re-reads: the same record must produce the same key
    /// even though each read mints a fresh `message_id`.
    #[tokio::test]
    async fn the_same_record_yields_the_same_identity_on_every_read() {
        let payload = r#"{"order_id":"A-1"}"#;
        let mut consumer = consumer_over(&[payload, payload], "${payload:order_id}").await;
        let first = consumer.receive().await.unwrap().message;
        let second = consumer.receive().await.unwrap().message;
        assert_ne!(
            first.message_id, second.message_id,
            "message_id is per-read, so this test would prove nothing if they matched"
        );
        assert_eq!(
            first.metadata.get(MESSAGE_IDENTITY_KEY),
            second.metadata.get(MESSAGE_IDENTITY_KEY)
        );
    }

    /// An empty render would collapse every message missing the field onto one identity.
    #[tokio::test]
    async fn an_unresolvable_template_leaves_the_key_unset() {
        let mut consumer = consumer_over(&[r#"{"other":"x"}"#], "${payload:order_id}").await;
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch.messages[0].metadata.get(MESSAGE_IDENTITY_KEY), None);
    }

    /// The dangerous case: one token resolves and the other does not, so the render is
    /// non-empty. Checking only for emptiness would stamp `"acme-"` on every message missing
    /// `order_id`, handing them all one identity.
    #[tokio::test]
    async fn a_partially_resolvable_template_leaves_the_key_unset() {
        let mut consumer = consumer_over(
            &[r#"{"tenant":"acme"}"#],
            "${payload:tenant}-${payload:order_id}",
        )
        .await;
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch.messages[0].metadata.get(MESSAGE_IDENTITY_KEY), None);
    }

    /// Same trap with a literal prefix instead of a second token.
    #[tokio::test]
    async fn a_literal_prefix_does_not_count_as_a_resolved_identity() {
        let mut consumer = consumer_over(&[r#"{"other":"x"}"#], "order-${payload:order_id}").await;
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch.messages[0].metadata.get(MESSAGE_IDENTITY_KEY), None);
    }

    /// Consumer middlewares are wrapped in reverse, so the **last** entry sits closest to the
    /// source and runs first. Anything reading `mqb.id` must therefore be listed *before* the
    /// `id` that produces it. The outer template here can only resolve if the inner one already
    /// ran, so the asserted value pins that order down.
    #[tokio::test]
    async fn the_last_listed_consumer_middleware_runs_first() {
        use crate::models::{Endpoint, EndpointType, Middleware};

        let config = MemoryConfig::new(format!("id_order_{}", fast_uuid_v7::gen_id()), None);
        let publisher = crate::endpoints::memory::MemoryPublisher::new(&config).unwrap();
        publisher
            .send(CanonicalMessage::from(r#"{"x":"X"}"#))
            .await
            .unwrap();

        let mut endpoint = Endpoint::new(EndpointType::Null);
        endpoint.middlewares = vec![
            Middleware::Id("${metadata:mqb.id}-outer".to_string()),
            Middleware::Id("${payload:x}".to_string()),
        ];

        let inner = Box::new(MemoryConsumer::new(&config).unwrap());
        let mut consumer =
            crate::middleware::apply_middlewares_to_consumer(inner, &endpoint, "ord")
                .await
                .unwrap();

        let message = consumer.receive().await.unwrap().message;
        assert_eq!(
            message.metadata.get(MESSAGE_IDENTITY_KEY),
            Some(&"X-outer".to_string())
        );
    }

    /// Silently ignoring it on an output would leave `mqb.id` unset with no way to notice.
    #[tokio::test]
    async fn id_on_an_output_endpoint_fails_fast() {
        use crate::models::{Endpoint, EndpointType, Middleware};

        let mut endpoint = Endpoint::new(EndpointType::Null);
        endpoint.middlewares = vec![Middleware::Id("${payload:order_id}".to_string())];

        let result = crate::middleware::apply_middlewares_to_publisher(
            Box::new(crate::endpoints::structural::null::NullPublisher),
            &endpoint,
            "id_output",
        )
        .await;

        let err = result
            .err()
            .expect("`id` on an output must not be accepted");
        assert!(
            err.to_string().contains("consumer-only"),
            "error should say why, got {err}"
        );
    }

    #[tokio::test]
    async fn a_malformed_template_fails_at_construction() {
        let config = MemoryConfig::new("id_mw_bad".to_string(), None);
        let inner = MemoryConsumer::new(&config).unwrap();
        assert!(IdConsumer::new(Box::new(inner), "${gen:not_a_generator}").is_err());
    }

    #[tokio::test]
    async fn delivery_varying_tokens_fail_at_construction() {
        for template in ["${gen:uuid}", "${message:id}"] {
            let config =
                MemoryConfig::new(format!("id_mw_unstable_{}", fast_uuid_v7::gen_id()), None);
            let inner = MemoryConsumer::new(&config).unwrap();
            let err = IdConsumer::new(Box::new(inner), template)
                .err()
                .expect("a delivery-varying identity must not be accepted");
            assert!(
                err.to_string().contains("replay-stable"),
                "error should say why, got {err}"
            );
        }
    }

    #[test]
    fn an_unresolved_template_clears_a_prior_identity() {
        let config = MemoryConfig::new(format!("id_mw_stale_{}", fast_uuid_v7::gen_id()), None);
        let inner = MemoryConsumer::new(&config).unwrap();
        let mut consumer = IdConsumer::new(Box::new(inner), "${payload:order_id}").unwrap();
        let mut message = CanonicalMessage::from(r#"{"other":"x"}"#);
        message
            .metadata
            .insert(MESSAGE_IDENTITY_KEY.to_string(), "stale".to_string());

        consumer.stamp(&mut message);

        assert_eq!(message.metadata.get(MESSAGE_IDENTITY_KEY), None);
    }

    /// A tokenless template renders the same string for every message, which is the collapse
    /// the empty and partial-render checks exist to prevent — just guaranteed instead of
    /// conditional. Cheaper to reject at startup than to detect at runtime.
    #[tokio::test]
    async fn a_template_with_no_tokens_fails_at_construction() {
        let config = MemoryConfig::new("id_mw_constant".to_string(), None);
        let inner = MemoryConsumer::new(&config).unwrap();
        let err = IdConsumer::new(Box::new(inner), "orders")
            .err()
            .expect("a constant identity must not be accepted");
        assert!(
            err.to_string().contains("same identity"),
            "error should say why, got {err}"
        );
    }
}
