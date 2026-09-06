# ETL / CDC benchmark harness (run through mq-bridge-app)

This harness produces **like-for-like ETL / data-movement numbers for the
mq-bridge engine, driven the way a real no-code user drives it** — through
`mq-bridge-app` configured by CLI/YAML, not a bespoke Rust harness.

**This file is the single source of truth for every measured ETL/CDC number.** The
library's [`benches/ETL_BENCHMARKS.md`](https://github.com/marcomq/mq-bridge/blob/dev/benches/ETL_BENCHMARKS.md)
states the methodology and reporting rules and links here; it does not carry its own
copy of the results. Update a number here and nowhere else — the book's
[tuning page](../../dev/docs/operations/tuning.md) and
[MCP page](../../dev/docs/MCP.md) pull their tables straight out of this file by
anchor, so they follow automatically.

**The two headline scenarios (§5 Postgres → JSONL and §6 CSV → JSONL)** are
full-dataset ETL jobs on a 1,000,000-row seed-42 dataset, reporting throughput
**and** peak RSS against a Sling and a Meltano (`tap-*` → `target-jsonl`) baseline (see
[Results](#results--the-two-headline-etl-scenarios-1m-rows)). The remaining
scenarios (1 & 3 table→table copy, 2 CDC latency, 4 local IPC, 7 the MCP server,
and 8 Postgres' own tools) are additional coverage, as are the Kafka streaming
comparisons in [C](#c--kafka--jsonl-mq-bridge-app-vs-arroyo-1000000-rows) and
[D](#d--kafka--native-ss-mq-bridge-app-vs-sea-streamer-1000000-rows), which are
reported as results rather than as a numbered scenario.

## Why this path is credible

Competing ETL/CDC tools publish "config in → data moved out" numbers. So do we:
scenarios 1 & 3 run through the app's zero-code `copy` command (a Postgres table
→ Postgres table job); scenario 2 runs a `postgres_cdc → null` route from a YAML
config applied over the app's HTTP API — the same action as clicking *Save* in
the UI. No scenario uses a Criterion micro-harness or hand-written Rust.

## Results — the two headline ETL scenarios (1M rows)

The two most common ETL jobs — **CSV → JSONL** and **Postgres → JSONL** — run on
the same seeded (seed 42) 7-column mixed-type dataset of 1,000,000 rows, against two
baselines — Sling (a compiled Go EL tool) and Meltano (`tap-*` → `target-jsonl`) —
reporting throughput **and** peak RSS (detailed writeups in §5 and §6 below). All
columns are measured on the same machine (this repo's Apple M1 host).

> **These A/B tables are a 0.4.10-era, single-session comparison, kept intact so the
> ratios against Sling and Meltano stay internally consistent.** mq-bridge's own
> throughput was re-measured on 0.4.12 — CSV untyped 2,824,858 rows/s, CSV typed
> 1,636,661 rows/s, Postgres 421,220 rows/s — and those figures are in
> [Reference numbers](#reference-numbers-all-scenarios-at-a-glance). The baselines were
> not re-run in that session, so the ratios below are not recomputed from it.

### A — CSV → JSONL (1,000,000 rows, ~116 MiB)

mq-bridge-app is measured **twice**, because the two configurations do different
work and each has a baseline it legitimately belongs against:

| Metric            | mq-bridge-app (typed) | mq-bridge-app (untyped) | Sling           | Meltano        |
| ----------------- | --------------------- | ----------------------- | --------------- | -------------- |
| Throughput        | **742,390 rows/s**    | **1,133,786 rows/s**    | 127,959 rows/s  | ~19,500 rows/s |
| Median wall-clock | 1.35 s                | 0.88 s                  | 7.82 s          | ~51 s          |
| Peak RSS          | 93.8 MiB              | 21.9 MiB                | 111.2 MiB       | 443.8 MiB      |
| Rows out          | 1,000,000             | 1,000,000               | 1,000,000       | 1,000,000      |

- **typed** runs a `transform` middleware that reproduces Sling's typing exactly,
  and the harness fails the run unless all 1,000,000 output records match. **This
  is the column to read against Sling** (~5.8x), and the only one that is
  like-for-like.
- **untyped** runs no middleware: every field stays the string the CSV reader
  produced. This is the column to read against **Meltano** (~58x), whose `tap-csv`
  likewise emits every field as a string — and it is the floor the transform's cost
  is measured against.

Quoting the untyped figure against Sling would overstate the margin (it would read
~8.9x); quoting the typed figure against Meltano understates it. See
[A note on the Sling comparison](#a-note-on-the-sling-comparison). Peak RSS
(measured with `/usr/bin/time -l`): untyped **21.9 MiB**, typed **93.8 MiB** — the
transform's per-row JSON decode/buffer adds ~72 MiB, still well under Meltano's
443.8 MiB and 17.4 MiB under Sling's 111.2 MiB even on the typed path.

Both mq-bridge-app columns are measured with the **mimalloc** global allocator,
which the shipped binaries use. It is the `mimalloc` cargo feature — on by
default, and also pulled in by the `bench` feature, so both documented builds
have it. To measure the system allocator instead, build with
`--no-default-features --features mq_bridge_app/bench` (the core feature set
without the app's `bench` alias). On this scenario it is worth ~+36% throughput for
~+33% peak RSS on the typed path; the Sling and Meltano columns are unaffected by
it. Sections A, B and C, §4, §7 and §8 are all mimalloc numbers. **§1/§3 (table→table)
and §2 (CDC latency) have not been re-measured** and still report system-allocator
figures — they are marked where they appear.

### B — Postgres → JSONL (1,000,000 rows, 7-col)

| Metric            | mq-bridge-app       | Sling           | Meltano        |
| ----------------- | ------------------- | --------------- | -------------- |
| Throughput        | **338,066 rows/s**  | 122,774 rows/s  | 15,356 rows/s  |
| Median wall-clock | 2.96 s              | 8.15 s          | 65.1 s         |
| Peak RSS          | 39.8 MiB            | not yet measured| 599.7 MiB      |
| Rows out          | 1,000,000           | 1,000,000       | 1,000,000      |

`mq-bridge-app` moves the same 1M rows at **~40 MiB peak RSS** — an order of
magnitude leaner than Meltano's Python pipeline (444–600 MiB). Throughput and RSS
are single-machine, single-process batch numbers.

The mq-bridge-app column was re-measured on 2026-08-01 with the mimalloc allocator
(2 repeats after a discarded warmup, batch 1024 / concurrency 1). The Sling and
Meltano columns are carried over from the earlier session on the same machine and
dataset — neither tool changed, but this table is therefore not single-session.
Raising concurrency to 4 gives **384,615 rows/s** at 41.2 MiB.

> **The two Sling columns are not on equal footing — check which one you're
> reading.** In **A (CSV)** the *typed* mq-bridge-app column does the same per-row
> work as Sling: it runs a `transform` middleware reproducing Sling's typing, and
> the outputs are asserted identical row-for-row, so that ratio is like-for-like.
> (A's *untyped* column is not — it belongs against Meltano.) In **B (Postgres)**
> the comparison is not equalised at all: there mq-bridge-app passes values through
> untyped while Sling does schema inference and type conversion, which is real work
> it does and mq-bridge-app doesn't. B has no typed column yet.
> See [A note on the Sling comparison](#a-note-on-the-sling-comparison).

### C — Kafka → JSONL: mq-bridge-app vs. Arroyo (1,000,000 rows)

This is a deliberately narrow streaming comparison, not a third headline ETL
scenario. Both sides consume the same four-partition Kafka topic containing the
usual seven-column JSON row, project the same four columns (`id`, `first_name`,
`country`, `amount`), and write newline-delimited JSON. The run uses four-way
parallelism and Arroyo `0.15.0` (`ghcr.io/arroyosystems/arroyo:0.15.0`).

| Tool | Median wall-clock | Throughput | Startup | Peak RSS |
| ---- | ----------------: | ---------: | ------: | -------: |
| mq-bridge-app passthrough (no transform) | 1.244 s | 803,640 rows/s | — | 105 MiB |
| mq-bridge-app projection (+ `transform`) | 1.472 s | 679,546 rows/s | — | 93 MiB |
| Arroyo projection | 1.764 s | 566,991 rows/s | 0.548 s | 362 MiB |

The mq-bridge-app rows are fresh results from 1 warmup + 3 timed runs with the
explicit release feature set `bench,kafka,mimalloc`. Arroyo's row is the existing
retained result (1 warmup + 5 timed runs), not a new Arroyo run. Both tools use
the same stopwatch: start the job and poll until all 1,000,000 output rows have
landed; Arroyo's one-off pipeline startup is reported separately. Output parity
was checked before quoting the comparison: both projected sinks were 65,615,161
bytes and contained the same 1,000,000 records. Arroyo's stateful features are
intentionally not exercised.

The delivery guarantees are not equivalent: Arroyo provides **exactly-once
processing within its checkpointed pipeline**, while mq-bridge-app provides
**at-least-once delivery** for the Kafka route. mq-bridge-app resumes from the
source's committed consumer offset, so a failure can replay records; the
`transform` measurement does not add deduplication or upgrade that guarantee.

The exact mq-bridge-app build command was:

```bash
cargo build -p mq-bridge-app --no-default-features \
  --features bench,kafka,mimalloc --release
```

The `mimalloc` allocator is included by the app's `bench` feature; do not compare
these rows with a system-allocator build.

### D — Kafka → native `.ss`: mq-bridge-app vs. Sea Streamer (1,000,000 rows)

This is a separate library/backend comparison. Both tools relay the original
Kafka payload from the same four-partition, 1,000,000-row topic without a
transform. mq-bridge-app writes its default `format=normal` file encoding: a
JSON `CanonicalMessage` envelope per record. Sea Streamer writes its native
framed and indexed `.ss` file. The Sea Streamer path uses the pinned official
`0.5.2` crates.

| Tool | Median wall-clock | Throughput | Peak RSS | Sink bytes |
| ---- | ----------------: | ---------: | -------: | ---------: |
| mq-bridge-app (`format=normal`, mimalloc) | 1.354 s | 738,297 rows/s | 109 MiB | 391,985,602 |
| Sea Streamer 0.5.2 relay (native `.ss`, system allocator) | 2.434 s | 410,786 rows/s | 689 MiB | 244,991,425 |
| Sea Streamer 0.5.2 relay (native `.ss`, mimalloc) | 2.235 s | 447,356 rows/s | 835 MiB | 244,991,425 |

All rows are fresh results from 1 warmup + 3 timed runs, using the same
stopwatch. Each Sea Streamer result was run with the repository-contained helper
and its output was verified with `sea-streamer-count` to contain exactly
1,000,000 messages. Its mimalloc build is a separate application-level allocator
measurement, not a Sea Streamer crate feature. The two native file formats are
not byte-for-byte equivalent, so the data supports a Kafka-to-file throughput
comparison—not a claim of identical sink encoding, delivery, or checkpoint
semantics. On these runs mq-bridge-app is 1.80x faster than Sea Streamer's default
allocator result and 1.65x faster than its mimalloc result.

The reproducible Sea Streamer helper is committed in
[`benches/etl/sea_streamer`](sea_streamer). It contains the relay and count
programs and pins the Sea Streamer `0.5.2` dependencies:

```bash
cargo build --manifest-path benches/etl/sea_streamer/Cargo.toml \
  --release --target-dir target
```

To add the allocator comparison, rebuild the same helper in the same target
directory with `--features mimalloc`, then run
`SEA_STREAMER_LABEL=sea-streamer-mimalloc REPEATS=3 ./benches/etl/run_kafka_stream.sh sea`.

## Reference numbers (all scenarios, at a glance)

Cross-scenario summary on this repo's Apple M1 (8 cores, 8 GB RAM), `mq-bridge-app`
built with default features (`full`, which includes mimalloc — the allocator the
shipped binaries use). **Numbers are hardware-dependent — treat them as shape, not
guarantees.** The `Version` column is the point of this table: rows are not all from
one session, and a row must never be quoted without it.

<!-- ANCHOR: reference_numbers -->
| Scenario | Batch | Conc. | Throughput | Peak RSS | Version |
|---|---|---|---|---|---|
| CSV → JSONL (strings passthrough, 1M rows ~116 MiB) | 1024 | 1 | **2,824,858 rows/s** | ~28 MiB | 0.4.12 |
| CSV → JSONL, same job driven through an MCP tool call | 1024 | 1 | **2,348,793 rows/s** | — | 0.4.12 |
| CSV → JSONL with typing `transform` (id→int, embedded JSON) | 1024 | 1 | **1,636,661 rows/s** | ~68 MiB | 0.4.12 |
| IPC forward (`static` → `memory`, Unix socket) | 1024 | 1 | **1,619,135 rows/s** | — | 0.4.12 (300 s window; 1,594,722 on 0.4.11) |
| Postgres → JSONL (1M rows, 7 mixed-type cols) | 1024 | 4 | **480,538 rows/s** | ~55 MiB | 0.4.12 |
| Postgres → JSONL (same) | 1024 | 1 | **421,220 rows/s** | ~46 MiB | 0.4.12 |
<!-- ANCHOR_END: reference_numbers -->

The 0.4.12 rows were measured on 2026-09-06 on a settled machine (no build and no
indexer running; load average under 2.5 entering each block), on the default (`full`)
release build. CSV: 3 timed runs after a discarded warm-up (untyped 0.354 s ±0.001,
typed 0.611 s ±0.020). MCP: 6 runs and **no discarded warm-up** — `mcp_bench.py` times
every run including the cold first one, so a 2- or 3-run median reads materially low.
Postgres: 10 timed runs across two sessions at concurrency 1 (median 2.374 s, spread
2.190–2.509 s) and 3 runs at concurrency 4 (2.081 s ±0.132). IPC: one 300 s window.
Peak RSS was re-measured in this session rather than carried over; the IPC scenario
reports a sustained rate sampled from the receiver's transport log rather than a
wall-clocked job, so it has no RSS column and is not comparable cell-for-cell with the
rest of the table. The Postgres figure is measured against a
**freshly seeded** `bench` table on a freshly created container; the same scenario read
369,412 rows/s against a table left in place from earlier runs, so reseed before
quoting it. See §4, §5 and §6 for the full write-ups and the Sling / Meltano
baselines.

> **`--concurrency 1` doesn't mean one thread.** It is one route *worker*. CSV decoding is spread across a shared pool, so the
> untyped run uses ~2.4 cores (0.61 s user + 0.13 s sys against 0.31 s wall) and the
> typed run ~3.9. Any comparison against a baseline pinned to one thread has to say so.

Three things the table shows:

- **Typing has a real but modest cost.** Adding a `transform` that coerces `id` to an
  integer and decodes an embedded JSON document costs ~0.26 µs/row — CSV→JSONL drops
  from 2.82M to 1.64M rows/s, about 1.73x, but every output record is fully typed.
- **Postgres → JSONL did not improve, and at concurrency 1 it read ~6% below the
  0.4.11 figure** (421,220 vs 446,830). Two independent 5-run sessions agreed
  (415,973 and 426,803), so this is not a one-off, but it is inside the session-to-
  session spread this scenario is already documented to have. The scenario is bound by
  the Postgres read, not by the file sink the 0.4.12 work touched. Concurrency 4, by
  contrast, went from 384,615 to 480,538 and now overtakes concurrency 1, which it did
  not before.
- **Peak RSS does not scale with dataset size**, because rows stream in batches rather
  than being buffered whole — ~28 MiB for a passthrough copy however large the input. It
  is not a constant: batch size, connector-side buffering, allocator retention and
  transforms all move it. The typing `transform` is the exception: its per-row JSON
  decode and buffering push peak RSS to ~68 MiB, still far leaner than tools that
  materialize the dataset.

### Same-session baseline: DuckDB

DuckDB, on the same file in the same session, is the reference for how fast this
machine can turn this CSV into JSON *at all*. It is not an ETL tool and offers none of
the delivery semantics mq-bridge does — no at-least-once, no checkpointing, no
arbitrary sinks — so this is a throughput ceiling, not a like-for-like product
comparison. `all_varchar=true` matches the untyped mq-bridge column: strings in,
strings out. Run with [`run_csv_duckdb.sh`](run_csv_duckdb.sh).

| Tool | Threads / cores used | Throughput | Peak RSS |
|---|---|---|---|
| mq-bridge-app `copy`, untyped | `--concurrency 1`, ~2.4 cores | **2,824,858 rows/s** (0.354 s ±0.001) | ~28 MiB |
| DuckDB, all cores (default) | ~3.7 cores | 2,036,659 rows/s (0.491 s ±0.015) | ~467 MiB |

mq-bridge was **~1.39x faster than DuckDB running on all of this machine's cores**,
while using fewer cores and ~17x less memory. Both sides are multi-threaded here (see
the note above), so the core counts belong next to the rates.

## Fixed parameters (printed next to every number)

| Parameter     | Value                                             |
| ------------- | ------------------------------------------------- |
| Payload       | 256 B and 4 KiB JSON rows (both reported)         |
| Message count | 1 000 000 per run                                 |
| Batch sizes   | 1 / 128 (table→table §1 & §3); 1024 (§5, §6 & the Kafka runs in C/D); 32768 (§8) |
| Concurrency   | 1 and 4 route workers                             |
| Postgres      | `postgres:16-alpine`, `wal_level=logical`         |
| Warm-up       | 5 000-message pre-roll, excluded from timing      |
| Environment   | CPU model, cores, RAM, mq-bridge(-app) version    |

Payload rows are `{"id":<n>,"pad":"xxx…"}` padded to exactly 256 / 4096 bytes.

## One-time setup

```bash
# 1. Release binary. The published §6 CSV numbers are measured on the DEFAULT
#    (`full`) release build — the same artifact shipped via Homebrew and
#    cargo-binstall — so the figures describe what a user actually installs.
cargo build -p mq-bridge-app --release

#    A lean build is also supported and is enough for the Postgres scenarios;
#    it avoids the heavy `full` deps (rdkafka/librdkafka, grpc/protoc, ibm-mq)
#    and builds much faster. Numbers from the two builds should not be mixed in
#    one table. Give it its own CARGO_TARGET_DIR so it cannot overwrite the
#    default build's binary, and pass the same value when running the
#    benchmarks against it (the runners resolve BIN from it):
# CARGO_TARGET_DIR=target-lean cargo build -p mq-bridge-app \
#   --no-default-features --features bench --release
# CARGO_TARGET_DIR=target-lean benches/etl/run_throughput.sh
#
#    For run_cdc_latency.sh use `bench-cdc` instead: CDC needs the postgres
#    logical-replication endpoint, which pulls aws-lc-sys (a slow C build) and
#    is therefore not in plain `bench`.
# CARGO_TARGET_DIR=target-lean cargo build -p mq-bridge-app \
#   --no-default-features --features bench-cdc --release

# 2. Postgres 16 with logical replication (mirrors the library's CDC compose).
benches/etl/seed.sh up

# 3. Optional — the Sling CLI baseline for §5/§6 (compiled Go EL tool).
#    Kept repo-local so a run never depends on what's on PATH; both runners
#    skip the Sling block if it's absent.
mkdir -p benches/etl/bin && curl -sL \
  "https://github.com/slingdata-io/sling-cli/releases/latest/download/sling_darwin_arm64.tar.gz" \
  | tar -xz -C benches/etl/bin sling
```

Requires Docker, `curl`, `python3` (sub-second timing), and `uv` (used by
`seed.sh` to run `gen_bench_data.py` for the §5/§6 dataset) on PATH. A host `psql`
is used if present; otherwise the harness runs `psql` inside the compose container,
so no host Postgres client is needed.

## Scenarios & commands

### 1 & 3 — bulk-insert / batched-vs-unbatched throughput

`copy` reads a seeded 1 000 000-row Postgres table and inserts into a fresh table,
wall-clocked. `rows/sec = 1 000 000 / elapsed`. The runner sweeps the full
payload × batch × concurrency matrix and re-seeds before each timed run:

```bash
benches/etl/run_throughput.sh          # -> benches/etl/results/throughput.csv
```

The single underlying command (what a user would type) is:

```bash
mqb copy \
  --from 'postgres://testuser:testpass@localhost:5432/testdb?table=src_256&cursor_column=id&sslmode=disable' \
  --to   'postgres://testuser:testpass@localhost:5432/testdb?table=dst_256&auto_create_table=true&sslmode=disable' \
  --drain --batch-size 128 --concurrency 4
```

`cursor_column=id` reads the source table non-destructively, paging on the
monotonic `id` column (the sqlx cursor reader — an incremental-sync read like
Airbyte's). Each moved record is the source row as JSON
(`{"id":N,"payload":"…"}`); the `payload` column is sized to exactly 256 B / 4 KiB.
Scenario 3 (the batching lever) is the `batch=1` vs `batch=128` rows of the same
matrix. The library's Criterion harness additionally covers the `memory` backend.

### 2 — CDC event-to-sink latency

A `postgres_cdc → null` route ([`cdc_latency.yaml`](cdc_latency.yaml)) with the
`metrics` middleware on the input endpoint. The runner boots the app with
`--config cdc_latency.yaml`, then **starts the route via `POST /consumer-start`**
— the zero-code equivalent of clicking *Start* in the UI. (Consumers do **not**
auto-start headless, and `POST /config` only validates+saves; it does not start
routes.) It then inserts rows into the captured table, waits for the processed
counter to settle, and reads the `queue_message_processing_duration_seconds`
summary quantiles:

```bash
benches/etl/run_cdc_latency.sh         # -> benches/etl/results/cdc_latency.csv
```

Two behaviours to know (validated in a smoke run):

- **Use a permanent replication slot** (`temporary_slot: false`, as in the YAML):
  a temporary slot is dropped when its creating connection closes and races the
  streaming connection (`replication slot "…" does not exist`). The runner drops
  the slot before each run for a clean start.
- **CDC delivers at ~transaction granularity:** one multi-row transaction arrives
  as a *single* processed message. So `queue_messages_processed_total` counts
  delivered messages, not rows, and to get N change events (each with its own
  latency sample) you insert **N single-row transactions**, not one bulk
  `generate_series`. The runner waits for the counter to *stop increasing* rather
  than reaching an exact row total.

**What this measures (be honest when publishing):** the metric is the engine's
**per-event in-engine processing time on the consumer side** (decode + hand-off),
exposed as Prometheus summary quantiles. It is *not* the full Postgres
commit → sink wall-clock that Debezium reports end-to-end; it excludes
replication-slot/WAL propagation delay. Report it as "mq-bridge CDC processing
latency (p50/p95/p99)" and note the difference rather than implying it is
identical to Debezium's end-to-end figure. New CDC endpoint schemes were also
added to `copy` (`postgres-cdc://… → null:`), which gives a wall-clock CDC
*throughput* cross-check if you want one.

### 4 — Local IPC throughput (Unix domain socket, no metrics middleware)

A `static` load generator (process A) forwards messages to a receiver (process B)
over a real Unix domain socket — a `memory:` endpoint with a `unix://` topic, so
this is genuine cross-process IPC, not just an in-process channel
([`ipc_sender.yaml`](ipc_sender.yaml) / [`ipc_receiver.yaml`](ipc_receiver.yaml)).
Both sides run with `batch_size: 1024, concurrency: 1`. After starting both routes
via `POST /consumer-start` (same zero-code "Start" action as scenario 2) and a
2-second settle window, `rows/sec` is the delta of the receiver's own per-batch
transport log (`count=`/`bytes=` from `ipc_unix.rs`, no metrics crate involved)
over a 15-second sampling window:

```bash
benches/etl/run_ipc_throughput.sh      # -> benches/etl/results/ipc_throughput.csv
```

**Latest result: 1,619,135 rows/s** (mq-bridge 0.4.12) at `concurrency: 1`, sustained
over a 5-minute window — `RUN_SECONDS=300 benches/etl/run_ipc_throughput.sh`, i.e. the
same script with the sampling window widened from its 15-second default. That is flat
against 0.4.11's 1,594,722 rows/s, which is the expected result: the 0.4.12 throughput
work was in the file endpoint, transform, filter and batch decode, and none of it is on
this path. The 0.4.9 run measured 1,769,700 rows/s over the same window
(system-allocator baseline on that run: 1,207,906 rows/s).

> **This scenario recorded nothing at all between 0.4.9 and 0.4.11.** `kill_pids` in
> `lib.sh` used `((waited++))`, which evaluates to the old value and so exits 1 at
> zero — under `set -e` that aborted the runner after it had measured but before it
> wrote the result row, and the EXIT trap still returned 0. Fixed, and covered by
> [`lib_test.sh`](lib_test.sh), which CI now runs.

An earlier
pre-mimalloc session measured 1,202,926 rows/s at `concurrency: 1`; `concurrency: 4`
gave a very slightly *lower* 1,167,428 rows/s there — expected, since both sides talk over a
single Unix socket connection either way, serialized through one mutex-guarded
stream; extra worker concurrency here only adds channel-handoff overhead with no
real parallelism to gain.)

### 5 — Postgres → JSONL vs. Meltano (`tap-postgres` → `target-jsonl`)

Same source table (`bench`: 1,000,000 rows, 7 mixed-type columns — `id,
first_name, country, amount, created_at, active, attributes` — seed 42, the same
dataset scenario 6 reads as CSV), same Postgres instance, same machine, both
one-shot full-table syncs to a local JSONL file. Both sides run through
[`run_meltano_bench.sh`](run_meltano_bench.sh) (1 warm-up + timed runs each,
median/stddev, output row count verified against the 1,000,000-row source):

```bash
benches/etl/run_meltano_bench.sh   # both sides -> results/meltano_pg_to_jsonl.csv
```

```bash
mqb copy \
  --from 'postgres://testuser:testpass@localhost:5432/testdb?table=bench&cursor_column=id&sslmode=disable' \
  --to   'file:///tmp/mqb_bench_out.jsonl?format=raw' \
  --drain --batch-size 1024 --concurrency 1
```

Meltano side: `benches/etl/meltano_project/bench` (`meltano.yml` configures
`tap-postgres` selecting only `public-bench.*`, and `target-jsonl`), run via
`meltano run tap-postgres target-jsonl`, wall-clocked the same way.

Sling side: `sling run --src-conn … --src-stream public.bench --tgt-object file://…`
with `{"format":"jsonlines","file_max_rows":0}`, defaults otherwise.

| Tool | Config | rows/s | peak RSS |
| --- | --- | --- | --- |
| mq-bridge-app `copy` | batch_size 1024, concurrency 1 | **338,066** (2-run median 2.958 s, ±0.053) | 39.8 MiB |
| mq-bridge-app `copy` | batch_size 1024, concurrency 4 | **384,615** | 41.2 MiB |
| Sling | defaults | 122,774 | not yet measured |
| Meltano (`tap-postgres` → `target-jsonl`) | default Singer config | 15,356 | 599.7 MiB |

**mq-bridge-app is ~2.8x faster than Sling and ~22x faster than Meltano** (and ~15x
leaner than Meltano in peak memory) in this scenario — but see
[the note on Sling below](#a-note-on-the-sling-comparison), because that ratio is
not comparing equal work. No `metrics` middleware
involved here either — the `copy` CLI command never attaches a handler (see
scenario 4's CommandPublisher bug/fix note), so there's no metrics path to
accidentally measure in the first place.

The mq-bridge-app figures were re-measured on 2026-08-01 with mimalloc (2 timed
runs, 2.958s ±0.053, after a discarded warmup). The Sling figure (8.145s ±0.155)
and the Meltano figure are carried over from earlier sessions on the same machine
and dataset — neither tool changed, but this table is not single-session.

### 6 — CSV → JSONL vs. Meltano

A common ETL benchmark is a CSV → JSONL conversion. Same seeded dataset (`data/bench.csv`: 1,000,000
mixed-type rows — `id, first_name, country, amount, created_at, active,
attributes` — seed 42, ~116 MiB, generated by [`gen_bench_data.py`](gen_bench_data.py)),
same machine, both one-shot full-file CSV → local JSONL, one JSON object per
input row (string-valued fields — the shape both mq-bridge's CSV reader and
tap-csv emit):

```bash
benches/etl/run_csv_to_jsonl.sh   # all tools -> benches/etl/results/csv_to_jsonl.csv
benches/etl/run_csv_duckdb.sh     # DuckDB ceiling, appended to the same results CSV
```

`run_csv_to_jsonl.sh` does not chain the DuckDB run — DuckDB is a ceiling reference
rather than an ETL baseline, so it is invoked on its own. Its numbers are in
[Same-session baseline: DuckDB](#same-session-baseline-duckdb).

Each tool is also a standalone script, so a single number can be re-measured
without re-running the matrix. They append to the same results CSV, each replacing
only its own row:

```bash
benches/etl/run_csv_mqb.sh              # mq-bridge-app, typed (transform)
benches/etl/run_csv_mqb.sh --untyped    # mq-bridge-app, untyped (no middleware)
benches/etl/run_csv_sling.sh
benches/etl/run_csv_meltano.sh
benches/etl/run_csv_parity.sh           # asserts typed output == Sling's
```

The two mq-bridge-app configurations differ only in the middleware suffix on the
output endpoint:

```bash
# untyped — every field stays the string the CSV reader produced
mqb copy \
  --from 'file:///…/benches/etl/data/bench.csv?format=csv' \
  --to   'file:///tmp/mqb_csv_out.jsonl?format=raw' \
  --drain --batch-size 1024 --concurrency 1

# typed — reproduces Sling's typing, and is what §6's Sling ratio compares
mqb copy \
  --from 'file:///…/benches/etl/data/bench.csv?format=csv' \
  --to   'file:///tmp/mqb_csv_out.jsonl?format=raw|transform?schema_file=…/schemas/bench.json' \
  --drain --batch-size 1024 --concurrency 1
```

Meltano side: same `meltano_project/bench` project with a `tap-csv` extractor
(pinned to `tap-csv 1.3.2`) → `target-jsonl 0.1.4`, run via
`meltano run tap-csv target-jsonl`. Install the plugin once with
`(cd meltano_project/bench && ../.venv/bin/meltano install extractor tap-csv)`.

Sling side: `sling run --src-stream file://…bench.csv --tgt-object file://…` with
`{"format":"jsonlines","file_max_rows":0}`, defaults otherwise.

| Tool | Config | rows/s | peak RSS |
| --- | --- | --- | --- |
| mq-bridge-app `copy` (typed) | batch_size 1024, concurrency 1, `transform` (schemas/bench.json) | **742,390** (2-run median 1.347s, ±0.001) | 93.8 MiB |
| mq-bridge-app `copy` (untyped) | batch_size 1024, concurrency 1, no middleware | **1,133,786** (2-run median 0.882s, ±0.001) | 21.9 MiB |
| Sling | defaults | 127,959 (2-run median 7.815s, ±0.077) | 111.2 MiB |
| Meltano (`tap-csv` → `target-jsonl`) | default Singer config | ~19,500 (clean runs 49.7s / 53.1s; fuller median pending) | 443.8 MiB |

**Typed vs Sling: ~5.8x** — equal work, asserted identical outputs. **Untyped vs
Meltano: ~58x** — both emit strings. Those are the two defensible ratios; see
[the note below](#a-note-on-the-sling-comparison) for why they are not
interchangeable.

The typed cost is explicit here rather than hidden in a single number: the
transform adds **0.47 s per 1,000,000 rows (~0.47 µs/row, ~35% of the typed
wall-clock)**. An earlier revision of the transform cost 2.10 s per 1M rows (2.1
µs/row, 64%), which is why the previously published typed figure was 303,214.

**The two mq-bridge-app columns are not from the same session as the Sling and
Meltano columns.** mq-bridge-app was re-measured on 2026-08-01 after the switch to
the mimalloc allocator (2 repeats each after a discarded warmup, spread ±0.001 s);
Sling and Meltano are carried over from earlier sessions on the same machine and
dataset (Sling 2026-07-19). Sling has been a stable control across three sessions
here — 119,217 / 128,766 / 127,959 rows/s — which is the basis for treating the
carried-over columns as still valid, but a same-session re-run of all four columns
is the cleaner way to publish these ratios and has not been done yet.

For reference, the pre-mimalloc figures this table replaced were 540,248 typed /
784,313 untyped, measured 2026-07-19 in a back-to-back session with Sling; the
untyped figure was measured three times there (788,022 / 784,313 / 768,639 rows/s),
a ~2.5% spread that is the honest run-to-run variance on this host.

### A note on the Sling comparison

Sling does schema inference and type conversion: given a CSV row it emits typed
values and re-parses nested JSON into real objects. mq-bridge-app's readers do not
infer types — they pass values through as strings. Comparing those two directly
would time a string passthrough against a tool that parses and re-types every row,
which is not a benchmark, so the two scenarios handle it differently.

**§6 (CSV → JSONL) — equal work, ratio is like-for-like.** The mq-bridge-app run
carries a `transform` middleware ([`schemas/bench.json`](schemas/bench.json)) that
reproduces Sling's output exactly: `coerce` widens the `id` string to an integer,
and `contentMediaType: application/json` decodes the embedded `attributes` document
into a nested object. Both tools then emit the same records:

```jsonc
// sling (defaults) and mq-bridge-app (+ transform) — identical records
{"id":1,"amount":"6767.32","attributes":{"score":2.501,"tier":"free"}}
```

This is asserted, not assumed: [`compare_jsonl.py`](compare_jsonl.py) diffs the two
outputs record-by-record and **fails the run** on any mismatch, so the numbers in §6
cannot be published unless all 1,000,000 rows match. Records are compared as parsed
JSON — mq-bridge-app preserves the source column order inside `attributes` while
Sling alphabetizes it, which is a serialization difference, not a data one.

Making the work equal is not free, and the harness measures the cost directly by
running mq-bridge-app both ways in the same session: **1,133,786 untyped → 742,390
typed**, i.e. the transform costs ~0.47 µs/row (~35% of wall-clock). The margin
against Sling is therefore ~5.8x, not the ~8.9x the untyped run would suggest.

Both configurations stay published on purpose. The untyped number is not a
discarded draft — it is the correct comparison against any tool that also does no
transformation (Meltano's `tap-csv` emits every field as a string), and it is what
makes the transform's cost auditable instead of baked invisibly into one figure.
What it must *not* be used for is a comparison against a type-inferring tool.

Historical note: this scenario previously published 303,214 rows/s typed, against
an untyped 834,724 — a ~2.7x cost for the same schema. A later optimization of the
transform middleware cut that to ~1.45x. Sling is a useful control across all of
it, measuring 119,217 → 128,766 → 127,959 rows/s in three separate sessions on this
machine, so the movement in the typed number is real engine work rather than a
drifting baseline.

**§5 (Postgres → JSONL) — not yet equalised.** That scenario still runs
mq-bridge-app untyped against a type-inferring Sling, so part of its ~2.8x gap is
mq-bridge-app doing less. Read it with that attached. Applying the same treatment
there needs a Postgres-shaped schema (the driver already returns typed values for
some columns, so it is not a copy of `bench.json`) and a re-run.

**A note on this benchmark:**

- **The CSV number does not scale with batch size or concurrency.** A `file://`
  source is a single sequential reader, so extra route workers can't parallelize
  the read; the bottleneck is per-row CSV→JSON conversion on the one reader, not
  I/O or batching. After the CSV-path optimization and the move to mimalloc this
  untyped baseline sits at ~1.13M. The raw byte-passthrough `file→file` reference
  it used to be compared against (~880k, no CSV decode) predates mimalloc and has
  not been re-run, so the two are no longer directly comparable.

### 7 — MCP server: tool-call latency, route throughput, agent token cost

Every scenario above drives the engine from a CLI or a YAML config. This one
drives it the way an **LLM agent** does: through the MCP server, over its real
stdio transport. [`mcp_bench.py`](mcp_bench.py) is a genuine MCP client — it spawns
`mqb mcp --transport stdio` and speaks JSON-RPC to it exactly as Claude
Code would, so the numbers include the framing, serialization and process-boundary
cost an agent actually pays. Nothing reaches into the process.

The job is the **same CSV → JSONL work as §6, on the same 1M-row dataset**, on
purpose: the question is not how fast the MCP server is in isolation, but whether
routing a job through a tool call costs anything per row versus the `copy` CLI. A
different workload would answer nothing.

```bash
benches/etl/run_mcp_bench.sh          # -> results/mcp_bench.json (+ a row in results/csv_to_jsonl.csv)
REPEATS=3 LATENCY_CALLS=1000 benches/etl/run_mcp_bench.sh
```

<!-- ANCHOR: mcp_results -->
| Measurement | Result |
| --- | --- |
| Tool-call round-trip latency (200 calls) | **p50 0.051 ms** · p95 0.060 ms · p99 0.277 ms |
| 1M-row CSV → JSONL via `start_route` (client wall-clock, 6 runs) | **2,348,793 rows/s** (0.426 s ±0.027) |
| Same job, server's own `average_messages_per_second` | no longer usable at this speed — see the note below |
| `copy` CLI baseline, same dataset and session (§6 untyped) | 2,824,858 rows/s (3-run median 0.354 s, ±0.001) |
| Fixed cost of going through the MCP interface | **~72 ms** |
| Agent tool traffic to move the whole dataset | **1,540 bytes** (~385 tokens, 3 calls) |
| The same 116.3 MiB through a model's context | ~30.5M tokens |
<!-- ANCHOR_END: mcp_results -->

- **The MCP interface costs one round-trip, not a per-row tax.** Client wall-clock is
  0.426 s against the CLI's 0.354 s on the same dataset in the same session — a fixed
  **~72 ms**, not a rate difference. Most of it is the client's own polling: it asks
  `route_status` every 50 ms (`POLL_INTERVAL_S` in `mcp_bench.py`), so completion is
  seen 0–50 ms late, ~25 ms on average. The two round trips that bracket the job cost
  ~0.05 ms each at p50. The comparison is in fact tilted *toward* MCP: the harness
  spawns a fresh `mqb copy` process for every CLI run, while the MCP server is spawned
  once and reused, so the CLI figure carries process startup the MCP path never pays.
  Scale the dataset and the ~72 ms stays put.
- **Run the MCP scenario with enough repeats.** `mcp_bench.py` discards no warm-up —
  every run is timed, including the cold first one. At `REPEATS=3` this measured
  1,831,609 rows/s off a monotonically warming series (1.48M → 1.83M → 2.03M); at
  `REPEATS=6` the trend is gone and the median is 2,348,793. Use 6 or more.
- **Ignore the server's own `average_messages_per_second` here.** The route sampler
  ticks every 200 ms and the whole job now finishes in about two ticks, so that field
  quantises — it reported anything from 2.4M to 5.1M rows/s across runs of nearly
  identical wall-clock. Client wall-clock is the only meaningful figure at this speed.
- **Agent token cost is flat in the number of rows moved** — the one row here with
  no CLI counterpart. An agent moves the dataset with three tool calls
  (`start_route`, one `route_status`, `stop_route`) totalling 1,540 bytes of
  JSON-RPC. The rows never enter the model's context, so the same ~1.5 KB moves 1M
  rows or 1,000 — only the digits of the counters differ. Passing the 116.3 MiB
  through a context window instead would cost ~30.5M tokens (~4 bytes/token, an
  estimate), which no context window holds at any price.

<details>
<summary><b>Methodology notes</b> — so nothing here is quotable out of context</summary>

- The throughput number is **client-side wall-clock** from the `start_route` call
  to observing `finished: true`, including route startup and completion-detection
  lag. It is the pessimistic figure of the two, which is why it is the one quoted.
- The server-side figure is `average_messages_per_second` (total messages over the
  span in which they moved), **not** the instantaneous `messages_per_second`, which
  decays to ~0 within a second of a drain finishing and cannot describe a completed
  job. That distinction is why the average was added to `route_status`; without it
  a sub-second-to-few-second job is unmeasurable through the tools.
- The completion poll is 50 ms. At 200 ms the poll lag dominated the wall-clock; at
  20 ms the polls measurably perturbed the run they were measuring.
- The **latency** row is `route_status` with nothing running — a map lookup over an
  empty map, so what is measured is the interface, not work.
- The **token** row counts only the three calls an agent makes. The harness itself
  polls ~26 times (~15 KB); publishing *that* as the agent's cost would be an
  artifact of the measurement, not a property of the server.
- `mcp_bench.py` calls `server_info` first and **aborts on a debug build**. An
  earlier MCP measurement session was silently invalidated by a stale debug binary;
  the check exists so that cannot recur.
- No `metrics` middleware anywhere on the path. The per-route counters
  `route_status` reports are the same ones the web UI uses and are not the metrics
  crate.

</details>

### 8 — Postgres → file vs. the tools that ship with Postgres

The Meltano and Sling comparisons line mq-bridge-app up against other ETL tools.
This one lines it up against what a Postgres user **already has installed**, which
is the more common reference point. The peer is **`psql \copy … TO … (FORMAT csv,
HEADER true)`**: both sides read the same `bench` table and write the same CSV, so
the ratio is meaningful. **`pg_dump`** is in the table too, but as a floor rather
than a peer — it writes a restore format and never decodes a row, so it marks the
cost of getting bytes out of Postgres at all.

```bash
benches/etl/seed.sh up && benches/etl/seed.sh bench 1000000
# A HOST psql/pg_dump is required here (see below). macOS:
brew install libpq && export PATH="$(brew --prefix libpq)/bin:$PATH"
BATCH=32768 CONC=4 benches/etl/run_pg_vendor.sh   # -> results/pg_vendor.csv
```

> **Measured 2026-08-05 on an idle machine.** The `psql \copy` and `pg_dump`
> baselines are from the 0.2.13 session and are unchanged — neither takes a batch
> size, so there was nothing to re-measure. Every mq-bridge-app cell in §8a, §8b
> and §8c was re-measured together at `--batch-size 32768` on a later build, with
> 1 warmup and 3 timed runs per cell and a landed-row check on each.

#### 8a — vs. `psql \copy`

1M rows, batch 32768 / concurrency 4, median of 3:

| Tool | rows/s | median | on disk | writes |
| --- | ---: | ---: | ---: | --- |
| `pg_dump` (floor, not a peer) | 660,066 | 1.515 s | 111 MB | COPY-block SQL script |
| `psql \copy` → csv | 668,449 | 1.496 s | 129 MB | csv |
| mq-bridge-app → csv | 498,504 | 2.006 s | 129 MB | csv |

- **`psql \copy` is ~1.34x faster, and being faster is the expected result.** It is
  a byte pump: the server serializes CSV in C inside the backend and psql copies
  bytes from the socket to a file, never parsing a row, in one query.
  mq-bridge-app issues one keyset query per batch, decodes every value into a
  typed message, and re-serializes it — a full decode/encode round trip per row.
  The two CSVs are identical (parity-checked by value: 1,000,000 rows equal,
  10,091 differing only in float spelling).
- **Batch size is the parameter that matters here.** At `--batch-size 1024` the
  same job runs at 356,252 rows/s (2.807 s) — 1.9x behind `\copy` — because the
  per-batch query cost is paid ~977 times instead of ~31. Nothing else changed.
- **What that cost buys** is that the same command targets any other sink — a
  broker, a second database, object storage, compressed or encrypted — where
  `\copy` writes one local CSV and stops. For a comparison against tools doing the
  *same* typed-pipeline work, see scenarios 5 and 6.

#### 8b — Output formats

The same read, written seven ways (batch 32768 / concurrency 4, median of 3, all
seven measured in one session). The read column is the reverse trip: that file
back in through mq-bridge-app, same parameters, written to a `raw` sink.

| Format | write rows/s | on disk | read rows/s |
| --- | ---: | ---: | ---: |
| `format=csv` | 498,504 | 128,982,268 | 920,810 |
| `format=normal` | 499,251 | 320,985,602 | 1,137,656 |
| `format=json` | 499,251 | 286,982,210 | 687,757 |
| `format=text` | 492,853 | 334,985,602 | 1,077,586 |
| `format=raw` | 489,236 | 208,982,210 | 3,802,281 |
| `format=normal&compression=lz4` | 501,350 | 73,225,866 | 1,067,235 |
| `format=normal&compression=zstd` | 467,954 | 45,417,349 | 953,288 |

> The two compressed **write** cells are from an interleaved re-measurement
> (lz4/zstd/lz4/zstd, 1 warmup + 3 timed each). The first pass put lz4 at 392,927,
> below zstd — implausible for the cheaper codec, and it did not reproduce in
> either later pair. Everything else in the table is from the single session.

- **Writes are source-bound; reads are not.** The five uncompressed write cells land
  within ~2% of each other (489,236-499,251) because the Postgres cursor is the
  limit, not the sink — at this batch size it tops out near 499k rows/s. The read
  column, where the same sinks are fed by a file instead, spans **5.5x**
  (687,757 to 3,802,281) and every cell in it beats the write column. So treat the
  write column as a floor for the sink, not a measurement of it; the format only
  starts to matter once the source stops being the constraint.
- **`normal` is the interchange format** — the whole message envelope as JSON — and
  it is the one to reach for. A UTF-8 payload is written as a plain string; only a
  non-UTF-8 payload is base64-encoded, into a separate `payload_base64` field
  (mutually exclusive with `payload`, mirroring the CloudEvents `data`/`data_base64`
  split). `text` (payload as a string) and `json` (payload as a JSON value) carry the
  same envelope including `message_id`; `raw` writes the payload alone — smallest and
  by far the fastest to read, but no envelope, so no `message_id`. All read back;
  verified at 1M records each.
- **`normal` is both smaller and faster than `text`** — 321 MB against 335 MB, and
  1,137,656 against 1,077,586 rows/s on read — so there is no case for choosing
  `text` over it on either axis. `text` remains the right choice only when the
  consumer requires the payload as an opaque string field.
- **`json` is the slowest format to read**, at 687,757 against `normal`'s 1,137,656.
  It is the only one that materializes the payload into a `serde_json::Value` and
  re-serializes it; the others hand bytes through. Choose it when you want the
  payload as queryable JSON, not for speed.
- **CSV cannot be compressed** — the endpoint rejects the combination — so the
  compressed cells use `normal`, which is the right pairing anyway.
- **zstd for size, lz4 for speed.** zstd is **1.6x smaller** (45.4 MB against
  73.2 MB; 7.1x smaller than uncompressed `normal`, and *2.8x smaller than the CSV*
  while carrying strictly more), and costs ~7% against lz4 on write (467,954 vs
  501,350) and ~11% on read (953,288 vs 1,067,235). zstd is the default
  recommendation: 1.6x on disk is worth ~10% of CPU for almost any at-rest or
  transfer use. Pick lz4 when the pipeline is CPU-bound and the bytes are transient.
- **What compression costs depends on which end is the constraint.** The codec
  always burns CPU; whether that CPU is on the critical path is the whole question.
  On write here it mostly is not — lz4's 501,350 sits inside the ~2% spread of the
  uncompressed cells and zstd's 467,954 is only ~6% under, because the Postgres
  source, not the sink, sets the pace. On read there is no slow source to hide
  behind and the cost surfaces in full: ~6% for lz4 and ~16% for zstd against
  uncompressed `normal`, all of it landing on the file source's single reader
  thread. Measure this on your own source rate rather than reading either number as
  the codec's price.

#### 8c — Round trip

Does the interchange format survive a full trip out and back? The zstd file from
§8b is read in again, written out as uncompressed `normal`, and compared
byte-for-byte against the *same* file decoded by the external `zstd` CLI.

| | |
| --- | --- |
| Records restored | 1,000,000 |
| Elapsed | 1.067 s |
| Result | byte-identical, **including `message_id`** |

- **Compare against that same file, not a re-read.** Comparing the restore against
  a separately written `normal` file would be wrong, and is a mistake worth naming:
  that file is a different read of Postgres, and Postgres rows carry no
  `message_id`, so the sink mints a fresh one per ingestion. Two independent reads
  disagree on `message_id` by construction, which looks exactly like "the sink
  regenerates ids on write" — a bug that is not there.
- **Row counts come from the external `zstd`/`lz4` CLIs**, not from mq-bridge:
  verifying a writer with its own reader would hide a bug in both, and this doubles
  as a check that the concatenated members decode as one stream.
- **Restore does not speed up with `--concurrency`.** The file source does not
  parallelize — one reader thread owns decode, delimiter splitting and message
  construction — which is also why the read column above is flat in
  `--concurrency`.

#### 8d — Methodology

<details>
<summary><b>Methodology notes for all of §8</b> — the host-client guard, <code>pg_dump</code>, and how parity is checked</summary>

- **This scenario refuses to run without a host `psql` and `pg_dump`**, and says
  why — unlike every other scenario. `lib.sh` otherwise falls back to the client
  inside the compose container, which would invalidate the comparison twice over:
  `\copy` writes client-side, so the CSV would land on container overlayfs instead
  of host disk, and the client would reach the server over a container-local socket
  rather than the TCP connection mq-bridge-app uses. Different disk, different
  network path — a faster number that is not a comparison. The guard exists because
  that failure is silent.
- **`pg_dump` is published as a floor, not as a peer.** A dump is a *restore
  format* — a COPY-block SQL script, not interchange data — and it skips the
  `ORDER BY` this comparison needs, skips CSV quoting, and writes less (111 MB
  against 129 MB). So it is not a head-to-head that anyone won or lost: it is the
  cost of getting bytes out of Postgres at all, and therefore the bound no
  extraction tool can beat. Read the row that way. `mongodump` is the same
  category; `mongoexport --type=json` would be the fair Mongo peer and is not
  wired up yet.
- **Parity is checked by value, not by bytes.** The two outputs are not
  byte-identical and should not be: a `double precision` holding a whole number
  renders as `7535.0` from the sink (it serializes an f64) and as `7535` from
  Postgres — 10,091 of 1,000,000 rows. The runner compares numeric fields as
  numbers and everything else exactly, and reports `1000000 rows equal`. Casting
  the baseline with `to_char`, or reseeding to avoid whole-valued floats, would
  make a byte comparison pass by shaping the data to fit the claim.

</details>

## Reference baselines to line up against

- **Debezium** — Postgres CDC latency/throughput → scenario 2.
- **OpenMessaging Benchmark** — payload sizes + latency-percentile reporting → 1/3.
- **Airbyte** — records/sec for a full-table sync → scenario 1.

## Teardown

```bash
benches/etl/seed.sh down
```
