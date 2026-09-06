# ETL / CDC-comparable benchmark methodology

This suite exists so that mq-bridge can be evaluated **like-for-like** against
config-driven ETL/CDC tools for moving Postgres data — not against
microbenchmarks of unrelated primitives. Credibility is the asset here: every
published number is reproducible from a pinned workload and a pinned backend
version, and the methodology is stated up front rather than cherry-picked.

> **Where the measured numbers live.** All ETL/CDC results — with their Sling,
> Meltano, Arroyo and Sea Streamer baselines, the fixed parameters each was taken
> under, and the exact commands — are published in **one** place:
> [`apps/mq-bridge-app/benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md).
> This page deliberately does not repeat those tables, so there is a single copy to
> keep current. It covers *why* the suite is shaped the way it is, and what the
> library's own Criterion harness measures.

## Two harnesses, two jobs

| Harness | What it measures | Where |
| --- | --- | --- |
| **Criterion** (`benches/performance_bench.rs`) | Per-backend batched throughput for the library itself, published to the [benchmark dashboard](https://marcomq.github.io/mq-bridge/dev/bench/). Covers `memory` with no external service, and the Docker-backed brokers. | this repo, `benches/` |
| **ETL harness** (shell + `mq-bridge-app`) | Whole-job ETL/CDC scenarios driven the zero-code way a real user drives them — CLI and YAML, no bespoke Rust — so the output is comparable to how other ETL tools publish theirs. | [`apps/mq-bridge-app/benches/etl/`](https://github.com/marcomq/mq-bridge/tree/dev/apps/mq-bridge-app/benches/etl) |

The ETL harness is the one that produces the published comparisons. The Criterion
harness is the continuous per-backend regression signal.

## Scenario definitions

The scenarios and the fixed parameters they run under (payload sizes, message
counts, batch sizes, concurrency, Postgres image, warm-up) are defined once, in the
ETL harness README's
[Fixed parameters](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md#fixed-parameters-printed-next-to-every-number)
and
[Scenarios & commands](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md#scenarios--commands)
sections. In outline:

1. **Bulk-insert / table→table throughput** (§1 & §3) — `postgres → postgres` via the
   zero-code `copy` command, at batch 1 vs 128, answering "how fast can mq-bridge
   load a table" and quantifying the batching lever.
2. **CDC event-to-sink latency** (§2) — a `postgres_cdc → null` route started over the
   app's HTTP API, reporting p50/p95/p99 of the engine's per-event processing time.
   This is *not* Debezium's end-to-end commit→sink figure; it excludes WAL
   propagation delay, and must be reported with that caveat.
3. **Postgres → JSONL** (§5) and **CSV → JSONL** (§6) — the two headline full-dataset
   ETL jobs, against Sling and Meltano baselines, reporting throughput **and** peak RSS.
4. Additional coverage: local IPC (§4), the MCP server path (§7), Postgres' own
   `psql`/`pg_dump` tools (§8), and the Kafka streaming comparisons against Arroyo and
   Sea Streamer.

## Running the Criterion harness

```bash
# All backends the enabled features cover. Docker-backed backends bring their own
# compose file up; `memory` needs no external service.
cargo bench --features test-utils --bench performance_bench

# One backend only — the criterion filter matches the benchmark id:
cargo bench --features test-utils --bench performance_bench -- memory
```

There is **no CDC benchmark in this harness.** CDC latency is measured by the ETL
harness (`benches/etl/run_cdc_latency.sh`), which needs the app binary and a Postgres
with logical replication; see its README section §2. The two other Criterion benches in
this directory are unrelated to ETL: `router_bench` (per-request HTTP route lookup) and
`zeromq_backends` (an A/B of the two ZeroMQ backends).

## Reporting rules

These are the rules the published tables follow, and any new number must too:

- **Print the parameters next to every number** — payload, batch, concurrency, row
  count, and the machine. Never publish a single favourable figure without its
  methodology row.
- **Print the version.** Engine and `mq-bridge-app` version, plus the tool version for
  every baseline. A table whose rows come from different sessions must say so.
- **Print the allocator.** The shipped binaries use mimalloc (the default `full`
  feature set). System-allocator numbers are not comparable with them and must not
  share a table.
- **Equalise the work before quoting a ratio.** Where a baseline does type inference
  and mq-bridge does not, either add the matching `transform` and assert output parity,
  or state plainly that the comparison is not equalised. See the ETL README's
  [note on the Sling comparison](https://github.com/marcomq/mq-bridge/blob/dev/apps/mq-bridge-app/benches/etl/README.md#a-note-on-the-sling-comparison).
- **Measure on an idle machine with free disk.** A parallel build, a busy browser, or a
  near-full filesystem all move these numbers by double-digit percentages — enough to
  invent a regression that is not there.
