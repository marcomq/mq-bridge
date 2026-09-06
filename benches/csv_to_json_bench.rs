//! CSV -> JSON decode throughput for the file source.
//!
//! Measures the CPU path only — no file I/O, no runtime — so a change to the row
//! parser shows up undiluted. The corpus mirrors `benches/etl/gen_bench_data.py`:
//! seven mixed-type columns, the last of which is an embedded JSON document, which
//! CSV quotes and whose inner quotes it doubles. That column is the reason the
//! quoted-field path matters as much as the plain one.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mq_bridge::test_utils::bench::{csv_batch_decode, csv_records_to_json};

const ROWS: usize = 20_000;

const FIRST_NAMES: [&str; 16] = [
    "Ada", "Bo", "Chen", "Dinesh", "Eve", "Farah", "Gus", "Hana", "Ivan", "Juno", "Kato", "Lena",
    "Mira", "Nils", "Omar", "Priya",
];
const COUNTRIES: [&str; 10] = ["US", "GB", "DE", "IN", "JP", "BR", "NG", "CA", "AU", "FR"];
const TIERS: [&str; 3] = ["free", "pro", "enterprise"];
const TAGS: [&str; 5] = ["alpha", "beta", "gamma", "delta", "epsilon"];

/// xorshift64*, so the corpus is identical on every machine and every run without
/// pulling in a rand dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The seven-column row of `gen_bench_data.py`, rendered as CSV.
fn generate_csv(rows: usize) -> String {
    let mut rng = Rng(42);
    let mut out = String::with_capacity(rows * 140);
    out.push_str("id,first_name,country,amount,created_at,active,attributes\n");
    for i in 1..=rows {
        let tags = &TAGS[..1 + rng.below(3)];
        // Embedded JSON, CSV-quoted with its own quotes doubled.
        let attributes = format!(
            "{{\"\"tier\"\":\"\"{}\"\",\"\"score\"\":{}.{:03},\"\"tags\"\":[{}]}}",
            TIERS[rng.below(TIERS.len())],
            rng.below(100),
            rng.below(1000),
            tags.iter()
                .map(|t| format!("\"\"{t}\"\""))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str(&format!(
            "{},{},{},{}.{:02},2020-01-01T{:02}:{:02}:{:02}Z,{},\"{}\"\n",
            i,
            FIRST_NAMES[rng.below(FIRST_NAMES.len())],
            COUNTRIES[rng.below(COUNTRIES.len())],
            1 + rng.below(10_000),
            rng.below(100),
            rng.below(24),
            rng.below(60),
            rng.below(60),
            rng.next() % 2 == 0,
            attributes,
        ));
    }
    out
}

/// A row of plain, unquoted fields — the floor the quoted corpus is measured against.
fn generate_plain_csv(rows: usize) -> String {
    let mut rng = Rng(42);
    let mut out = String::with_capacity(rows * 60);
    out.push_str("id,first_name,country,amount,created_at,active,attributes\n");
    for i in 1..=rows {
        out.push_str(&format!(
            "{},{},{},{}.{:02},2020-01-01T00:00:{:02}Z,{},tier-{}\n",
            i,
            FIRST_NAMES[rng.below(FIRST_NAMES.len())],
            COUNTRIES[rng.below(COUNTRIES.len())],
            1 + rng.below(10_000),
            rng.below(100),
            rng.below(60),
            rng.next() % 2 == 0,
            TIERS[rng.below(TIERS.len())],
        ));
    }
    out
}

fn records(csv: &str) -> Vec<&[u8]> {
    csv.lines().map(str::as_bytes).collect()
}

fn bench_csv_to_json(c: &mut Criterion) {
    let mixed = generate_csv(ROWS);
    let plain = generate_plain_csv(ROWS);
    let corpora = [("mixed_quoted", &mixed), ("plain", &plain)];

    let mut group = c.benchmark_group("csv_to_json");
    for (name, csv) in corpora {
        let records = records(csv);
        // The header row establishes columns and yields no message.
        group.throughput(Throughput::Elements(ROWS as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &records, |b, records| {
            b.iter(|| csv_records_to_json(records));
        });
    }
    group.finish();
}

/// The same decode, driven the way the file reader drives it: in batches, which is what
/// lets it spread across cores. Compared against the sequential figure above, the delta
/// is what the split actually buys after paying for itself.
fn bench_csv_batch_decode(c: &mut Criterion) {
    let mixed = generate_csv(ROWS);
    let records = records(&mixed);

    let mut group = c.benchmark_group("csv_batch_decode");
    group.throughput(Throughput::Elements(ROWS as u64));
    for batch in [64usize, 256, 1024, 4096] {
        group.bench_with_input(BenchmarkId::from_parameter(batch), &batch, |b, &batch| {
            b.iter(|| csv_batch_decode(&records, batch));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_csv_to_json, bench_csv_batch_decode);
criterion_main!(benches);
