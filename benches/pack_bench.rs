//! Cost of transport-level batching: `pack` / `unpack`, with and without a codec.
//!
//! The question the numbers have to answer is *where the win comes from*. A packed
//! batch buys two different things, and they are separable:
//!
//! - **Fewer transport operations.** 1000 rows become one physical message. That
//!   saving lives in the transport, not here, so this benchmark prints the physical
//!   message count and leaves the per-operation cost to the endpoint benchmarks.
//! - **Fewer bytes.** The summary table prints bytes per row for each codec, so the
//!   wire saving is visible next to the CPU it costs.
//!
//! `individual` is the baseline: what an unpacked route spends framing the same rows
//! one at a time, which is the ZeroMQ `raw_framed` shape (a JSON metadata frame plus
//! the payload). Comparing it with `pack/none` shows what the envelope itself costs.
//!
//! Rows are the seven-column shape `csv_to_json_bench` uses, so a figure here is
//! directly comparable with the CSV/ETL numbers.
//!
//! Run with: `cargo bench --bench pack_bench --features test-utils,compression`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mq_bridge::models::{Compression, PackFormat, PackMiddleware};
use mq_bridge::test_utils::bench::{pack_batches, unpack_batch};
use mq_bridge::CanonicalMessage;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

const ROWS: usize = 20_000;
const BATCH: usize = 1000;

/// Counts allocations so the summary can report them per row. Criterion allocates
/// too, so the counts are only read from the one-shot summary pass, never from
/// inside a timed loop.
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
static ALLOCATOR: Counting = Counting;

fn corpus() -> Vec<CanonicalMessage> {
    const COUNTRIES: [&str; 5] = ["US", "GB", "DE", "IN", "JP"];
    const TIERS: [&str; 3] = ["free", "pro", "enterprise"];
    (0..ROWS)
        .map(|i| {
            let payload = format!(
                r#"{{"id":"{}","first_name":"Ada","country":"{}","amount":"{}.{:02}","created_at":"2020-01-01T00:00:00Z","active":"{}","attributes":"{{\"tier\":\"{}\",\"score\":{}}}"}}"#,
                i,
                COUNTRIES[i % COUNTRIES.len()],
                i % 10_000,
                i % 100,
                i % 2 == 0,
                TIERS[i % TIERS.len()],
                i % 100,
            );
            let mut message = CanonicalMessage::new(payload.into_bytes(), None);
            message
                .metadata
                .insert("mqb.src.file_offset".to_string(), i.to_string());
            message.metadata.insert("kind".to_string(), "order".to_string());
            message
        })
        .collect()
}

fn config(compression: Compression) -> PackMiddleware {
    PackMiddleware {
        format: PackFormat::Mqb,
        compression,
        max_messages: BATCH,
        max_bytes: 64 * 1024 * 1024,
        drop_message_id: false,
    }
}

fn codecs() -> [(&'static str, Compression); 4] {
    [
        ("none", Compression::None),
        ("gzip", Compression::Gzip),
        ("lz4", Compression::Lz4),
        ("zstd", Compression::Zstd),
    ]
}

/// What an unpacked route pays to frame the same rows one at a time: a JSON metadata
/// frame plus the payload, per message. The ZeroMQ `raw_framed` layout.
fn frame_individually(messages: &[CanonicalMessage]) -> usize {
    let mut bytes = 0;
    for message in messages {
        let meta = serde_json::to_vec(&message.metadata).expect("metadata serializes");
        bytes += meta.len() + message.payload.len();
    }
    bytes
}

fn individual_wire_bytes(messages: &[CanonicalMessage]) -> usize {
    frame_individually(messages)
}

/// Prints the wire-size and allocation table once, before the timed groups run.
fn summary(messages: &[CanonicalMessage]) {
    let raw: usize = messages.iter().map(|m| m.payload.len()).sum();
    let individual = individual_wire_bytes(messages);

    println!("\n=== pack: {ROWS} rows, batch {BATCH} ===");
    println!(
        "payload bytes only        {:>12}  ({:.1} B/row)",
        raw,
        raw as f64 / ROWS as f64
    );
    println!(
        "individual (raw_framed)   {:>12}  ({:.1} B/row)  {ROWS} transport ops",
        individual,
        individual as f64 / ROWS as f64
    );

    for (name, codec) in codecs() {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        let packed = pack_batches(&config(codec), messages).expect("packs");
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let bytes: usize = packed.iter().map(|b| b.len()).sum();
        println!(
            "pack/{name:<20} {:>12}  ({:.1} B/row)  {} transport ops, {:.3} allocs/row, {:.2}x vs individual",
            bytes,
            bytes as f64 / ROWS as f64,
            packed.len(),
            allocations as f64 / ROWS as f64,
            individual as f64 / bytes as f64,
        );
    }
    println!();
}

fn bench_pack(c: &mut Criterion) {
    let messages = corpus();
    summary(&messages);

    let mut group = c.benchmark_group("pack");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("individual", |b| {
        b.iter(|| frame_individually(&messages));
    });
    for (name, codec) in codecs() {
        let config = config(codec);
        group.bench_with_input(BenchmarkId::from_parameter(name), &config, |b, config| {
            b.iter(|| pack_batches(config, &messages).expect("packs"));
        });
    }
    group.finish();
}

fn bench_unpack(c: &mut Criterion) {
    let messages = corpus();

    let mut group = c.benchmark_group("unpack");
    group.throughput(Throughput::Elements(ROWS as u64));
    for (name, codec) in codecs() {
        let packed = pack_batches(&config(codec), &messages).expect("packs");
        group.bench_with_input(BenchmarkId::from_parameter(name), &packed, |b, packed| {
            b.iter(|| {
                let mut count = 0;
                for batch in packed {
                    count += unpack_batch(PackFormat::Mqb, batch).expect("unpacks").len();
                }
                count
            });
        });
    }
    group.finish();
}

/// The full pipeline cost, so pack and unpack can be compared against the
/// unpacked baseline as one number.
fn bench_round_trip(c: &mut Criterion) {
    let messages = corpus();

    let mut group = c.benchmark_group("pack_round_trip");
    group.throughput(Throughput::Elements(ROWS as u64));
    for (name, codec) in codecs() {
        let config = config(codec);
        group.bench_with_input(BenchmarkId::from_parameter(name), &config, |b, config| {
            b.iter(|| {
                let mut count = 0;
                for batch in pack_batches(config, &messages).expect("packs") {
                    count += unpack_batch(PackFormat::Mqb, &batch)
                        .expect("unpacks")
                        .len();
                }
                count
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_pack, bench_unpack, bench_round_trip);
criterion_main!(benches);
