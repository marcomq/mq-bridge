//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! OpenTelemetry spans per message, continuing the W3C `traceparent` carried in metadata.
//!
//! Only the OpenTelemetry API is used; the host installs the tracer provider. The middleware
//! activates itself when one is installed at route start and otherwise stays out of the chain,
//! so configuring it costs nothing. `traceparent`/`tracestate` are read and written here
//! directly, so no global propagator is needed.

use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::trace::{
    Span, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
    Tracer,
};
use opentelemetry::{Context, KeyValue};
use std::any::Any;
use std::str::FromStr;

const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

fn tracer() -> BoxedTracer {
    global::tracer("mq-bridge")
}

/// Whether the host has installed a real tracer provider.
///
/// The no-op tracer hands back its parent's span context, a real one issues a new span id.
/// The probe's parent is unsampled, so a parent-based sampler (the default) exports nothing.
pub(crate) fn tracer_installed() -> bool {
    let parent = SpanContext::new(
        TraceId::from(1),
        SpanId::from(1),
        TraceFlags::default(),
        true,
        TraceState::default(),
    );
    let cx = Context::new().with_remote_span_context(parent);
    let mut span = tracer().start_with_context("mq-bridge.probe", &cx);
    let installed = span.span_context().span_id() != SpanId::from(1);
    span.end();
    installed
}

/// Reads a W3C `traceparent` (`00-<trace id>-<span id>-<flags>`) and `tracestate`.
fn extract(msg: &CanonicalMessage) -> Context {
    let remote = msg.metadata.get(TRACEPARENT).and_then(|header| {
        let mut parts = header.trim().split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if version.len() != 2 || version == "ff" || trace.len() != 32 || span.len() != 16 {
            return None;
        }
        let state = msg
            .metadata
            .get(TRACESTATE)
            .and_then(|s| TraceState::from_str(s).ok())
            .unwrap_or_default();
        let span_context = SpanContext::new(
            TraceId::from_hex(trace).ok()?,
            SpanId::from_hex(span).ok()?,
            TraceFlags::new(u8::from_str_radix(flags, 16).ok()?),
            true,
            state,
        );
        span_context.is_valid().then_some(span_context)
    });
    match remote {
        Some(span_context) => Context::new().with_remote_span_context(span_context),
        None => Context::new(),
    }
}

fn inject(cx: &Context, msg: &mut CanonicalMessage) {
    let span = cx.span();
    let sc = span.span_context();
    if !sc.is_valid() {
        return;
    }
    msg.metadata.insert(
        TRACEPARENT.to_string(),
        format!(
            "00-{:032x}-{:016x}-{:02x}",
            sc.trace_id(),
            sc.span_id(),
            sc.trace_flags() & TraceFlags::SAMPLED
        ),
    );
    let state = sc.trace_state().header();
    if !state.is_empty() {
        msg.metadata.insert(TRACESTATE.to_string(), state);
    }
}

/// Starts a span continuing the message's trace and writes the span's context back into
/// the metadata, so the next hop continues from it.
fn start_span(
    tracer: &BoxedTracer,
    name: &str,
    kind: SpanKind,
    route: &str,
    msg: &mut CanonicalMessage,
) -> Context {
    let parent = extract(msg);
    let span = tracer
        .span_builder(name.to_string())
        .with_kind(kind)
        .with_attributes([
            KeyValue::new("mqb.route", route.to_string()),
            KeyValue::new("messaging.message.id", format!("{:032x}", msg.message_id)),
        ])
        .start_with_context(tracer, &parent);
    let cx = parent.with_span(span);
    inject(&cx, msg);
    cx
}

fn end_span(cx: &Context, error: Option<String>) {
    let span = cx.span();
    if let Some(error) = error {
        span.set_status(Status::error(error));
    }
    span.end();
}

pub struct OtelConsumer {
    inner: Box<dyn MessageConsumer>,
    tracer: BoxedTracer,
    route: String,
    span_name: String,
}

impl OtelConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, route_name: &str) -> Self {
        Self {
            inner,
            tracer: tracer(),
            route: route_name.to_string(),
            span_name: format!("{route_name} receive"),
        }
    }
}

#[async_trait]
impl MessageConsumer for OtelConsumer {
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

    /// Each span lasts until the message is committed, so it covers the whole pass
    /// through the route; a Nack marks it as an error.
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        if batch.messages.is_empty() {
            return Ok(batch);
        }
        let spans: Vec<Context> = batch
            .messages
            .iter_mut()
            .map(|m| {
                start_span(
                    &self.tracer,
                    &self.span_name,
                    SpanKind::Consumer,
                    &self.route,
                    m,
                )
            })
            .collect();
        let inner_commit = batch.commit;
        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let nacked: Vec<bool> = dispositions
                .iter()
                .map(|d| matches!(d, MessageDisposition::Nack))
                .collect();
            Box::pin(async move {
                let result = inner_commit(dispositions).await;
                for (i, cx) in spans.iter().enumerate() {
                    let error = if let Err(e) = &result {
                        Some(format!("commit failed: {e}"))
                    } else if nacked.get(i).copied().unwrap_or(false) {
                        Some("nacked".to_string())
                    } else {
                        None
                    };
                    end_span(cx, error);
                }
                result
            })
        });
        batch.commit = commit;
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

pub struct OtelPublisher {
    inner: Box<dyn MessagePublisher>,
    tracer: BoxedTracer,
    route: String,
    span_name: String,
}

impl OtelPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, route_name: &str) -> Self {
        Self {
            inner,
            tracer: tracer(),
            route: route_name.to_string(),
            span_name: format!("{route_name} send"),
        }
    }

    fn start(&self, msg: &mut CanonicalMessage) -> Context {
        start_span(
            &self.tracer,
            &self.span_name,
            SpanKind::Producer,
            &self.route,
            msg,
        )
    }
}

#[async_trait]
impl MessagePublisher for OtelPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let cx = self.start(&mut message);
        let result = self.inner.send(message).await;
        end_span(&cx, result.as_ref().err().map(|e| e.to_string()));
        result
    }

    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let spans: Vec<(u128, Context)> = messages
            .iter_mut()
            .map(|m| (m.message_id, self.start(m)))
            .collect();
        let result = self.inner.send_batch(messages).await;
        match &result {
            Ok(SentBatch::Ack) => spans.iter().for_each(|(_, cx)| end_span(cx, None)),
            Ok(SentBatch::Partial { failed, .. }) => {
                for (id, cx) in &spans {
                    let error = failed
                        .iter()
                        .find(|(m, _)| m.message_id == *id)
                        .map(|(_, e)| e.to_string());
                    end_span(cx, error);
                }
            }
            Err(e) => {
                let error = e.to_string();
                spans
                    .iter()
                    .for_each(|(_, cx)| end_span(cx, Some(error.clone())));
            }
        }
        result
    }

    crate::traits::forward_publisher_meta!(inner);

    fn as_any(&self) -> &dyn Any {
        self
    }
}
