//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! A small semantic test suite for endpoint implementations.
//!
//! It runs against a [`CustomEndpointFactory`], so the same checks can be run
//! twice: once against your factory linked directly as Rust code, and once
//! against the factory the host builds from your compiled plugin. If both pass,
//! the ABI round trip preserved your endpoint's semantics.
//!
//! ```no_run
//! # use mq_bridge::plugin::conformance::{self, ConformanceOptions};
//! # async fn check(factory: &dyn mq_bridge::traits::CustomEndpointFactory) -> anyhow::Result<()> {
//! conformance::run(
//!     factory,
//!     ConformanceOptions::new("conformance", serde_json::json!({ "queue": "t" })),
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```

use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures::future::BoxFuture;

use crate::traits::{CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher};
use crate::{CanonicalMessage, SentBatch};

/// What to exercise, and against which endpoint configuration.
#[derive(Debug, Clone)]
pub struct ConformanceOptions {
    /// Route name passed to the factory.
    pub route_name: String,
    /// Endpoint configuration. Input and output share it, so the checks
    /// round-trip through a single queue or topic.
    pub config: serde_json::Value,
    /// Number of messages the delivery check moves.
    pub messages: usize,
    /// How long a check waits for messages it has already published.
    pub receive_timeout: Duration,
    /// Whether unacknowledged messages must be redelivered. Turn off for
    /// endpoints without redelivery (a file tail, an append-only sink).
    pub expect_redelivery: bool,
    /// Whether producer metadata must survive the round trip. Turn off for
    /// transports that carry payloads only.
    pub expect_metadata: bool,
}

impl ConformanceOptions {
    pub fn new(route_name: impl Into<String>, config: serde_json::Value) -> Self {
        Self {
            route_name: route_name.into(),
            config,
            messages: 8,
            receive_timeout: Duration::from_secs(10),
            expect_redelivery: true,
            expect_metadata: true,
        }
    }
}

type Check = for<'a> fn(
    &'a dyn CustomEndpointFactory,
    &'a ConformanceOptions,
) -> BoxFuture<'a, anyhow::Result<()>>;

/// Runs the suite, returning the checks that passed. Stops at the first
/// failure, with context naming the check.
pub async fn run(
    factory: &dyn CustomEndpointFactory,
    options: ConformanceOptions,
) -> anyhow::Result<Vec<&'static str>> {
    let mut checks: Vec<(&'static str, Check)> = vec![("round_trip", |factory, options| {
        Box::pin(round_trip(factory, options))
    })];
    if options.expect_metadata {
        checks.push(("metadata_preserved", |factory, options| {
            Box::pin(metadata_preserved(factory, options))
        }));
    }
    if options.expect_redelivery {
        checks.push(("nack_redelivers", |factory, options| {
            Box::pin(nack_redelivers(factory, options))
        }));
        checks.push(("uncommitted_batch_redelivers", |factory, options| {
            Box::pin(uncommitted_batch_redelivers(factory, options))
        }));
    }

    let mut passed = Vec::with_capacity(checks.len());
    for (name, check) in checks {
        check(factory, &options)
            .await
            .with_context(|| format!("conformance check `{name}` failed"))?;
        tracing::info!(check = name, "endpoint conformance check passed");
        passed.push(name);
    }
    Ok(passed)
}

/// Everything published is received, payloads intact.
async fn round_trip(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<()> {
    let mut consumer = open_consumer(factory, options).await?;
    let publisher = open_publisher(factory, options).await?;
    let sent = payloads("round-trip", options.messages);
    publish(&*publisher, &sent).await?;

    let mut actual: Vec<String> = receive(&mut *consumer, &sent, options, MessageDisposition::Ack)
        .await?
        .iter()
        .map(|message| message.get_payload_str().to_string())
        .collect();
    let mut expected = sent;
    expected.sort();
    actual.sort();
    if expected != actual {
        bail!("published {expected:?} but received {actual:?}");
    }
    Ok(())
}

/// Metadata set by the producer survives the trip.
async fn metadata_preserved(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<()> {
    let mut consumer = open_consumer(factory, options).await?;
    let publisher = open_publisher(factory, options).await?;

    let mut message = CanonicalMessage::from("metadata-check");
    message
        .metadata
        .insert("conformance".to_string(), "value".to_string());
    send(&*publisher, vec![message]).await?;

    let expected = vec!["metadata-check".to_string()];
    let received = receive(&mut *consumer, &expected, options, MessageDisposition::Ack).await?;
    let value = received[0].metadata.get("conformance");
    if value.map(String::as_str) != Some("value") {
        // Not every transport carries metadata; say so precisely rather than
        // leaving the caller to guess from a payload mismatch.
        bail!(
            "metadata `conformance` did not survive the round trip (got {value:?}); \
             if this endpoint intentionally carries no metadata, set \
             `expect_metadata = false`"
        );
    }
    Ok(())
}

/// A nacked message comes back.
async fn nack_redelivers(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<()> {
    let mut consumer = open_consumer(factory, options).await?;
    let publisher = open_publisher(factory, options).await?;
    let payload = payloads("nack", 1);
    publish(&*publisher, &payload).await?;

    let first = receive(&mut *consumer, &payload, options, MessageDisposition::Nack).await?;
    if first[0].get_payload_str() != payload[0] {
        bail!(
            "expected `{}`, got `{}`",
            payload[0],
            first[0].get_payload_str()
        );
    }
    let second = receive(&mut *consumer, &payload, options, MessageDisposition::Ack).await?;
    if second[0].get_payload_str() != payload[0] {
        bail!(
            "a nacked message was not redelivered; got `{}`",
            second[0].get_payload_str()
        );
    }
    Ok(())
}

/// A batch dropped without committing acknowledges nothing.
async fn uncommitted_batch_redelivers(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<()> {
    let mut consumer = open_consumer(factory, options).await?;
    let publisher = open_publisher(factory, options).await?;
    let payload = payloads("dropped", 1);
    publish(&*publisher, &payload).await?;

    let deadline = Instant::now() + options.receive_timeout;
    loop {
        let batch = consumer.receive_batch(1).await?;
        if batch.messages.iter().any(|message| {
            payload
                .iter()
                .any(|value| value.as_str() == message.get_payload_str())
        }) {
            drop(batch); // deliberately without calling `batch.commit`
            break;
        }
        let dispositions = vec![MessageDisposition::Ack; batch.messages.len()];
        (batch.commit)(dispositions)
            .await
            .context("acknowledging messages left by an earlier check")?;
        if Instant::now() > deadline {
            bail!("nothing was delivered within {:?}", options.receive_timeout);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let redelivered = receive(&mut *consumer, &payload, options, MessageDisposition::Ack).await?;
    if redelivered[0].get_payload_str() != payload[0] {
        bail!(
            "a batch dropped without commit was not redelivered; got `{}`",
            redelivered[0].get_payload_str()
        );
    }
    Ok(())
}

// ------------------------------------------------------------------- helpers

async fn open_consumer(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<Box<dyn MessageConsumer>> {
    factory
        .create_consumer(&options.route_name, &options.config)
        .await
        .context("creating the input endpoint")
}

async fn open_publisher(
    factory: &dyn CustomEndpointFactory,
    options: &ConformanceOptions,
) -> anyhow::Result<Box<dyn MessagePublisher>> {
    factory
        .create_publisher(&options.route_name, &options.config)
        .await
        .context("creating the output endpoint")
}

/// Distinguishes this run's messages from anything an earlier run left behind
/// in a durable queue.
fn payloads(prefix: &str, count: usize) -> Vec<String> {
    let run = fast_uuid_v7::gen_id();
    (0..count)
        .map(|index| format!("{prefix}-{run:x}-{index}"))
        .collect()
}

async fn publish(publisher: &dyn MessagePublisher, payloads: &[String]) -> anyhow::Result<()> {
    let messages = payloads
        .iter()
        .map(|payload| CanonicalMessage::from(payload.as_str()))
        .collect();
    send(publisher, messages).await
}

async fn send(
    publisher: &dyn MessagePublisher,
    messages: Vec<CanonicalMessage>,
) -> anyhow::Result<()> {
    let count = messages.len();
    match publisher.send_batch(messages).await {
        Ok(SentBatch::Ack) => {}
        Ok(SentBatch::Partial { failed, .. }) if failed.is_empty() => {}
        Ok(SentBatch::Partial { failed, .. }) => {
            bail!("{} of {count} messages failed to publish", failed.len())
        }
        Err(err) => bail!("publishing failed: {err}"),
    }
    publisher.flush().await.context("flushing after publish")
}

/// Receives until `count` messages have arrived, committing each batch with
/// `disposition`.
async fn receive(
    consumer: &mut dyn MessageConsumer,
    expected: &[String],
    options: &ConformanceOptions,
    disposition: MessageDisposition,
) -> anyhow::Result<Vec<CanonicalMessage>> {
    let count = expected.len();
    let deadline = Instant::now() + options.receive_timeout;
    let mut collected = Vec::with_capacity(count);
    while collected.len() < count {
        if Instant::now() > deadline {
            bail!(
                "received {} of {count} messages within {:?}",
                collected.len(),
                options.receive_timeout
            );
        }
        let batch = consumer.receive_batch(count - collected.len()).await?;
        if batch.messages.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        let mut dispositions = Vec::with_capacity(batch.messages.len());
        for message in batch.messages {
            if expected
                .iter()
                .any(|value| value.as_str() == message.get_payload_str())
            {
                collected.push(message);
                dispositions.push(disposition.clone());
            } else {
                dispositions.push(MessageDisposition::Ack);
            }
        }
        (batch.commit)(dispositions)
            .await
            .context("committing a received batch")?;
    }
    Ok(collected)
}
