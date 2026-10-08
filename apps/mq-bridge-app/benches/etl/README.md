# ETL / CDC benchmark harness (run through mq-bridge-app)

This harness measures how fast the mq-bridge engine moves data **when it is driven
the way a no-code user drives it**: through `mq-bridge-app`, configured by CLI
arguments or a YAML file. No scenario uses a Criterion micro-harness or hand-written
Rust. Competing ETL/CDC tools publish "config in → data moved out" numbers; these are
the same kind of number.

**This file is the single source of truth for every measured ETL/CDC number.** Change
a number here and nowhere else. The book's
[tuning page](../../../../docs/book/operations/tuning.md) and
[MCP page](../../../../docs/book/MCP.md) include their tables from this file by
anchor, so they follow automatically.

Contents:

1. [How to read the numbers](#how-to-read-the-numbers)
2. [Results at a glance](#results-at-a-glance)
3. [Scenario index](#scenario-index)
4. [Setup and teardown](#setup-and-teardown)
5. [Scenarios](#scenarios) — one section each, all with the same layout
6. [Typed vs. untyped: how to read the Sling ratios](#typed-vs-untyped-how-to-read-the-sling-ratios)
7. [Published benchmarks these line up against](#published-benchmarks-these-line-up-against)

## How to read the numbers

These rules hold for every table on this page unless a cell says otherwise.

| Item | Value |
| --- | --- |
| mq-bridge version | **0.4.20**, measured 2026-10-06 |
| Build | default release build (`full` features, **mimalloc** allocator) — what Homebrew and cargo-binstall ship |
| Machine | Apple M1, 8 cores, 8 GB RAM, in ordinary desktop use |
| Dataset | 1,000,000 rows, seed 42, 7 mixed-type columns (scenarios 5–11); padded 256 B / 4 KiB JSON rows (scenarios 1–3) |
| Throughput | rows ÷ median wall-clock of the whole process, start to exit |
| Runs | one discarded warm-up, then the timed runs; run counts are next to each number |
| Peak RSS | `/usr/bin/time -l`, measured alongside throughput |
| Correctness | every timed run must land the full row count or the run fails |
| Baseline versions | Sling 1.5.21 · Meltano 4.4.0 (`tap-csv` 1.3.2, `tap-postgres` 0.9.0, `target-jsonl` 0.1.4) · DuckDB 1.5.6 · Arroyo 0.15.0 · Sea Streamer 0.5.2 · Redpanda Connect 4.112.0 (mq-bridge-connect plugin 0.1.1) · Vector 0.59.0 |

- **Numbers are hardware-dependent.** Treat them as shape, not guarantees. Differences
  of a few percent are noise. Ratios against a baseline on the same machine are the
  portable figures.
- **Never quote a number without its mq-bridge version.**
- **`--concurrency 1` does not mean one thread.** It is one route *worker*. CSV
  decoding is spread across a shared pool, so the untyped CSV run uses ~2.9 cores
  (0.66 s user + 0.15 s sys against 0.28 s wall) and the typed run ~4.5. Any comparison
  against a baseline pinned to one thread has to say so.
- **Every mq-bridge-app number is a mimalloc number.** mimalloc is a cargo feature,
  on by default and also pulled in by the `bench` feature. To measure the system
  allocator instead, build with
  `--no-default-features --features mq_bridge_app/bench`. Baseline tools are
  unaffected.

## Results at a glance

### mq-bridge-app on its own

<!-- ANCHOR: reference_numbers -->
| Scenario | Batch | Conc. | Throughput | Peak RSS | Version |
|---|---|---|---|---|---|
| CSV → JSONL (strings passthrough, 1M rows ~116 MiB) | 1024 | 1 | **3,134,796 rows/s** | ~29 MiB | 0.4.20 |
| CSV → JSONL, same job driven through an MCP tool call | 1024 | 1 | **2,466,515 rows/s** | — | 0.4.20 |
| CSV → JSONL with typing `transform` (id→int, embedded JSON) | 1024 | 1 | **1,626,016 rows/s** | ~71 MiB | 0.4.20 |
| IPC forward (`static` → `memory`, Unix socket) | 1024 | 1 | **1,419,025 rows/s** | — | 0.4.20 (300 s window) |
| Postgres → JSONL (1M rows, 7 mixed-type cols) | 1024 | 4 | **505,050 rows/s** | ~57 MiB | 0.4.20 |
| Postgres → JSONL (same) | 1024 | 1 | **413,907 rows/s** | ~55 MiB | 0.4.20 |
<!-- ANCHOR_END: reference_numbers -->

The IPC row is a sustained rate sampled from the receiver's transport log, not a
wall-clocked job. It has no RSS column and is not comparable cell-for-cell with the
other rows.

Three things the table shows:

- **Typing has a real but modest cost.** A `transform` that coerces `id` to an integer
  and decodes an embedded JSON document costs ~0.30 µs/row. CSV → JSONL drops from
  3.13M to 1.63M rows/s (about 1.93x), but every output record is fully typed.
- **Postgres → JSONL is bound by the Postgres read**, not by the file sink: four route
  workers add ~22% over one, far from 4x.
- **Peak RSS does not scale with dataset size**, because rows stream in batches rather
  than being buffered whole: ~29 MiB for a passthrough copy however large the input.
  It is not a constant. Batch size, connector-side buffering, allocator retention and
  transforms all move it; the typing `transform` raises it to ~71 MiB through per-row
  JSON decode and buffering.

### Against other tools

| Job | mq-bridge-app | Other tool | Result | Read it with this attached | § |
| --- | ---: | ---: | --- | --- | --- |
| CSV → JSONL, typed | 1,626,016 rows/s | Sling: 111,358 | **~14.6x** faster | equal work; outputs asserted identical | [6](#6--csv--jsonl-vs-sling-and-meltano) |
| CSV → JSONL, untyped | 3,134,796 rows/s | Meltano: 9,771 | **~321x** faster | equal work; both emit strings | [6](#6--csv--jsonl-vs-sling-and-meltano) |
| CSV → JSONL, untyped | 3,134,796 rows/s | DuckDB: 2,109,704 | **~1.49x** faster, ~18x less memory | DuckDB is a ceiling, not an ETL tool | [6](#duckdb-as-a-ceiling) |
| Postgres → JSONL | 413,907 rows/s | Sling: 102,743 | **~4.0x** faster | *not* equal work: Sling types, mq-bridge-app does not | [5](#5--postgres--jsonl-vs-sling-and-meltano) |
| Postgres → JSONL | 413,907 rows/s | Meltano: 8,759 | **~47x** faster, ~11x less memory | default Singer config | [5](#5--postgres--jsonl-vs-sling-and-meltano) |
| Postgres → CSV | 495,540 rows/s | `psql \copy`: 587,199 | psql **~1.18x** faster | psql is a byte pump; expected | [8](#8--postgres--file-vs-the-tools-that-ship-with-postgres) |
| Kafka → JSONL, projection | 599,031 rows/s | Arroyo: 566,991 | within noise, half the memory | both in containers; mq-bridge-app image 0.4.19; delivery guarantees differ | [9a](#9a--kafka--jsonl-vs-arroyo) |
| Kafka → file, passthrough | 878,105 rows/s | Sea Streamer: 483,800 | **~1.82x** faster (1.78x vs. its mimalloc build) | sink formats differ | [9b](#9b--kafka--file-vs-sea-streamer) |
| CSV → JSONL, typed | 1,594,896 rows/s | Redpanda Connect: 81,893 | **~19.5x** faster | equal work; outputs asserted identical; same-session native figure | [10a](#10a--csv--jsonl) |
| Kafka → JSONL, projection | 766,280 rows/s | Redpanda Connect: 65,741 | **~11.7x** faster, under half the memory | both on the host; identical sink bytes | [10b](#10b--kafka--jsonl) |
| CSV → JSONL, typed | 1,564,945 rows/s | Vector: 56,670 | **~27.6x** faster | equal work; outputs asserted identical; Vector is bound by its sink | [11a](#11a--csv--jsonl) |
| Kafka → JSONL, projection | 722,337 rows/s | Vector: 57,431 | **~12.6x** faster, about half the memory | both on the host; identical sink bytes; Vector is bound by its sink | [11b](#11b--kafka--jsonl) |
| Kafka → Kafka, passthrough | 281,489 rows/s | Vector: 154,414 | **~1.8x** faster, about half the memory | both on the host; a sink Vector is built for; record counts asserted | [11c](#11c--kafka--kafka) |

## Scenario index

Every scenario section below has the same four parts: **Measures**, **Run**,
**Result**, **Notes**.

| § | Job | Reports | Runner | Headline |
| --- | --- | --- | --- | --- |
| [1 & 3](#1--3--postgres-table--table-batched-vs-unbatched) | Postgres table → Postgres table | rows/s over a payload × batch × concurrency matrix | `run_throughput.sh` | 110,448 rows/s (256 B, batch 128, conc. 4); 251,955 with `bulk_copy` at batch 32,768 |
| [2](#2--cdc-commit-to-sink-latency) | Postgres CDC → file | latency from `COMMIT` to sink line | `run_cdc_latency.sh` | p50 ~0.2 ms, p99 < 0.8 ms |
| [4](#4--local-ipc-throughput) | process → process over a Unix socket | sustained rows/s | `run_ipc_throughput.sh` | 1,419,025 rows/s |
| [5](#5--postgres--jsonl-vs-sling-and-meltano) | Postgres → JSONL | rows/s, peak RSS vs. Sling, Meltano | `run_meltano_bench.sh` | 413,907 rows/s, 54.9 MiB |
| [6](#6--csv--jsonl-vs-sling-and-meltano) | CSV → JSONL | rows/s, peak RSS vs. Sling, Meltano, DuckDB | `run_csv_to_jsonl.sh` | 3,134,796 rows/s, 28.7 MiB |
| [7](#7--mcp-server-tool-call-latency-throughput-token-cost) | CSV → JSONL through an MCP tool call | latency, rows/s, agent tokens | `run_mcp_bench.sh` | 2,466,515 rows/s, ~86 ms fixed cost |
| [8](#8--postgres--file-vs-the-tools-that-ship-with-postgres) | Postgres → file | rows/s vs. `psql \copy`, `pg_dump`; seven output formats | `run_pg_vendor.sh` | 495,540 rows/s to CSV |
| [9](#9--kafka--file-vs-arroyo-and-sea-streamer) | Kafka → file | rows/s, peak RSS vs. Arroyo, Sea Streamer | `run_kafka_stream.sh` | 878,105 rows/s, 160 MiB |
| [10](#10--redpanda-connect-and-the-connect-plugin) | CSV → JSONL and Kafka → JSONL | rows/s, peak RSS vs. Redpanda Connect; native endpoints vs. the Connect plugin | `run_csv_connect.sh`, `run_kafka_stream.sh connect` | ~19.5x (CSV, typed), ~11.7x (Kafka, projection) |
| [11](#11--vector) | CSV → JSONL, Kafka → JSONL, Kafka → Kafka | rows/s, peak RSS vs. Vector | `run_csv_vector.sh`, `run_kafka_stream.sh vector`, `run_kafka_stream.sh kafka-sink` | ~27.6x (CSV, typed), ~12.6x (Kafka → JSONL), ~1.8x (Kafka → Kafka) |

Scenarios 5 and 6 are the two headline ETL jobs. Scenario 3 (the batching lever) is
the `batch=1` vs. `batch=128` rows of scenario 1's matrix, which is why the two share
a section.

## Setup and teardown

Commands are run from `apps/mq-bridge-app`. Results land in `benches/etl/results/`.

```bash
# 1. Release binary (the default `full` build; every published number uses it).
cargo build -p mq-bridge-app --release

# 2. Postgres 16 with logical replication (mirrors the library's CDC compose).
benches/etl/seed.sh up

# 3. Optional: the Sling CLI baseline for scenarios 5 and 6 (a compiled Go EL tool).
#    Kept repo-local so a run never depends on PATH; the runners skip Sling if absent.
mkdir -p benches/etl/bin && curl -sL \
  "https://github.com/slingdata-io/sling-cli/releases/latest/download/sling_darwin_arm64.tar.gz" \
  | tar -xz -C benches/etl/bin sling

# 4. Optional: Redpanda Connect and the mq-bridge-connect plugin for scenario 10.
#    The runners skip whichever is absent.
curl -sL "https://github.com/redpanda-data/connect/releases/download/v4.112.0/redpanda-connect_4.112.0_darwin_arm64.tar.gz" \
  | tar -xz -C benches/etl/bin redpanda-connect
brew install marcomq/tap/mq-bridge-connect

# 5. Optional: Vector for scenario 11. Skipped if absent.
curl -sL "https://github.com/vectordotdev/vector/releases/download/v0.59.0/vector-0.59.0-arm64-apple-darwin.tar.gz" \
  | tar -xz -C benches/etl/bin --strip-components 3 ./vector-arm64-apple-darwin/bin/vector
```

Requirements: Docker, `curl`, `python3` (sub-second timing) and `uv` (runs
`gen_bench_data.py` and `cdc_latency.py`) on PATH. A host `psql` is used if present;
otherwise the harness runs `psql` inside the compose container. Scenario 8 is the
exception and needs host `psql` and `pg_dump`.

Teardown:

```bash
benches/etl/seed.sh down
```

<details>
<summary><b>Lean build</b> — faster to compile, enough for the Postgres scenarios</summary>

A lean build avoids the heavy `full` dependencies (rdkafka/librdkafka, grpc/protoc,
ibm-mq). Do not mix numbers from the two builds in one table. Give the lean build its
own `CARGO_TARGET_DIR` so it cannot overwrite the default binary, and pass the same
value when running the benchmarks (the runners resolve `BIN` from it):

```bash
CARGO_TARGET_DIR=target-lean cargo build -p mq-bridge-app \
  --no-default-features --features bench --release
CARGO_TARGET_DIR=target-lean benches/etl/run_throughput.sh
```

For `run_cdc_latency.sh` use `--features bench-cdc` instead: CDC needs the Postgres
logical-replication endpoint, which pulls aws-lc-sys (a slow C build) and is therefore
not in plain `bench`.

</details>

### Fixed parameters

| Parameter | Value |
| --- | --- |
| Payload | 256 B and 4 KiB JSON rows (scenarios 1–3); the 7-column `bench` row (scenarios 5–11) |
| Message count | 1,000,000 per run unless a table says otherwise |
| Batch sizes | 1 / 128 (scenarios 1 & 3); 1024 (scenarios 4–7 and 9–11); 32768 (scenario 8) |
| Concurrency | 1 and 4 route workers |
| Postgres | `postgres:16-alpine`, `wal_level=logical` |
| Warm-up | one discarded run per cell, plus a 5,000-row pre-roll when seeding scenarios 1 & 3 |
| Environment | CPU model, cores, RAM, mq-bridge(-app) version are printed next to every result |

Payload rows for scenarios 1–3 are `{"id":<n>,"pad":"xxx…"}` padded to exactly 256 /
4096 bytes.

## Scenarios

### 1 & 3 — Postgres table → table, batched vs. unbatched

**Measures.** `copy` reads a seeded Postgres table and inserts into a fresh table,
wall-clocked. `rows/s = rows / elapsed`. Scenario 3 is the batching lever: compare
the `batch=1` and `batch=128` rows.

**Run.** The runner sweeps the payload × batch × concurrency matrix and re-seeds
before each timed run:

```bash
benches/etl/run_throughput.sh          # -> benches/etl/results/throughput.csv
```

The single underlying command, as a user would type it:

```bash
mqb copy \
  --from 'postgres://testuser:testpass@localhost:5432/testdb?table=src_256&cursor_column=id&sslmode=disable' \
  --to   'postgres://testuser:testpass@localhost:5432/testdb?table=dst_256&columns=auto&sslmode=disable' \
  --drain --batch-size 128 --concurrency 4
```

**Result.** 1 warm-up, median of the timed runs:

| Payload | Batch | Conc. | Rows | Runs | Median | rows/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 256 B | 1 | 1 | 100,000 | 2 | 65.553 s | 1,525 |
| 256 B | 1 | 4 | 100,000 | 2 | 34.221 s | 2,922 |
| 256 B | 128 | 1 | 1,000,000 | 3 | 19.055 s | 52,479 |
| 256 B | 128 | 4 | 1,000,000 | 3 | 9.054 s | 110,448 |
| 4 KiB | 1 | 1 | 100,000 | 2 | 79.854 s | 1,252 |
| 4 KiB | 1 | 4 | 100,000 | 2 | 73.679 s ±27.008 | 1,357 |
| 4 KiB | 128 | 1 | 100,000 | 3 | 8.446 s | 11,839 |
| 4 KiB | 128 | 4 | 100,000 | 3 | 5.355 s | 18,674 |

**Notes.**

- **Batching is the lever.** At 256 B, `batch=128` is ~34x `batch=1` at concurrency 1
  and ~38x at concurrency 4.
- **The 4 KiB, batch 1, concurrency 4 cell is not reliable**: its two runs were ~47 s
  and ~101 s. Every other cell stayed within ±1.3 s.
- **`columns=auto` writes the row's fields into the table's columns** (`id`, `payload`).
  The other write modes are measured below.
- **Row counts differ per cell.** `batch=1` cells and the 4 KiB cells use 100,000 rows
  (`MSG_COUNT=100000`); the 256 B `batch=128` cells use 1,000,000. The 4 KiB cells are
  bound by Postgres writing ~4 GB per million rows on this 8 GB machine.
- **`cursor_column=id` reads the source non-destructively**, paging on the monotonic
  `id` column (the sqlx cursor reader, an incremental-sync read like Airbyte's). Each
  moved record is the source row as JSON (`{"id":N,"payload":"…"}`).
- The library's Criterion harness additionally covers the `memory` backend.

#### Larger batches and the write path

**Measures.** The same 256 B table → table copy at batch sizes above the matrix, once
per way the Postgres sink can write. Only the `columns=auto` column is what
`run_throughput.sh` runs; the other cells are the command above with a different `--to`:

| Variant | `--to` parameters | What lands in `dst_256.payload` |
| --- | --- | --- |
| envelope | `auto_create_table=true`, no `columns` or `insert_query` | the whole source row, hex-encoded (see notes) |
| envelope, `bytea` | same, table created by the sink instead of `seed.sh` | the whole source row as bytes |
| token `INSERT` | `insert_query=INSERT INTO dst_256 (payload) VALUES (${payload:payload})` | the 256 B payload |
| token `COPY` | the same `insert_query` plus `bulk_copy=true` | the 256 B payload |
| `columns=auto` | `columns=auto` | `id` and the 256 B payload |

**Result.** 1,000,000 rows, 1 warm-up + 3 timed runs, median rows/s (0.4.20):

| Batch | Conc. | envelope | envelope, `bytea` | token `INSERT` | token `COPY` | `columns=auto` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,024 | 1 | 102,791 | 116,431 | 136,757 | 128,759 | 89,843 |
| 1,024 | 4 | 201,038 | 211,126 | 233,744 | 215,668 | 185,923 |
| 8,192 | 1 | 116,068 | 136,092 | 147,278 | 151,642 | 101,845 |
| 8,192 | 4 | 161,539 | 197,463 | 240,182 | **251,128** | 197,796 |
| 32,768 | 1 | 100,392 | 122,268 | 136,232 | 147,095 | 93,107 |
| 32,768 | 4 | 140,549 | 180,091 | 193,037 | **251,955** | 182,608 |

Postgres on its own, same table and machine, three runs each:

| Path | rows/s |
| --- | ---: |
| `psql -c 'COPY (SELECT payload FROM src_256) TO STDOUT' \| psql -c 'COPY dst_256 (payload) FROM STDIN'`, inside the container | 218,752–253,371 |
| `INSERT INTO dst_256 (payload) SELECT payload FROM src_256` | 308,053–431,848 |

**Notes.**

- **The server is the limit, not the protocol.** The best cell (~252k rows/s) equals the
  `psql` COPY pipe. `INSERT … SELECT` never leaves the server, so it marks what no
  client can beat; the 413,907 rows/s of scenario 5 is a read into a local file and
  not a target for a table sink.
- **Batch size.** 128 → 1,024 is worth ~1.5–1.8x; beyond that multi-row `INSERT` gains
  little and loses at 32,768 with concurrency 4. `COPY` is ahead from 8,192 up (3–30%)
  and slightly behind below. Which mode and batch size to pick is in the book's
  [Performance tuning](../../../../docs/book/operations/tuning.md#writing-to-a-postgres-table) page.
- **The envelope cell writes hex.** Without `columns` or an `insert_query` the sink binds the message
  as bytes, for the `payload BYTEA` queue table it creates itself. `seed.sh` creates
  `dst_256.payload` as `text`, so Postgres stores `\x7b22…` (~570 characters instead of
  256). The sink's own table also carries a second index, on `locked_until`.
- **One table shape only**: a `text` column and a `bigserial` primary key, source and
  sink on the same Postgres in the Docker VM. Wide tables were not measured.

### 2 — CDC commit-to-sink latency

**Measures.** How long after a committed Postgres transaction its change event is in
the sink file. The route is `postgres_cdc → file`
([`cdc_latency.yaml`](cdc_latency.yaml)) at `batch_size: 1`.

**Run.**

```bash
benches/etl/run_cdc_latency.sh         # -> benches/etl/results/cdc_latency.csv
```

The runner boots the app with `--config cdc_latency.yaml`, then starts the route via
`POST /consumer-start`, the zero-code equivalent of clicking *Start* in the UI.
[`cdc_latency.py`](cdc_latency.py) then commits single-row transactions at a fixed
rate and tails the sink file, timing every event.

**Result.** A 20 s window per cell after a 2 s pre-roll; every committed row must
reach the sink or the cell fails:

| Payload | Commits/s | Events | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 256 B | 200 | 4,000 | 0.220 ms | 0.423 ms | 0.577 ms |
| 256 B | 1,000 | 20,000 | 0.163 ms | 0.246 ms | 0.297 ms |
| 4 KiB | 200 | 4,000 | 0.338 ms | 0.579 ms | 0.770 ms |
| 4 KiB | 1,000 | 20,000 | 0.176 ms | 0.281 ms | 0.367 ms |

**A committed row is in the sink file about 0.2 ms later, and within 0.8 ms at p99**,
for both payload sizes.

**Notes.**

- **The clock starts when the client sees the `COMMIT` acknowledged** and stops when
  the tailing reader sees the complete line in the sink file. Both timestamps are
  taken by the same process on one monotonic clock, so nothing is compared across the
  Docker boundary. In between lie WAL decoding in Postgres, the replication stream,
  the route and the file write.
- **The commit itself is not included.** It costs 0.3–1.7 ms on this Postgres (median
  0.33 ms at 1,000 commits/s, 1.5 ms at 200), which is more than the CDC path adds.
  Counted from the moment the `INSERT` is sent, p99 is 1.3–3.7 ms; the results CSV
  carries both.
- **The higher rate reads lower** because the route and the replication connection
  stay warm between events; at 200 commits/s each event finds them idle.
- **It is an open-loop measurement at a modest rate**, on one machine with Postgres in
  a local container. It is not a saturation test and not comparable with a Debezium
  figure measured through Kafka across a network. For CDC *throughput*, run `copy`
  with a `postgres-cdc://…` source and wall-clock it.
- **Use a permanent replication slot** (`temporary_slot: false`, as in the YAML). A
  temporary slot is dropped when its creating connection closes and races the
  streaming connection (`replication slot "…" does not exist`). The runner drops the
  slot before each run for a clean start.
- **Consumers do not auto-start headless**, and `POST /config` only validates and
  saves; it does not start routes. Hence the explicit `POST /consumer-start`.
- **Do not read latency off the `metrics` middleware on an input endpoint.** Its
  `queue_message_processing_duration_seconds` there is the time `receive_batch` took,
  which is mostly the wait for the next event: it tracks the arrival rate, not the
  engine.

### 4 — Local IPC throughput

**Measures.** Sustained rows/s from one process to another over a real Unix domain
socket. A `static` load generator (process A) forwards to a receiver (process B)
through a `memory:` endpoint with a `unix://` topic
([`ipc_sender.yaml`](ipc_sender.yaml) / [`ipc_receiver.yaml`](ipc_receiver.yaml)), so
this is genuine cross-process IPC, not an in-process channel. Both sides run with
`batch_size: 1024, concurrency: 1`.

**Run.**

```bash
benches/etl/run_ipc_throughput.sh                    # 15 s window -> results/ipc_throughput.csv
RUN_SECONDS=300 benches/etl/run_ipc_throughput.sh    # the published 5-minute window
```

**Result. 1,419,025 rows/s**, sustained over a 5-minute window.

**Notes.**

- **How the rate is taken.** Both routes are started via `POST /consumer-start`. After
  a 2-second settle window, `rows/s` is the delta of the receiver's own per-batch
  transport log (`count=`/`bytes=` from `ipc_unix.rs`) over the sampling window. No
  metrics middleware and no metrics crate are involved.
- **Raising `concurrency` does not help.** Both sides talk over a single Unix socket
  connection, serialized through one mutex-guarded stream, so extra workers only add
  channel-handoff overhead.

### 5 — Postgres → JSONL vs. Sling and Meltano

**Measures.** A one-shot full-table sync of the `bench` table (1,000,000 rows, 7
mixed-type columns: `id, first_name, country, amount, created_at, active, attributes`)
to a local JSONL file. Same Postgres instance, same machine, same job for all three
tools. This is the same dataset scenario 6 reads as CSV.

**Run.** All three tools run through one script (1 warm-up + timed runs each, output
row count verified):

```bash
benches/etl/run_meltano_bench.sh   # -> results/meltano_pg_to_jsonl.csv
```

| Tool | Command |
| --- | --- |
| mq-bridge-app | the `mqb copy` below |
| Sling | `sling run --src-conn … --src-stream public.bench --tgt-object file://…` with `{"format":"jsonlines","file_max_rows":0}`, defaults otherwise |
| Meltano | `meltano run tap-postgres target-jsonl` in `benches/etl/meltano_project/bench` (`tap-postgres` selects only `public-bench.*`) |

```bash
mqb copy \
  --from 'postgres://testuser:testpass@localhost:5432/testdb?table=bench&cursor_column=id&sslmode=disable' \
  --to   'file:///tmp/mqb_bench_out.jsonl?format=raw' \
  --drain --batch-size 1024 --concurrency 1
```

**Result.** 3 timed runs per row:

| Tool | Config | rows/s | Median wall-clock | Peak RSS |
| --- | --- | ---: | ---: | ---: |
| mq-bridge-app `copy` | batch 1024, concurrency 1 | **413,907** | 2.416 s ±0.139 | 54.9 MiB |
| mq-bridge-app `copy` | batch 1024, concurrency 4 | **505,050** | 1.98 s | 57.0 MiB |
| Sling | defaults | 102,743 | 9.733 s ±0.174 | 114.1 MiB |
| Meltano (`tap-postgres` → `target-jsonl`) | default Singer config | 8,759 | 114.157 s ±0.282 | 599.7 MiB |

**mq-bridge-app is ~4.0x faster than Sling and ~47x faster than Meltano**, at about
half of Sling's peak memory and ~11x less than Meltano's.

**Notes.**

- **The Sling ratio does not compare equal work.** mq-bridge-app passes values through
  untyped here, while Sling does schema inference and type conversion. Part of the
  ~4.0x is mq-bridge-app doing less. This scenario has no typed column yet; see
  [Typed vs. untyped](#typed-vs-untyped-how-to-read-the-sling-ratios).
- **Reseed before quoting.** The figure is measured against a freshly seeded `bench`
  table on a freshly created container.
- **Concurrency adds ~22%, not 4x**, because the Postgres read is the limit.
- **No `metrics` middleware is on the path.** The `copy` command never attaches one.

### 6 — CSV → JSONL vs. Sling and Meltano

**Measures.** A one-shot conversion of a 1,000,000-row CSV file to one local JSONL
file, one JSON object per input row. mq-bridge-app is measured **twice**, because the
two configurations do different work and each has a baseline it belongs against:

- **typed** runs a `transform` middleware that reproduces Sling's typing exactly. The
  harness fails the run unless all 1,000,000 output records match Sling's. **Read this
  column against Sling.**
- **untyped** runs no middleware: every field stays the string the CSV reader
  produced, as Meltano's `tap-csv` also emits it. **Read this column against
  Meltano.**

**Run.** The fixture is generated on first run if missing:

```bash
benches/etl/run_csv_to_jsonl.sh   # all tools -> benches/etl/results/csv_to_jsonl.csv
benches/etl/run_csv_duckdb.sh     # DuckDB ceiling, appended to the same results CSV
```

Each tool is also a standalone script, so one number can be re-measured without
re-running the matrix. Each replaces only its own row in the results CSV:

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
# untyped: every field stays the string the CSV reader produced
mqb copy \
  --from 'file:///…/benches/etl/data/bench.csv?format=csv' \
  --to   'file:///tmp/mqb_csv_out.jsonl?format=raw' \
  --drain --batch-size 1024 --concurrency 1

# typed: reproduces Sling's typing
mqb copy \
  --from 'file:///…/benches/etl/data/bench.csv?format=csv' \
  --to   'file:///tmp/mqb_csv_out.jsonl?format=raw|transform?schema_file=…/schemas/bench.json' \
  --drain --batch-size 1024 --concurrency 1
```

| Tool | Command |
| --- | --- |
| Sling | `sling run --src-stream file://…bench.csv --tgt-object file://…` with `{"format":"jsonlines","file_max_rows":0}`, defaults otherwise |
| Meltano | `meltano run tap-csv target-jsonl` in `meltano_project/bench`. Install the plugin once: `(cd meltano_project/bench && ../.venv/bin/meltano install extractor tap-csv)` |

**Result.**

| Tool | Config | rows/s | Median wall-clock | Runs | Peak RSS |
| --- | --- | ---: | ---: | ---: | ---: |
| mq-bridge-app `copy` (typed) | batch 1024, concurrency 1, `transform` (schemas/bench.json) | **1,626,016** | 0.615 s ±0.043 | 5 | 71.4 MiB |
| mq-bridge-app `copy` (untyped) | batch 1024, concurrency 1, no middleware | **3,134,796** | 0.319 s ±0.004 | 5 | 28.7 MiB |
| Sling | defaults | 111,358 | 8.980 s ±0.003 | 2 | 112.4 MiB |
| Meltano (`tap-csv` → `target-jsonl`) | default Singer config | 9,771 | 102.340 s ±3.520 | 3 | 443.8 MiB |

**Typed vs. Sling: ~14.6x**, equal work. **Untyped vs. Meltano: ~321x**, both emit
strings. Those are the two defensible ratios.

**Notes.**

- **Do not cross the columns.** Quoting the untyped figure against Sling would
  overstate the margin (it would read ~28x); quoting the typed figure against Meltano
  understates it. See
  [Typed vs. untyped](#typed-vs-untyped-how-to-read-the-sling-ratios).
- **The typing cost is explicit.** The transform adds 0.30 s per 1,000,000 rows
  (~0.30 µs/row, ~48% of the typed wall-clock).
- **Memory.** The transform's per-row JSON decode and buffering add ~43 MiB. The typed
  path is still 41 MiB under Sling and well under Meltano.
- **The number does not scale with batch size or concurrency.** A `file://` source is
  a single sequential reader, so extra route workers cannot parallelize the read. The
  bottleneck is per-row CSV → JSON conversion, not I/O or batching.

#### Workload definition

This is an existing, independently published 1,000,000-row CSV → JSONL workload
against Meltano's `tap-csv` → `target-jsonl`, not a variant of it. The generator, seed
and row count are unchanged, so the input is **byte-identical** to that fixture. The
SHA-256 is the check: any fixture generated the same way has the same hash.

| Item | Value |
| --- | --- |
| Fixture | `data/bench.csv`, from [`gen_bench_data.py`](gen_bench_data.py) `--rows 1000000 --seed 42` |
| Rows | 1,000,000 data rows + 1 header line |
| Size | 121,981,421 bytes (116.3 MiB) |
| SHA-256 | `a84894e01d4c3fccfe89949ae169cef2e2d755770512bf2fe14e84360c45b221` |
| Columns | `id` (int), `first_name`, `country` (strings), `amount` (float), `created_at` (RFC 3339), `active` (bool), `attributes` (nested JSON) |
| Job | one-shot, full file, CSV → one local JSONL file, one JSON object per input row |
| Transformation | none in the untyped run: every field stays a string, as `tap-csv` also emits it |
| Baseline | Meltano `tap-csv` → `target-jsonl`, default Singer config, same file |
| Timing boundary | wall-clock of the whole process, start to exit: startup, read, parse, serialize, write, flush and close |
| Runs | 1 discarded warm-up, then timed runs; median ± stddev |
| Correctness | every timed run must land exactly 1,000,000 output rows or the run fails |
| Reported | throughput (rows ÷ median wall-clock) and peak RSS |

What is *not* shared with the other published run of this workload is the machine
(here an Apple M1, 8 cores, 8 GB) and the number of timed runs. Absolute rows/s
compare across publications only with that attached; the ratio against Meltano on the
same machine is the portable figure.

#### DuckDB as a ceiling

DuckDB, on the same file in the same session, shows how fast this machine can turn
this CSV into JSON *at all*. It is not an ETL tool and offers none of the delivery
semantics mq-bridge does (no at-least-once, no checkpointing, no arbitrary sinks), so
this is a throughput ceiling, not a product comparison. `all_varchar=true` matches the
untyped mq-bridge column: strings in, strings out. Run with
[`run_csv_duckdb.sh`](run_csv_duckdb.sh); `run_csv_to_jsonl.sh` does not chain it.

| Tool | Threads / cores used | Throughput | Peak RSS |
|---|---|---|---|
| mq-bridge-app `copy`, untyped | `--concurrency 1`, ~2.9 cores | **3,134,796 rows/s** (0.319 s ±0.004) | ~29 MiB |
| DuckDB 1.5.6, all cores (default) | ~4.0 cores | 2,109,704 rows/s (0.474 s ±0.007) | ~522 MiB |

mq-bridge was **~1.49x faster than DuckDB running on all of this machine's cores**,
while using fewer cores and ~18x less memory. Both sides are multi-threaded, so the
core counts belong next to the rates.

### 7 — MCP server: tool-call latency, throughput, token cost

**Measures.** What it costs to drive the engine the way an **LLM agent** does: through
the MCP server, over its real stdio transport. The job is the **same CSV → JSONL work
as scenario 6, on the same dataset**, on purpose. The question is not how fast the MCP
server is in isolation, but whether routing a job through a tool call costs anything
per row compared with the `copy` CLI.

**Run.**

```bash
benches/etl/run_mcp_bench.sh          # -> results/mcp_bench.json (+ a row in results/csv_to_jsonl.csv)
REPEATS=3 LATENCY_CALLS=1000 benches/etl/run_mcp_bench.sh
```

[`mcp_bench.py`](mcp_bench.py) is a genuine MCP client. It spawns
`mqb mcp --transport stdio` and speaks JSON-RPC to it exactly as Claude Code would, so
the numbers include the framing, serialization and process-boundary cost an agent
actually pays. Nothing reaches into the process.

**Result.**

<!-- ANCHOR: mcp_results -->
| Measurement | Result |
| --- | --- |
| Tool-call round-trip latency (200 calls) | **p50 0.054 ms** · p95 0.070 ms · p99 1.343 ms |
| 1M-row CSV → JSONL via `start_route` (client wall-clock, 6 runs) | **2,466,515 rows/s** (median 0.405 s) |
| Same job, server's own `average_messages_per_second` | no longer usable at this speed — see the note below |
| `copy` CLI baseline, same dataset and session (§6 untyped) | 3,134,796 rows/s (5-run median 0.319 s, ±0.004) |
| Fixed cost of going through the MCP interface | **~86 ms** |
| Agent tool traffic to move the whole dataset | **1,541 bytes** (~385 tokens, 3 calls) |
| The same 116.3 MiB through a model's context | ~30.5M tokens |
<!-- ANCHOR_END: mcp_results -->

**Notes.**

- **The MCP interface costs one round-trip, not a per-row tax.** Client wall-clock is
  0.405 s against the CLI's 0.319 s on the same dataset in the same session: a fixed
  **~86 ms**, not a rate difference. Most of it is the client's own polling: it asks
  `route_status` every 50 ms (`POLL_INTERVAL_S` in `mcp_bench.py`), so completion is
  seen 0–50 ms late, ~25 ms on average. The two round trips that bracket the job cost
  ~0.05 ms each at p50. The comparison is in fact tilted *toward* MCP: the harness
  spawns a fresh `mqb copy` process for every CLI run, while the MCP server is spawned
  once and reused, so the CLI figure carries process startup the MCP path never pays.
  Scale the dataset and the ~86 ms stays put.
- **Run the MCP scenario with enough repeats.** `mcp_bench.py` discards no warm-up.
  Every run is timed, including the cold first one (2.08M rows/s here, against
  2.44M–2.51M for the five after it), so a short series reads low. Use 6 or more.
- **Ignore the server's own `average_messages_per_second` here.** The route sampler
  ticks every 200 ms and the whole job finishes in about two ticks, so that field
  quantises: it reported anything from 2.6M to 4.2M rows/s across runs of nearly
  identical wall-clock. Client wall-clock is the only meaningful figure at this speed.
- **Agent token cost is flat in the number of rows moved.** This is the one row with
  no CLI counterpart. An agent moves the dataset with three tool calls (`start_route`,
  one `route_status`, `stop_route`) totalling 1,541 bytes of JSON-RPC. The rows never
  enter the model's context, so the same ~1.5 KB moves 1M rows or 1,000; only the
  digits of the counters differ. Passing the 116.3 MiB through a context window
  instead would cost ~30.5M tokens (~4 bytes/token, an estimate), which no context
  window holds at any price.

<details>
<summary><b>Methodology notes</b> — so nothing here is quotable out of context</summary>

- The throughput number is **client-side wall-clock** from the `start_route` call to
  observing `finished: true`, including route startup and completion-detection lag. It
  is the pessimistic figure of the two, which is why it is the one quoted.
- The server-side figure is `average_messages_per_second` (total messages over the
  span in which they moved), **not** the instantaneous `messages_per_second`, which
  decays to ~0 within a second of a drain finishing and cannot describe a completed
  job. That distinction is why the average was added to `route_status`; without it a
  sub-second-to-few-second job is unmeasurable through the tools.
- The completion poll is 50 ms. At 200 ms the poll lag dominated the wall-clock; at
  20 ms the polls measurably perturbed the run they were measuring.
- The **latency** row is `route_status` with nothing running: a map lookup over an
  empty map, so what is measured is the interface, not work.
- The **token** row counts only the three calls an agent makes. The harness itself
  polls ~8 times (~5 KB); publishing *that* as the agent's cost would be an artifact
  of the measurement, not a property of the server.
- `mcp_bench.py` calls `server_info` first and **aborts on a debug build**, so a stale
  debug binary cannot silently invalidate a measurement.
- No `metrics` middleware anywhere on the path. The per-route counters `route_status`
  reports are the same ones the web UI uses and are not the metrics crate.

</details>

### 8 — Postgres → file vs. the tools that ship with Postgres

**Measures.** The same `bench` table written to a local file, against what a Postgres
user **already has installed**. The peer is
**`psql \copy … TO … (FORMAT csv, HEADER true)`**: both sides read the same table and
write the same CSV, so the ratio is meaningful. **`pg_dump`** is in the table as a
floor rather than a peer: it writes a restore format and never decodes a row, so it
marks the cost of getting bytes out of Postgres at all.

**Run.** A host `psql` and `pg_dump` are required (see
[8d](#8d--methodology)):

```bash
benches/etl/seed.sh up && benches/etl/seed.sh bench 1000000
# macOS:
brew install libpq && export PATH="$(brew --prefix libpq)/bin:$PATH"
BATCH=32768 CONC=4 benches/etl/run_pg_vendor.sh   # -> results/pg_vendor.csv
```

All of scenario 8 runs at `--batch-size 32768` / `--concurrency 4`, with 1 warm-up and
3 timed runs per cell and a landed-row check on each. `psql \copy` and `pg_dump` are
the libpq 18 clients. Everything, including the read column in 8b, was measured in one
session.

#### 8a — vs. `psql \copy`

| Tool | rows/s | Median | On disk | Writes |
| --- | ---: | ---: | ---: | --- |
| `pg_dump` (floor, not a peer) | 747,943 | 1.337 s | 111 MB | COPY-block SQL script |
| `psql \copy` → csv | 587,199 | 1.703 s | 129 MB | csv |
| mq-bridge-app → csv | 495,540 | 2.018 s | 129 MB | csv |

- **`psql \copy` is ~1.18x faster, and being faster is the expected result.** It is a
  byte pump: the server serializes CSV in C inside the backend and psql copies bytes
  from the socket to a file, never parsing a row, in one query. mq-bridge-app issues
  one keyset query per batch, decodes every value into a typed message, and
  re-serializes it: a full decode/encode round trip per row. The two CSVs are
  identical (parity-checked by value: 1,000,000 rows equal, 10,091 differing only in
  float spelling).
- **The CSV sink is told to keep nested JSON in one cell** (`csv={"nested":"json"}`,
  URL-encoded in the target URI). Its default since 0.4.19 flattens a nested object
  into `parent.child` columns, which is a different file from the one `\copy` writes.
- **Batch size is a minor lever here.** At `--batch-size 1024` the same job measured
  483,505 rows/s (2.068 s ±0.040), with the per-batch query cost paid ~977 times
  instead of ~31.
- **What that cost buys** is that the same command targets any other sink (a broker, a
  second database, object storage, compressed or encrypted), where `\copy` writes one
  local CSV and stops. For a comparison against tools doing the *same* typed-pipeline
  work, see scenarios 5 and 6.

#### 8b — Output formats

The same read, written seven ways. The read column is the reverse trip: that file back
in through mq-bridge-app, same parameters, written to a `raw` sink.

| Format | Write rows/s | On disk (bytes) | Read rows/s |
| --- | ---: | ---: | ---: |
| `format=csv` | 495,540 | 128,982,268 | 2,746,007 |
| `format=normal` | 516,262 | 292,982,210 | 2,706,088 |
| `format=json` | 530,503 | 272,980,514 | 3,288,554 |
| `format=text` | 505,305 | 306,982,210 | 2,665,278 |
| `format=raw` | 530,785 | 194,980,514 | 4,839,635 |
| `format=normal&compression=lz4` | 489,476 | 71,195,347 | 1,204,913 |
| `format=normal&compression=zstd` | 511,770 | 44,190,932 | 1,124,402 |

- **Writes are source-bound; reads are not.** All seven write cells land within ~8% of
  each other (489,476–530,785) because the Postgres cursor is the limit, not the sink.
  The read column, where the same sinks are fed by a file instead, spans **4.3x** and
  every cell in it beats the write column. Treat the write column as a floor for the
  sink, not a measurement of it; the format only starts to matter once the source
  stops being the constraint.
- **`normal` is the interchange format**, the whole message envelope as JSON, and it
  is the one to reach for. A UTF-8 payload is written as a plain string; only a
  non-UTF-8 payload is base64-encoded, into a separate `payload_base64` field
  (mutually exclusive with `payload`, mirroring the CloudEvents `data`/`data_base64`
  split). `text` (payload as a string) and `json` (payload as a JSON value) carry the
  same envelope including `message_id`. `raw` writes the payload alone: smallest and
  by far the fastest to read, but no envelope, so no `message_id`.
- **`normal` is smaller than `text`** (293 MB against 307 MB), so `text` is the right
  choice only when the consumer requires the payload as an opaque string field.
- **CSV cannot be compressed.** The endpoint rejects the combination, so the
  compressed cells use `normal`, which is the right pairing anyway.
- **zstd for size, lz4 for speed.** zstd is **1.6x smaller** (44.2 MB against 71.2 MB;
  6.6x smaller than uncompressed `normal`, and *2.9x smaller than the CSV* while
  carrying strictly more). On this source-bound write the two are within noise of each
  other (511,770 vs. 489,476); on read lz4 is ~7% faster. zstd is the default
  recommendation: 1.6x on disk is worth a few percent of CPU for almost any at-rest or
  transfer use. Pick lz4 when the pipeline is CPU-bound and the bytes are transient.

#### 8c — Round trip

Does the interchange format survive a full trip out and back? The zstd file from 8b is
read in again, written out as uncompressed `normal`, and compared byte-for-byte
against the *same* file decoded by the external `zstd` CLI.

| | |
| --- | --- |
| Records restored | 1,000,000 |
| Elapsed | 0.941 s |
| Result | byte-identical, **including `message_id`** |

- **Compare against that same file, not a re-read.** Comparing the restore against a
  separately written `normal` file would be wrong, and is a mistake worth naming: that
  file is a different read of Postgres, and Postgres rows carry no `message_id`, so
  the sink mints a fresh one per ingestion. Two independent reads disagree on
  `message_id` by construction, which looks exactly like "the sink regenerates ids on
  write" — a bug that is not there.
- **Row counts come from the external `zstd`/`lz4` CLIs**, not from mq-bridge.
  Verifying a writer with its own reader would hide a bug in both, and this doubles as
  a check that the concatenated members decode as one stream.
- **Restore does not speed up with `--concurrency`.** The file source does not
  parallelize: one reader thread owns decode, delimiter splitting and message
  construction. That is also why the read column in 8b is flat in `--concurrency`.

#### 8d — Methodology

<details>
<summary><b>Methodology notes for all of scenario 8</b> — the host-client guard, <code>pg_dump</code>, and how parity is checked</summary>

- **This scenario refuses to run without a host `psql` and `pg_dump`**, and says why,
  unlike every other scenario. `lib.sh` otherwise falls back to the client inside the
  compose container, which would invalidate the comparison twice over: `\copy` writes
  client-side, so the CSV would land on container overlayfs instead of host disk, and
  the client would reach the server over a container-local socket rather than the TCP
  connection mq-bridge-app uses. Different disk, different network path: a faster
  number that is not a comparison. The guard exists because that failure is silent.
- **`pg_dump` is published as a floor, not as a peer.** A dump is a *restore format*
  (a COPY-block SQL script, not interchange data), and it skips the `ORDER BY` this
  comparison needs, skips CSV quoting, and writes less (111 MB against 129 MB). So it
  is not a head-to-head that anyone won or lost: it is the cost of getting bytes out
  of Postgres at all, and therefore the bound no extraction tool can beat. Read the
  row that way. `mongodump` is the same category; `mongoexport --type=json` would be
  the fair Mongo peer and is not wired up yet.
- **Parity is checked by value, not by bytes.** The two outputs are not byte-identical
  and should not be: a `double precision` holding a whole number renders as `7535.0`
  from the sink (it serializes an f64) and as `7535` from Postgres — 10,091 of
  1,000,000 rows. The runner compares numeric fields as numbers and everything else
  exactly, and reports `1000000 rows equal`. Casting the baseline with `to_char`, or
  reseeding to avoid whole-valued floats, would make a byte comparison pass by shaping
  the data to fit the claim.

</details>

### 9 — Kafka → file vs. Arroyo and Sea Streamer

**Measures.** How fast a four-partition, 1,000,000-row Kafka topic lands in a local
file. The topic holds the usual seven-column JSON row and is filled from the same
`bench` table. These are two deliberately narrow streaming comparisons, not headline
ETL scenarios: 9a against a stream processor, 9b against a streaming library.

**Run.**

```bash
docker compose -f benches/etl/docker-compose.kafka.yml up -d --wait   # Kafka + Arroyo
benches/etl/seed.sh up && benches/etl/seed.sh bench 1000000
benches/etl/run_kafka_stream.sh seed                                  # fill the topic
export MQB_CONSUMER_OPTIONS='[["fetch.queue.backoff.ms","10"]]'
benches/etl/run_kafka_stream.sh mqb      # passthrough + projection
benches/etl/run_kafka_stream.sh arroyo
benches/etl/run_kafka_stream.sh sea      # mq-bridge-app format=normal + Sea Streamer
```

All tools share one stopwatch ([`stream_bench.py`](stream_bench.py)): start the job
and poll the sink until all 1,000,000 rows have landed. Batch 1024, four-way
parallelism, topic seeded with `linger.ms=1`.

> **The mq-bridge-app rows set one consumer option:
> `consumer_options: [["fetch.queue.backoff.ms", "10"]]`**, through
> `MQB_CONSUMER_OPTIONS`. With librdkafka's default (1000 ms) the consumer pauses
> fetching for a full second whenever its local queue fills. The same cells then
> measured 139,119 rows/s (9a passthrough) and 126,633 rows/s (9a projection) in the
> container, and 326,828 rows/s in 9b.

#### 9a — Kafka → JSONL vs. Arroyo

Both sides project the same four columns (`id`, `first_name`, `country`, `amount`) and
write newline-delimited JSON. Arroyo is `ghcr.io/arroyosystems/arroyo:0.15.0`.

| Tool | Median wall-clock | Throughput | Startup | Peak RSS |
| ---- | ----------------: | ---------: | ------: | -------: |
| mq-bridge-app passthrough (no transform) | 1.830 s | 546,337 rows/s | 0.145 s | 233 MiB |
| mq-bridge-app projection (+ `transform`) | 1.669 s | 599,031 rows/s | 0.123 s | 186 MiB |
| Arroyo projection | 1.764 s | 566,991 rows/s | 0.548 s | 362 MiB |

The two projection rows are within noise of each other (~6%); mq-bridge-app uses about
half the memory.

- **Both tools run as containers here**, in the same Docker VM (4 CPUs), reaching the
  broker over the compose network and writing to the same named volume. mq-bridge-app
  is the published `ghcr.io/marcomq/mq-bridge-app:0.4.19` image
  (`stream_bench.py --tool mqb-docker`), so this table is a **0.4.19** measurement.
- **Runs.** mq-bridge-app: 1 warm-up + 3 timed runs. Arroyo: 1 warm-up + 5 timed runs.
  Startup (container start for mq-bridge-app, pipeline scheduling for Arroyo) is
  reported separately. Both projected sinks are 65,615,161 bytes.
- **The delivery guarantees are not equivalent.** Arroyo provides **exactly-once
  processing within its checkpointed pipeline**; mq-bridge-app provides
  **at-least-once delivery** for the Kafka route. It resumes from the source's
  committed consumer offset, so a failure can replay records. The `transform`
  measurement does not add deduplication or upgrade that guarantee. Arroyo's stateful
  features are intentionally not exercised.

#### 9b — Kafka → file vs. Sea Streamer

Both tools relay the original Kafka payload without a transform. mq-bridge-app writes
its default `format=normal` file encoding: a JSON `CanonicalMessage` envelope per
record. Sea Streamer writes its native framed and indexed `.ss` file, using the pinned
official `0.5.2` crates.

| Tool | Median wall-clock | Throughput | Peak RSS | Sink bytes |
| ---- | ----------------: | ---------: | -------: | ---------: |
| mq-bridge-app (`format=normal`, mimalloc) | 1.139 s | 878,105 rows/s | 160 MiB | 363,982,210 |
| Sea Streamer 0.5.2 relay (native `.ss`, system allocator) | 2.067 s | 483,800 rows/s | 726 MiB | 230,989,222 |
| Sea Streamer 0.5.2 relay (native `.ss`, mimalloc) | 2.031 s | 492,465 rows/s | 894 MiB | 230,989,222 |

mq-bridge-app is **1.82x faster** than Sea Streamer's default-allocator result and
**1.78x faster** than its mimalloc result.

- **Both tools run on the host here**, each row 1 warm-up + 3 timed runs.
- **The two file formats are not byte-for-byte equivalent.** The data supports a
  Kafka-to-file throughput comparison, not a claim of identical sink encoding,
  delivery, or checkpoint semantics.
- **Row counts are verified externally.** Each Sea Streamer run is checked with
  `sea-streamer-count` to contain exactly 1,000,000 messages.
- **The backoff option applies to mq-bridge-app only**, because Sea Streamer's relay
  exposes no librdkafka options.
- **The mimalloc row is an application-level allocator measurement**, not a Sea
  Streamer crate feature.

The Sea Streamer helper is committed in [`benches/etl/sea_streamer`](sea_streamer). It
contains the relay and count programs and pins the `0.5.2` dependencies:

```bash
cargo build --manifest-path benches/etl/sea_streamer/Cargo.toml \
  --release --target-dir target
```

For the allocator row, rebuild the same helper in the same target directory with
`--features mimalloc`, then run
`SEA_STREAMER_LABEL=sea-streamer-mimalloc REPEATS=3 ./benches/etl/run_kafka_stream.sh sea`.

### 10 — Redpanda Connect, and the Connect plugin

**Measures.** Two jobs this page already has, run against
[Redpanda Connect](https://github.com/redpanda-data/connect) 4.112.0, and run again
with Redpanda Connect components *inside* mq-bridge-app through the
[Connect plugin](../../../../docs/book/connectors/connect.md) (mq-bridge-connect
0.1.1). Each table therefore answers two questions: how mq-bridge-app compares with
Redpanda Connect, and what a route pays for using a Connect component instead of a
native endpoint or the native `transform`.

All rows of one table were measured in one session, the native rows included, so the
native figures here are a few percent off the headline ones in scenarios 6 and 9.
Compare within a table.

#### 10a — CSV → JSONL

The scenario 6 job on the same fixture. "Typed" is the scenario 6 typing (`id` to an
integer, `attributes` decoded into an object); on the Connect side it is this Bloblang
mapping:

```coffee
root = this
root.id = this.id.int64()
root.attributes = this.attributes.parse_json()
```

**Run.**

```bash
benches/etl/run_csv_mqb.sh && benches/etl/run_csv_mqb.sh --untyped   # native rows
benches/etl/run_csv_connect.sh          # Redpanda Connect + the plugin rows
benches/etl/run_csv_connect.sh parity   # typed outputs == mq-bridge-app's typed output
```

Redpanda Connect runs [`connect/csv_untyped.yaml`](connect/csv_untyped.yaml) and
[`connect/csv_typed.yaml`](connect/csv_typed.yaml): a `file` input with the `csv`
scanner, the mapping, a `file` output, defaults otherwise. The plugin rows are `mqb
copy` with `--plugin`:

```bash
# native endpoints, Bloblang as a middleware
mqb copy --plugin …/libmq_bridge_connect.dylib \
  --from 'file:///…/bench.csv?format=csv' \
  --to   'file:///tmp/out.jsonl?format=raw|connect_mapping?mapping=<url-encoded mapping>' \
  --drain --batch-size 1024 --concurrency 1

# Connect `file` input and output as the endpoints
mqb copy --plugin …/libmq_bridge_connect.dylib \
  --from 'connect://?yaml=<url-encoded input (+ pipeline) document>' \
  --to   'connect://?yaml=<url-encoded output document>' \
  --drain --batch-size 1024 --concurrency 1
```

**Result.** 1 warm-up + 3 timed runs per row. `connect_mapping` is not part of
mq-bridge: it is Redpanda Connect's `mapping` processor, provided by the Connect
plugin. Every row marked **Connect plugin** needs the plugin loaded.

| Tool | Endpoints | Typing | rows/s | Median wall-clock | Peak RSS |
| --- | --- | --- | ---: | ---: | ---: |
| mq-bridge-app | native | none | **2,967,359** | 0.337 s ±0.008 | 30.6 MiB |
| mq-bridge-app | **Connect plugin** | none | 53,050 | 18.850 s ±0.195 | 137.1 MiB |
| Redpanda Connect | its own | none | 97,885 | 10.216 s ±0.113 | 171.4 MiB |
| mq-bridge-app | native | native `transform` (no plugin) | **1,594,896** | 0.627 s ±0.003 | 74.0 MiB |
| mq-bridge-app | native | **Connect plugin** middleware (`connect_mapping`, Bloblang) | 142,734 | 7.006 s ±0.019 | 218.4 MiB |
| mq-bridge-app | **Connect plugin** | **Connect plugin** (mapping in the input's `pipeline`) | 45,077 | 22.184 s ±0.464 | 138.5 MiB |
| Redpanda Connect | its own | mapping | 81,893 | 12.211 s ±0.182 | 171.2 MiB |

**Native mq-bridge-app is ~19.5x faster than Redpanda Connect typed and ~30x
untyped.** All four typed outputs hold the same 1,000,000 records.

**Notes.**

- **A Connect component costs one hop into Go per message, and that hop is the
  price.** The same typing is ~11x slower as `connect_mapping` than as the native
  `transform` (142,734 against 1,594,896), though still ~1.7x faster than Redpanda
  Connect running the same mapping, because the read and the write stay native.
- **With Connect components at both ends the route is slower than Redpanda Connect
  itself** (~1.85x): every message crosses the plugin boundary twice and Redpanda
  Connect's own stream does not. This is why the book says to prefer a native
  connector where one exists and to keep the plugin for systems mq-bridge has no
  endpoint for.
- **Parity is checked after sorting by `id`.** Redpanda Connect runs its pipeline on
  several threads, and the plugin keeps up to 64 batches in flight, so neither
  preserves the input order. Records are compared as parsed JSON, as in scenario 6.
- **Redpanda Connect runs at its defaults**, like Sling and Meltano in scenario 6. Its
  `file` output writes message by message; no batching was added on either side of
  it.
- **Peak RSS is from a separate single run** per row under `/usr/bin/time -l`.

#### 10b — Kafka → JSONL

The scenario 9 job: the same four-partition, 1,000,000-row topic, the same stopwatch
([`stream_bench.py`](stream_bench.py)), the same four-column projection as 9a. The
Connect plugin links no Kafka component (see the book's
[licensing note](../../../../docs/book/connectors/connect.md#what-is-not-included)),
so the plugin row is the native Kafka source with the projection as a
`connect_mapping` middleware.

**Run.** Setup and seeding as in scenario 9, then:

```bash
export MQB_CONSUMER_OPTIONS='[["fetch.queue.backoff.ms","10"]]'
benches/etl/run_kafka_stream.sh mqb       # native passthrough + projection
benches/etl/run_kafka_stream.sh connect   # Redpanda Connect + the Bloblang middleware
```

**Result.** All tools on the host. Native rows: 1 warm-up + 5 timed runs; the others
1 warm-up + 3.

| Tool | Projection | Median wall-clock | Throughput | Peak RSS |
| ---- | ---------- | ----------------: | ---------: | -------: |
| mq-bridge-app | none (passthrough) | 1.080 s | **925,972 rows/s** | 136 MiB |
| Redpanda Connect | none (passthrough) | 9.792 s | 102,123 rows/s | 262 MiB |
| mq-bridge-app | native `transform` (no plugin) | 1.305 s | **766,280 rows/s** | 174 MiB |
| mq-bridge-app | **Connect plugin** middleware (`connect_mapping`, Bloblang) | 6.149 s | 162,618 rows/s | 292 MiB |
| Redpanda Connect | mapping | 15.211 s | 65,741 rows/s | 382 MiB |

**mq-bridge-app is ~9.1x faster on the passthrough and ~11.7x on the projection.**
Every passthrough sink is 194,980,514 bytes and every projected sink 65,615,161.

**Notes.**

- **Redpanda Connect's input is not at its defaults here, in its favour.** It is the
  `redpanda` input with `unordered_processing` enabled and `batching.count: 1024`,
  the batch size mq-bridge-app uses. With the ordered default the same job stalled for
  seconds at a time and had not landed the topic after 40 s. This mirrors the
  `fetch.queue.backoff.ms` option set on the mq-bridge-app side.
- **The plugin hop costs less here than in 10a** (~4.7x against the native
  `transform` instead of ~11x) because the Kafka read, not the mapping, carries more
  of the wall-clock. It is still ~2.5x faster than Redpanda Connect end to end.
- **Delivery is at-least-once on both sides**: each resumes from the committed
  consumer-group offset.
- **The calibration window is 8 s for Redpanda Connect**, not the usual 2 s. Its
  fetches can pause mid-backlog for longer than 2 s, which the harness would otherwise
  take for the end of the data and fail on the row count.

### 11 — Vector

**Measures.** The same two jobs as scenario 10, and a topic-to-topic copy, against
[Vector](https://vector.dev) 0.59.0: a single Rust binary that, like mq-bridge-app,
runs a source → transform → sink pipeline from a config file. As in scenario 10, every
row of a table was measured in one session, so compare within a table.

**Read the JSONL figures (11a, 11b) with this attached: Vector's file sink is the
limit, not its engine.** All four of those cells land at ~57,000 rows/s whatever work
they do.
The same Kafka source into Vector's `blackhole` sink read the whole topic in about
four seconds, and a `console` sink redirected to a file was no faster than the `file`
sink. Vector is built to ship logs and metrics to network sinks; writing a local
JSONL file is not what it is tuned for. Those rows say that mq-bridge-app is much
faster *at that job*, not that its engine is 12–50x faster than Vector's. **11c is the
engine comparison**: the same source into a Kafka sink, where the gap is ~1.8x.

#### 11a — CSV → JSONL

The scenario 6 job on the same fixture. Vector has no CSV codec and its `file` source
is a tailer that never ends, so the file arrives on stdin and a `remap` parses each
line ([`vector/csv_untyped.yaml`](vector/csv_untyped.yaml),
[`vector/csv_typed.yaml`](vector/csv_typed.yaml)); Vector exits at end of input.

**Run.**

```bash
benches/etl/run_csv_mqb.sh && benches/etl/run_csv_mqb.sh --untyped   # native rows
benches/etl/run_csv_vector.sh          # vector-untyped, vector (typed)
benches/etl/run_csv_vector.sh parity   # both outputs == mq-bridge-app's
```

**Result.** 1 warm-up + 3 timed runs per row:

| Tool | Typing | rows/s | Median wall-clock | Peak RSS |
| --- | --- | ---: | ---: | ---: |
| mq-bridge-app | none | **2,915,451** | 0.343 s ±0.009 | 30.6 MiB |
| Vector | none (`remap` with `parse_csv`) | 57,823 | 17.294 s ±0.083 | 158.5 MiB |
| mq-bridge-app | native `transform` | **1,564,945** | 0.639 s ±0.082 | 74.0 MiB |
| Vector | `remap` (`parse_csv`, `to_int`, `parse_json`) | 56,670 | 17.646 s ±0.094 | 168.0 MiB |

**mq-bridge-app is ~27.6x faster typed and ~50x untyped.** Both Vector outputs hold
the same 1,000,000 records as the matching mq-bridge-app output.

**Notes.**

- **Typing costs Vector 2%**, where it halves mq-bridge-app's rate. That is the sink
  bound showing: the `remap` is not what Vector is waiting on.
- **Parity is checked after sorting by `id`**, because Vector runs `remap`
  concurrently and does not keep the input order.
- **The fixture has CRLF line ends**, which Vector's stdin source keeps on the line;
  the `remap` strips them. Without that every row fails to parse and is dropped
  without a log line.
- **The config carries literal paths.** A sink path taken from an environment variable
  (`path: ${OUT}`) wrote no file on Vector 0.59.0, so the runner fills in the paths.
- **mq-bridge-app's peak RSS is the 10a measurement** (same build, same job); Vector's
  is a separate single run under `/usr/bin/time -l`.

#### 11b — Kafka → JSONL

The scenario 9 job with the scenario 9 stopwatch and projection. Vector's `kafka`
source is librdkafka, as mq-bridge-app's is, and gets the same
`fetch.queue.backoff.ms: 10` through `librdkafka_options`.

**Run.** Setup and seeding as in scenario 9, then:

```bash
export MQB_CONSUMER_OPTIONS='[["fetch.queue.backoff.ms","10"]]'
benches/etl/run_kafka_stream.sh mqb      # native passthrough + projection
benches/etl/run_kafka_stream.sh vector   # passthrough + remap projection
```

**Result.** All on the host, 1 warm-up + 3 timed runs per row:

| Tool | Projection | Median wall-clock | Throughput | Peak RSS |
| ---- | ---------- | ----------------: | ---------: | -------: |
| mq-bridge-app | none (passthrough) | 1.082 s | **924,485 rows/s** | 126 MiB |
| Vector | none (passthrough) | 17.203 s | 58,130 rows/s | 335 MiB |
| mq-bridge-app | native `transform` | 1.384 s | **722,337 rows/s** | 179 MiB |
| Vector | `remap` | 17.412 s | 57,431 rows/s | 346 MiB |

**mq-bridge-app is ~15.9x faster on the passthrough and ~12.6x on the projection.**
Every passthrough sink is 194,980,514 bytes and every projected sink 65,615,161.

**Notes.**

- **The passthrough and the projection cost Vector the same**, for the reason given at
  the top of this scenario.
- **Delivery is at-least-once on both sides.**

#### 11c — Kafka → Kafka

The 11b source into a Kafka topic instead of a file: a job Vector is commonly deployed
for, and one that does not go through its file sink. Each run writes to a fresh
4-partition topic on the same broker; the stopwatch stops when that topic's end
offsets add up to the 1,000,000 source records.

**Run.** Setup and seeding as in scenario 9. The harness reads the end offsets with
`kafka-python`, which the runner pulls in through `uv`:

```bash
export MQB_CONSUMER_OPTIONS='[["fetch.queue.backoff.ms","10"]]'
benches/etl/run_kafka_stream.sh kafka-sink   # both tools, passthrough + projection
```

**Result.** All on the host, 1 warm-up + 3 timed runs per row:

| Tool | Projection | Median wall-clock | Throughput | Peak RSS |
| ---- | ---------- | ----------------: | ---------: | -------: |
| mq-bridge-app | none (passthrough) | 3.553 s ±0.238 | **281,489 rows/s** | 180 MiB |
| Vector | none (passthrough) | 6.476 s ±0.066 | 154,414 rows/s | 337 MiB |
| mq-bridge-app | native `transform` | 3.827 s ±0.203 | **261,322 rows/s** | 184 MiB |
| Vector | `remap` | 6.856 s ±0.347 | 145,852 rows/s | 352 MiB |

**mq-bridge-app is ~1.8x faster on both, at about half the memory.**

**Notes.**

- **Producer settings are each tool's defaults.** Both are librdkafka with `acks=all`
  and no compression; mq-bridge-app adds idempotence and lingers 1 ms, Vector 5 ms.
- **Only the record count is asserted** (exactly 1,000,000 in the destination topic on
  every run); the destination records are not compared with each other.
- **Both sides are producing to a single broker in a Docker VM**, and the clock
  includes process start and the consumer group join, which weigh more on a 3.5 s run
  than on a 17 s one. Treat ~1.8x as the size of the gap, not as two digits.

## Typed vs. untyped: how to read the Sling ratios

Sling does schema inference and type conversion: given a CSV row it emits typed values
and re-parses nested JSON into real objects. mq-bridge-app's readers do not infer
types; they pass values through as strings. Comparing those two directly would time a
string passthrough against a tool that parses and re-types every row, which is not a
benchmark. The two Sling scenarios handle this differently:

| Scenario | mq-bridge-app does | Sling does | Ratio | Like-for-like? |
| --- | --- | --- | --- | --- |
| 6, typed column | types via `transform` | types | ~14.6x | **yes**, outputs asserted identical |
| 6, untyped column | passes strings through | types | ~28x | **no**, do not quote; this column belongs against Meltano |
| 5 | passes values through | types | ~4.0x | **no**, not yet equalised |

**Scenario 6 (CSV → JSONL): equal work.** The typed run carries a `transform`
middleware ([`schemas/bench.json`](schemas/bench.json)) that reproduces Sling's output
exactly: `coerce` widens the `id` string to an integer, and
`contentMediaType: application/json` decodes the embedded `attributes` document into a
nested object. Both tools then emit the same records:

```jsonc
// sling (defaults) and mq-bridge-app (+ transform) — identical records
{"id":1,"amount":"6767.32","attributes":{"score":2.501,"tier":"free"}}
```

This is asserted, not assumed. [`compare_jsonl.py`](compare_jsonl.py) diffs the two
outputs record-by-record and **fails the run** on any mismatch, so the typed number
cannot be published unless all 1,000,000 rows match. Records are compared as parsed
JSON: mq-bridge-app preserves the source column order inside `attributes` while Sling
alphabetizes it, which is a serialization difference, not a data one.

Making the work equal is not free, and the harness measures the cost directly by
running mq-bridge-app both ways in the same session: **3,134,796 untyped → 1,626,016
typed**, so the transform costs ~0.30 µs/row (~48% of wall-clock). The margin against
Sling is therefore ~14.6x, not the ~28x the untyped run would suggest.

Both configurations stay published on purpose. The untyped number is the correct
comparison against any tool that also does no transformation (Meltano's `tap-csv`
emits every field as a string), and it is what makes the transform's cost auditable
instead of baked invisibly into one figure. What it must *not* be used for is a
comparison against a type-inferring tool.

**Scenario 5 (Postgres → JSONL): not yet equalised.** That scenario runs mq-bridge-app
untyped against a type-inferring Sling, so part of its ~4.0x gap is mq-bridge-app
doing less. Read it with that attached. Applying the same treatment there needs a
Postgres-shaped schema (the driver already returns typed values for some columns, so
it is not a copy of `bench.json`) and a re-run.

## Published benchmarks these line up against

- **Debezium**: Postgres CDC latency and throughput → scenario 2.
- **OpenMessaging Benchmark**: payload sizes and latency-percentile reporting →
  scenarios 1 & 3.
- **Airbyte**: records/s for a full-table sync → scenario 1.
