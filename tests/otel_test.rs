//! The `otel` middleware end to end. Its own test binary: the tracer provider is global.
#![cfg(feature = "otel")]

use mq_bridge::endpoints::memory::MemoryConsumer;
use mq_bridge::endpoints::structural::null::NullPublisher;
use mq_bridge::middleware::{apply_middlewares_to_consumer, apply_middlewares_to_publisher};
use mq_bridge::models::{Endpoint, Middleware, OtelMiddleware};
use mq_bridge::traits::MessageDisposition;
use mq_bridge::CanonicalMessage;
use opentelemetry::trace::SpanKind;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_SPAN: &str = "00f067aa0ba902b7";

fn otel_endpoint() -> Endpoint {
    let mut endpoint = Endpoint::new_memory("otel", 10);
    endpoint.middlewares = vec![Middleware::Otel(OtelMiddleware::default())];
    endpoint
}

fn traced_message() -> CanonicalMessage {
    CanonicalMessage::from("hello")
        .with_metadata_kv("traceparent", format!("00-{TRACE_ID}-{PARENT_SPAN}-01"))
}

/// Both phases share one test: before a provider exists and after, in that order.
#[tokio::test]
async fn otel_middleware_activates_itself_and_continues_the_trace() {
    // No provider installed: the middleware stays out of the way and touches nothing.
    let source = MemoryConsumer::new_local("otel_inactive", 10);
    source
        .channel()
        .send_message(traced_message())
        .await
        .unwrap();
    let mut consumer = apply_middlewares_to_consumer(Box::new(source), &otel_endpoint(), "r")
        .await
        .unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(
        batch.messages[0].get_metadata("traceparent").unwrap(),
        format!("00-{TRACE_ID}-{PARENT_SPAN}-01")
    );

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());

    let source = MemoryConsumer::new_local("otel_active", 10);
    source
        .channel()
        .send_message(traced_message())
        .await
        .unwrap();
    let mut consumer = apply_middlewares_to_consumer(Box::new(source), &otel_endpoint(), "r")
        .await
        .unwrap();
    let publisher = apply_middlewares_to_publisher(Box::new(NullPublisher), &otel_endpoint(), "r")
        .await
        .unwrap();

    let batch = consumer.receive_batch(10).await.unwrap();
    let received = batch.messages[0].clone();
    let traceparent = received.get_metadata("traceparent").unwrap();
    assert!(traceparent.starts_with(&format!("00-{TRACE_ID}-")));
    assert!(
        !traceparent.contains(PARENT_SPAN),
        "must point at the new span"
    );

    publisher.send(received).await.unwrap();
    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let receive = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Consumer)
        .unwrap();
    let send = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Producer)
        .unwrap();
    assert_eq!(
        format!("{:032x}", receive.span_context.trace_id()),
        TRACE_ID
    );
    assert_eq!(format!("{:016x}", receive.parent_span_id), PARENT_SPAN);
    assert_eq!(send.parent_span_id, receive.span_context.span_id());
    assert_eq!(
        send.span_context.trace_id(),
        receive.span_context.trace_id()
    );
}
