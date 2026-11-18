// Structural performance invariants: which fast path ran, how many sends overlapped.
// Asserted from probe counters and counting sinks instead of timings, so they are quick
// and stable enough for every CI run. A failure here means a fast path silently vanished.
#![cfg(feature = "file")]

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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

/// Counts messages and the most sends that were in flight at once; each send takes 1 ms.
#[derive(Debug, Default)]
struct Overlap {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    messages: AtomicUsize,
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
        tokio::time::sleep(Duration::from_millis(1)).await;
        self.0.messages.fetch_add(messages.len(), Ordering::SeqCst);
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(SentBatch::Ack)
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

fn overlap_sink(name: &str) -> (Endpoint, Arc<Overlap>) {
    let overlap = Arc::new(Overlap::default());
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
    let (sink, overlap) = overlap_sink("perf_parallel_decode");
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
    let (sink, overlap) = overlap_sink("perf_sequential_decode");
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
        let (sink, overlap) = overlap_sink(&name);

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
        let (sink, overlap) = overlap_sink(&name);
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
