//! A/B throughput bench: omq.rs (`zeromq-omq`) vs zmq.rs (`zeromq`) on the
//! PUSH/PULL pattern, to sanity-check omq's "faster than libzmq" claim on this
//! machine for the mq-bridge endpoint path (framing + trait plumbing included,
//! not the raw socket in isolation).
//!
//! Not a criterion bench — it drives the real endpoint types directly and prints
//! msgs/s so the two backends can be compared side by side.
//!
//! Run with: `cargo bench --bench zeromq_backends \
//!            --no-default-features --features zeromq,zeromq-omq`

use std::time::Instant;

use mq_bridge::CanonicalMessage;
use mq_bridge::endpoints::zeromq::{
    ZeroMqConsumer, ZeroMqOmqConsumer, ZeroMqOmqPublisher, ZeroMqPublisher,
};
use mq_bridge::models::{ZeroMqConfig, ZeroMqFormat, ZeroMqSocketType};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};

const N: usize = 200_000;
const PAYLOAD_BYTES: usize = 64;
const BATCH: usize = 1_000;

fn configs(url: &str, format: ZeroMqFormat) -> (ZeroMqConfig, ZeroMqConfig) {
    let consumer = ZeroMqConfig {
        url: url.to_string(),
        socket_type: Some(ZeroMqSocketType::Pull),
        bind: true,
        format: format.clone(),
        ..Default::default()
    };
    let publisher = ZeroMqConfig {
        url: url.to_string(),
        socket_type: Some(ZeroMqSocketType::Push),
        bind: false,
        format,
        ..Default::default()
    };
    (consumer, publisher)
}

fn make_batch() -> Vec<CanonicalMessage> {
    let payload = vec![b'x'; PAYLOAD_BYTES];
    (0..BATCH)
        .map(|_| CanonicalMessage::new(payload.clone(), None))
        .collect()
}

/// Drive a boxed consumer+publisher: publisher sends N messages in BATCH-sized
/// batches while a spawned task drains the consumer until N are received.
/// Returns messages/second across the full transfer.
async fn run(mut consumer: Box<dyn MessageConsumer>, publisher: Box<dyn MessagePublisher>) -> f64 {
    // Let the connection establish so we don't time the TCP handshake.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let drain = tokio::spawn(async move {
        let mut got = 0usize;
        while got < N {
            // Bound each receive so a stalled/lossy transport fails fast instead of
            // hanging the whole bench forever.
            let batch = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                consumer.receive_batch(BATCH),
            )
            .await
            .expect("drain stalled: no batch within 30s")
            .expect("receive_batch");
            got += batch.messages.len();
        }
    });

    let start = Instant::now();
    let mut sent = 0usize;
    while sent < N {
        publisher
            .send_batch(make_batch())
            .await
            .expect("send_batch");
        sent += BATCH;
    }
    drain.await.expect("drain task");
    let elapsed = start.elapsed().as_secs_f64();

    N as f64 / elapsed
}

/// Distinct port per (format, backend) run so nothing races on rebind.
static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(5710);
fn next_url() -> String {
    let port = NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tcp://127.0.0.1:{port}")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("ZeroMQ backend A/B — PUSH/PULL, N={N}, payload={PAYLOAD_BYTES}B, batch={BATCH}\n");

    // Json is the default format and batches the whole send_batch() call into one
    // ZMQ frame; Raw sends one ZMQ message per canonical message. zmq.rs routes
    // every send through a per-job oneshot-ack actor, so Raw (one job per message)
    // exposes that serialization while Json (one job per batch) hides it. omq sends
    // direct (&self, no ack channel), so it is largely format-insensitive.
    for format in [ZeroMqFormat::Json, ZeroMqFormat::Raw] {
        println!("format = {format:?}");

        let (c, p) = configs(&next_url(), format.clone());
        let consumer = Box::new(ZeroMqConsumer::new(&c).await.unwrap());
        let publisher = Box::new(ZeroMqPublisher::new(&p).await.unwrap());
        let rate = run(consumer, publisher).await;
        println!("  zmq.rs (zeromq)      : {rate:>12.0} msg/s");

        let (c, p) = configs(&next_url(), format.clone());
        let consumer = Box::new(ZeroMqOmqConsumer::new(&c).await.unwrap());
        let publisher = Box::new(ZeroMqOmqPublisher::new(&p).await.unwrap());
        let rate = run(consumer, publisher).await;
        println!("  omq.rs (zeromq-omq)  : {rate:>12.0} msg/s\n");
    }
}
