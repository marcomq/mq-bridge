# ETL / CDC-comparable benchmark methodology

This suite exists so that mq-bridge can be evaluated **like-for-like** against
config-driven ETL/CDC tools for moving Postgres data — not against
microbenchmarks of unrelated primitives. Credibility is the asset here: every
number below is reproducible from a pinned workload and a pinned backend
version, and the methodology is stated up front rather than cherry-picked.

The existing criterion harness (`benches/performance_bench.rs`, published to the
[benchmark dashboard](https://marcomq.github.io/mq-bridge/dev/bench/)) already
measures per-backend batched throughput. This document defines the additional
**ETL/CDC-shaped scenarios** and the fixed parameters they run under.

## Fixed parameters (keep constant across runs and across tools)

| Parameter        | Value                                             |
| ---------------- | ------------------------------------------------- |
| Message payload  | 256 B and 4 KiB JSON rows (report both)           |
| Message count    | 100_000 per run                                   |
| Batch sizes      | 1 (unbatched) and 128 (batched)                   |
| Concurrency      | 1 and 4 route workers                             |
| Postgres         | `postgres:16-alpine`, `wal_level=logical`         |
| Warm-up          | criterion default + 5_000-message pre-roll        |
| Machine          | record CPU model, cores, RAM in the published run |

Report the **backend version and the exact parameters next to every number**.
Never publish a single favourable figure without its methodology row.

## Scenarios

1. **Bulk-insert throughput** — `memory → sqlx(postgres)` sink, measured in
   rows/second at batch sizes 1 and 128. Answers "how fast can mq-bridge load a
   table". Uses `docker-compose/postgres_cdc.yml`.

2. **CDC event-to-sink latency** — insert a row into a captured table and
   measure the time until the corresponding change event is delivered to a
   `memory` sink through the `postgres_cdc` endpoint (p50 / p95 / p99). Answers
   "how quickly does a change propagate". Uses `docker-compose/postgres_cdc.yml`.

3. **Batched vs. unbatched throughput** — the same source→sink route at
   `batch_size: 1` vs `batch_size: 128`, per backend, to quantify the batching
   lever the README documents. Runs on `memory` (no external service) and on
   each broker that has a compose file.

## Running

```bash
# Batched-vs-unbatched (no external services)
cargo bench --features test-utils --bench performance_bench

# CDC latency + bulk-insert (needs Docker Postgres with logical replication)
docker compose -f tests/integration/docker-compose/postgres_cdc.yml up -d --wait
cargo bench --features "test-utils postgres-cdc" --bench performance_bench -- cdc
docker compose -f tests/integration/docker-compose/postgres_cdc.yml down -v
```

## Results

The scenarios are run through
[`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) (the zero-code, config-driven
path a real user takes) so the numbers are comparable to how other ETL/CDC tools publish
theirs. The run brief lives in [`mq-bridge-app-benchmark-prompt.md`](mq-bridge-app-benchmark-prompt.md).

> ⚠️ **Preliminary — measured on battery power.** The numbers below were measured on a
> laptop running on battery, where macOS throttles the CPU. They are **not yet authoritative**
> and are pending a re-run on AC (mains) power before publication. Treat them as a floor, not
> confirmed figures.

**Environment:** Apple M1, 8 cores, 8 GB RAM, macOS (battery power) · via `mq-bridge-app` 0.2.6 · postgres:16

### Preliminary run — bulk copy Postgres → JSONL

| Scenario | Payload | Batch | Concurrency | Source → Sink | Result |
| --- | --- | --- | --- | --- | --- |
| Bulk-copy throughput | 7-col mixed-type rows, 1,000,000 rows | 1024 | 4 | postgres (keyset cursor) → file (JSONL) | **384,615 rows/s** (2.600 s) |

Command: `mq-bridge-app copy --from postgres://…?table=…&cursor_column=id --to file://…?format=raw --drain --batch-size 1024 --concurrency 4`, wall-clocked. Requires an index on the cursor column (`CREATE INDEX ON <table>(id)`) — without it the keyset-pagination reader (`WHERE id > $cursor ORDER BY id LIMIT batch`) does a full scan per batch (near-quadratic).

### Preliminary run — Postgres → JSONL vs. Meltano

Same source table (`bench`: 1,000,000 rows, 7 mixed-type columns), same Postgres instance, same machine, both one-shot full-table syncs to a local JSONL file — mq-bridge-app's `copy` CLI vs. Meltano's `tap-postgres` → `target-jsonl` (median of timed runs each side, row-count parity verified).

| Scenario | Payload | Batch | Concurrency | Source → Sink | Throughput | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| mq-bridge-app `copy` | 7-col, 1,000,000 rows | 1024 | 1 | postgres (keyset cursor) → file (JSONL) | **338,066 rows/s** | 39.8 MiB |
| Meltano (`tap-postgres` → `target-jsonl`) | 7-col, 1,000,000 rows | default Singer config | — | postgres → JSONL | 15,356 rows/s | 599.7 MiB |

**~22x faster and ~15x leaner in peak memory** than Meltano in this scenario. Full setup (including the Meltano project config) is in [mq-bridge-app's `benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md#5--postgres--jsonl-vs-meltano-tap-postgres--target-jsonl).

### Preliminary run — CSV → JSONL vs. Meltano

Same seeded dataset both sides, same machine, one-shot full-file CSV → local JSONL (one JSON object per input row) — mq-bridge-app's `copy` CLI vs. Meltano's `tap-csv` → `target-jsonl`.

| Scenario | Payload | Batch | Concurrency | Source → Sink | Throughput | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| mq-bridge-app `copy` | 7-col mixed-type, 1,000,000 rows (~116 MiB) | 1024 | 1 | file (CSV) → file (JSONL) | **1,133,786 rows/s** | 21.9 MiB |
| Meltano (`tap-csv` → `target-jsonl`) | 7-col mixed-type, 1,000,000 rows | default Singer config | — | file (CSV) → JSONL | ~19,500 rows/s | 443.8 MiB |

**~58x faster and ~20x leaner in peak memory** than Meltano in this scenario. Full setup is in [mq-bridge-app's `benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md#6--csv--jsonl-vs-meltano).

## Status

Scenario 3 (batched vs. unbatched) is covered by the existing harness. Scenarios
1 and 2 (bulk-insert, CDC latency) share the Docker Postgres environment with the
CDC integration tests (`tests/integration/postgres_cdc.rs`); their criterion
wiring is added and validated together with that Docker run so the published
numbers are real, not estimated.
