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
//! Compression is a separate middleware, so the `pack+<codec>` cases here measure the
//! composed pipeline — `[compression, pack]` on the output — rather than an inner codec.
//!
//! `individual` is the baseline on both sides: what an unpacked route spends framing
//! the same rows one at a time, and decoding them again, which is the ZeroMQ
//! `raw_framed` shape (a JSON metadata frame plus the payload). Comparing it with
//! `pack/none` shows what the envelope itself costs.
//!
//! **Do not read `pack` against `unpack` directly.** Packing metadata is a memcpy into
//! one buffer; unpacking it rebuilds a `HashMap<String, String>` per record, which costs
//! one map plus two allocations per pair. `unpack/none_no_metadata` isolates that: the
//! records themselves decode several times faster than they pack, and everything above
//! that is the map, not the format. The honest comparison for `unpack` is
//! `unpack/individual`, which pays the same reconstruction per message.
//!
//! Rows are the seven-column shape `csv_to_json_bench` uses, so a figure here is
//! directly comparable with the CSV/ETL numbers.
//!
//! Run with: `cargo bench --bench pack_bench --features test-utils,compression`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mq_bridge::models::{Compression, PackFormat, PackMiddleware};
use mq_bridge::test_utils::bench::{compress_member, decompress_member, pack_batches, unpack_batch};
use bytes::Bytes;
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

/// The same rows without metadata, to separate the envelope's own cost from the
/// cost of rebuilding a `HashMap<String, String>` per record.
fn corpus_without_metadata() -> Vec<CanonicalMessage> {
    corpus()
        .into_iter()
        .map(|mut message| {
            message.metadata.clear();
            message
        })
        .collect()
}

/// The per-message counterpart of `decompress_then_unpack`: what a route pays to turn
/// a `raw_framed` pair back into a message. `pack` only ever *writes* metadata, so
/// comparing it against `unpack` — which rebuilds it — needs this baseline to be fair.
fn decode_individually(wire: &[(Vec<u8>, Bytes)]) -> usize {
    let mut count = 0;
    for (meta, payload) in wire {
        let mut message = CanonicalMessage::new_bytes(payload.clone(), None);
        message.metadata =
            serde_json::from_slice(meta).expect("metadata frame parses");
        count += message.metadata.len();
    }
    count
}

fn frame_individually_owned(messages: &[CanonicalMessage]) -> Vec<(Vec<u8>, Bytes)> {
    messages
        .iter()
        .map(|message| {
            (
                serde_json::to_vec(&message.metadata).expect("metadata serializes"),
                message.payload.clone(),
            )
        })
        .collect()
}

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

fn config() -> PackMiddleware {
    PackMiddleware {
        format: PackFormat::Mqb,
        max_messages: BATCH,
        max_bytes: 64 * 1024 * 1024,
        drop_message_id: false,
    }
}

/// `pack`, then the `compression` middleware over each physical message — the
/// `[compression, pack]` output pipeline, measured end to end.
fn pack_then_compress(algorithm: Compression, messages: &[CanonicalMessage]) -> Vec<Bytes> {
    let packed = pack_batches(&config(), messages).expect("packs");
    if algorithm == Compression::None {
        return packed;
    }
    packed
        .iter()
        .map(|batch| Bytes::from(compress_member(algorithm, batch).expect("compresses")))
        .collect()
}

/// The reading half: `[unpack, compression]`.
fn decompress_then_unpack(algorithm: Compression, wire: &[Bytes]) -> usize {
    let mut count = 0;
    for batch in wire {
        let plain = if algorithm == Compression::None {
            batch.clone()
        } else {
            Bytes::from(decompress_member(algorithm, batch).expect("decompresses"))
        };
        count += unpack_batch(PackFormat::Mqb, &plain).expect("unpacks").len();
    }
    count
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
        let packed = pack_then_compress(codec, messages);
        let pack_allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let bytes: usize = packed.iter().map(|b| b.len()).sum();

        ALLOCATIONS.store(0, Ordering::Relaxed);
        decompress_then_unpack(codec, &packed);
        let unpack_allocations = ALLOCATIONS.load(Ordering::Relaxed);

        println!(
            "pack/{name:<10} {:>10}  ({:>5.1} B/row)  {:>5} ops  pack {:.3} allocs/row, unpack {:.3} allocs/row, {:.2}x vs individual",
            bytes,
            bytes as f64 / ROWS as f64,
            packed.len(),
            pack_allocations as f64 / ROWS as f64,
            unpack_allocations as f64 / ROWS as f64,
            individual as f64 / bytes as f64,
        );
    }

    // Same envelope, no metadata: the difference is what rebuilding the map costs.
    let bare = corpus_without_metadata();
    ALLOCATIONS.store(0, Ordering::Relaxed);
    let packed = pack_then_compress(Compression::None, &bare);
    decompress_then_unpack(Compression::None, &packed);
    println!(
        "pack/none (no metadata)                            unpack {:.3} allocs/row",
        ALLOCATIONS.load(Ordering::Relaxed) as f64 / ROWS as f64,
    );
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
        group.bench_with_input(BenchmarkId::from_parameter(name), &codec, |b, codec| {
            b.iter(|| pack_then_compress(*codec, &messages));
        });
    }
    group.finish();
}

fn bench_unpack(c: &mut Criterion) {
    let messages = corpus();

    let mut group = c.benchmark_group("unpack");
    group.throughput(Throughput::Elements(ROWS as u64));

    // The per-message baseline, and the same batch with no metadata at all. Together
    // they say how much of unpack's time is the envelope and how much is metadata.
    let individual = frame_individually_owned(&messages);
    group.bench_function("individual", |b| {
        b.iter(|| decode_individually(&individual));
    });
    let bare = pack_then_compress(Compression::None, &corpus_without_metadata());
    group.bench_function("none_no_metadata", |b| {
        b.iter(|| decompress_then_unpack(Compression::None, &bare));
    });

    for (name, codec) in codecs() {
        let wire = pack_then_compress(codec, &messages);
        group.bench_with_input(BenchmarkId::from_parameter(name), &wire, |b, wire| {
            b.iter(|| decompress_then_unpack(codec, wire));
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
        group.bench_with_input(BenchmarkId::from_parameter(name), &codec, |b, codec| {
            b.iter(|| decompress_then_unpack(*codec, &pack_then_compress(*codec, &messages)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_pack, bench_unpack, bench_round_trip);
criterion_main!(benches);
