// Structural performance invariants: which fast path ran, how many sends overlapped.
// Asserted from probe counters and counting sinks instead of timings, so they are quick
// and stable enough for every CI run. A failure here means a fast path silently vanished.
#![cfg(feature = "file")]

use mq_bridge::endpoints::create_publisher_from_route;
use mq_bridge::extensions::register_endpoint_factory;
use mq_bridge::models::{Endpoint, EndpointType, FileConfig, Middleware};
use mq_bridge::support::probe;
use mq_bridge::traits::{
    CustomEndpointFactory, MessageConsumer, MessagePublisher, PublisherError, SentBatch,
};
use mq_bridge::{CanonicalMessage, Route};
use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Counts every allocation in this binary, for the per-message budgets below.
struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocation counts are process-wide, so the tests here take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Counts messages and the most sends that were in flight at once. With `gate`, a lone
/// send waits (bounded) for a second one to enter, so overlap does not hinge on timing.
#[derive(Debug, Default)]
struct Overlap {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    messages: AtomicUsize,
    calls: AtomicUsize,
    gate: bool,
    ordered: bool,
    entered: tokio::sync::Notify,
}

struct OverlapSink(Arc<Overlap>);

#[async_trait::async_trait]
impl MessagePublisher for OverlapSink {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let now = self.0.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_in_flight.fetch_max(now, Ordering::SeqCst);
        self.0.entered.notify_waiters();
        if self.0.gate {
            let entered = self.0.entered.notified();
            tokio::pin!(entered);
            entered.as_mut().enable();
            if self.0.max_in_flight.load(Ordering::SeqCst) < 2 {
                let _ = tokio::time::timeout(Duration::from_secs(5), entered).await;
            }
        }
        self.0.messages.fetch_add(messages.len(), Ordering::SeqCst);
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(SentBatch::Ack)
    }

    fn requires_ordered_publish(&self) -> bool {
        self.0.ordered
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct OverlapFactory(Arc<Overlap>);

#[async_trait::async_trait]
impl CustomEndpointFactory for OverlapFactory {
    async fn create_consumer(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        anyhow::bail!("sink only")
    }

    async fn create_publisher(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(OverlapSink(Arc::clone(&self.0))))
    }
}

fn overlap_sink(name: &str, gate: bool) -> (Endpoint, Arc<Overlap>) {
    sink_with(
        name,
        Overlap {
            gate,
            ..Default::default()
        },
    )
}

fn sink_with(name: &str, overlap: Overlap) -> (Endpoint, Arc<Overlap>) {
    let overlap = Arc::new(overlap);
    register_endpoint_factory(name, Arc::new(OverlapFactory(Arc::clone(&overlap)))).unwrap();
    let endpoint = Endpoint::new(EndpointType::Custom {
        name: name.to_string(),
        config: serde_json::Value::Null,
    });
    (endpoint, overlap)
}

fn jsonl_source(dir: &tempfile::TempDir, lines: usize) -> Endpoint {
    let path = dir.path().join("in.jsonl");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..lines {
        writeln!(file, r#"{{"id":{i},"name":"row {i}"}}"#).unwrap();
    }
    file.flush().unwrap();
    Endpoint::new(EndpointType::File(FileConfig::new(path.to_str().unwrap())))
}

async fn drain(route: Route, name: &str) {
    tokio::time::timeout(
        Duration::from_secs(30),
        route
            .with_exit_on_empty(true)
            .run_until_err(name, None, None),
    )
    .await
    .expect("route did not drain")
    .expect("route failed");
}

/// Probe counters are process-wide, so each test asserts on the delta it caused.
fn probe_delta(name: &'static str) -> impl Fn() -> u64 {
    let start = probe::count(name);
    move || probe::count(name) - start
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_batch_decodes_and_transforms_in_parallel() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, overlap) = overlap_sink("perf_parallel_decode", false);
    let decode = probe_delta("file.decode.parallel");
    let middleware = probe_delta("map_messages.parallel");
    let transform: Middleware =
        serde_json::from_value(serde_json::json!({"transform": {"mapping": {"id": "$.id"}}}))
            .unwrap();

    let source = jsonl_source(&dir, 2048).add_middleware(transform);
    drain(
        Route::new(source, sink).with_batch_size(1024),
        "perf_parallel_decode",
    )
    .await;

    assert_eq!(overlap.messages.load(Ordering::SeqCst), 2048);
    if std::thread::available_parallelism().map_or(1, |n| n.get()) > 1 {
        assert!(
            decode() >= 1,
            "batches of 1024 records must decode in parallel"
        );
        assert!(
            middleware() >= 1,
            "batches of 1024 must spread transform over cores"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_file_batch_decodes_sequentially() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, overlap) = overlap_sink("perf_sequential_decode", false);
    let sequential = probe_delta("file.decode.sequential");

    let route = Route::new(jsonl_source(&dir, 64), sink).with_batch_size(16);
    drain(route, "perf_sequential_decode").await;

    assert_eq!(overlap.messages.load(Ordering::SeqCst), 64);
    assert!(
        sequential() >= 1,
        "batches of 16 records must stay on the calling thread"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_overlaps_sends_and_one_keeps_them_serial() {
    let _serial = SERIAL.lock().await;
    for (concurrency, expect_overlap) in [(8, true), (1, false)] {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("perf_overlap_{concurrency}");
        let (sink, overlap) = overlap_sink(&name, expect_overlap);

        let route = Route::new(jsonl_source(&dir, 512), sink)
            .with_batch_size(8)
            .with_concurrency(concurrency);
        drain(route, &name).await;

        let max = overlap.max_in_flight.load(Ordering::SeqCst);
        assert_eq!(overlap.messages.load(Ordering::SeqCst), 512);
        if expect_overlap {
            assert!(
                max >= 2,
                "concurrency {concurrency} never overlapped two sends"
            );
        } else {
            assert_eq!(max, 1, "concurrency 1 overlapped sends");
        }
    }
}

/// Allocations a file route spends per message, fixed setup cost subtracted by running
/// two lengths. Measured 8 and 11 on macOS (2026-11); the budgets allow one more.
async fn allocations_per_message(transform: bool) -> usize {
    let mut counts = Vec::new();
    for lines in [1024, 3072] {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("perf_alloc_{transform}_{lines}");
        let (sink, overlap) = overlap_sink(&name, false);
        let mut source = jsonl_source(&dir, lines);
        if transform {
            source = source.add_middleware(
                serde_json::from_value(
                    serde_json::json!({"transform": {"mapping": {"id": "$.id"}}}),
                )
                .unwrap(),
            );
        }
        let before = ALLOCATIONS.load(Ordering::SeqCst);
        drain(Route::new(source, sink).with_batch_size(128), &name).await;
        counts.push(ALLOCATIONS.load(Ordering::SeqCst) - before);
        assert_eq!(overlap.messages.load(Ordering::SeqCst), lines);
    }
    (counts[1].saturating_sub(counts[0])) / 2048
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_route_stays_within_its_allocation_budget() {
    let _serial = SERIAL.lock().await;
    let plain = allocations_per_message(false).await;
    let transformed = allocations_per_message(true).await;
    assert!(
        plain <= 9,
        "plain file route: {plain} allocations per message"
    );
    assert!(
        transformed <= 12,
        "transform: {transformed} allocations per message"
    );
}

/// Every output middleware that does not merge messages, with a config that passes all through.
fn passthrough_output_middlewares() -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::json;
    #[allow(unused_mut)]
    let mut list = vec![
        ("retry", json!({"retry": {}})),
        ("dlq", json!({"dlq": {"endpoint": {"null": null}}})),
        (
            "transform",
            json!({"transform": {"mapping": {"id": "$.id"}}}),
        ),
        (
            "limiter",
            json!({"limiter": {"messages_per_second": 10_000_000}}),
        ),
        ("delay", json!({"delay": {"delay_ms": 1}})),
        ("timeout", json!({"timeout": {"timeout_ms": 5000}})),
        ("cookie_jar", json!({"cookie_jar": {}})),
        (
            "lookup",
            json!({"lookup": {"from": {"static": {"body": "{\"tier\":\"gold\"}", "raw": true}}, "into": "customer"}}),
        ),
        ("parallel", json!({"parallel": {}})),
    ];
    #[cfg(feature = "filter")]
    list.push(("filter", json!({"filter": "id >= 0"})));
    #[cfg(feature = "encryption")]
    list.push((
        "encryption",
        json!({"encryption": {"key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}),
    ));
    #[cfg(feature = "compression")]
    list.push(("compression", json!({"compression": {"algorithm": "zstd"}})));
    #[cfg(feature = "metrics")]
    list.push(("metrics", json!({"metrics": {}})));
    #[cfg(feature = "otel")]
    list.push(("otel", json!({"otel": {}})));
    #[cfg(feature = "aggregate")]
    list.push((
        "aggregate",
        json!({"aggregate": {"consistency": "single_writer", "key": "${payload:id}", "into": "agg",
            "expression": "{ count: (state.count ?? 0) + 1 }"}}),
    ));
    list
}

fn middleware(config: &serde_json::Value) -> Middleware {
    serde_json::from_value(config.clone()).unwrap()
}

/// A middleware that turns one batch into many sends silently costs a round trip per message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_output_middleware_hands_the_sink_whole_batches() {
    let _serial = SERIAL.lock().await;
    let mut split = Vec::new();
    for (name, config) in passthrough_output_middlewares() {
        let dir = tempfile::tempdir().unwrap();
        let route_name = format!("perf_whole_{name}");
        let (sink, overlap) = overlap_sink(&route_name, false);
        let route = Route::new(
            jsonl_source(&dir, 256),
            sink.add_middleware(middleware(&config)),
        )
        .with_batch_size(32);
        drain(route, &route_name).await;

        let messages = overlap.messages.load(Ordering::SeqCst);
        let calls = overlap.calls.load(Ordering::SeqCst);
        if messages != 256 || calls != 8 {
            split.push(format!("{name}: {messages} messages in {calls} sends"));
        }
    }
    assert!(
        split.is_empty(),
        "expected 256 messages in 8 sends: {split:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffer_and_pack_never_send_more_often_than_batches_arrive() {
    let _serial = SERIAL.lock().await;
    for (name, config, expect_messages) in [
        (
            "buffer",
            serde_json::json!({"buffer": {"max_messages": 64, "max_delay_ms": 5}}),
            256,
        ),
        ("pack", serde_json::json!({"pack": {}}), 8),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let route_name = format!("perf_merge_{name}");
        let (sink, overlap) = overlap_sink(&route_name, false);
        let route = Route::new(
            jsonl_source(&dir, 256),
            sink.add_middleware(middleware(&config)),
        )
        .with_batch_size(32);
        drain(route, &route_name).await;

        assert_eq!(
            overlap.messages.load(Ordering::SeqCst),
            expect_messages,
            "{name}"
        );
        let calls = overlap.calls.load(Ordering::SeqCst);
        assert!(calls <= 8, "{name}: 8 batches became {calls} sends");
    }
}

/// An order-sensitive sink loses its sequencing if a wrapper forgets to forward the flag.
#[tokio::test]
async fn every_output_middleware_keeps_the_sink_ordered() {
    let _serial = SERIAL.lock().await;
    let mut list = passthrough_output_middlewares();
    list.push((
        "buffer",
        serde_json::json!({"buffer": {"max_messages": 64, "max_delay_ms": 5}}),
    ));
    list.push(("pack", serde_json::json!({"pack": {}})));
    let mut lost = Vec::new();
    for (name, config) in list {
        let (sink, _) = sink_with(
            &format!("perf_ordered_{name}"),
            Overlap {
                ordered: true,
                ..Default::default()
            },
        );
        let endpoint = sink.add_middleware(middleware(&config));
        let publisher = create_publisher_from_route(&format!("perf_ordered_{name}"), &endpoint)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        if !publisher.requires_ordered_publish() {
            lost.push(name);
        }
    }
    assert!(
        lost.is_empty(),
        "these drop requires_ordered_publish: {lost:?}"
    );
}
