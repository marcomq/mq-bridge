# Performance tuning

`mq-bridge` is fast by default and stays out of your way until you need more. This page is the
map of the knobs that trade throughput against latency, memory, and ordering — what each one
does, when raising it helps, and when it hurts.

## Philosophy: fast by default, safe knobs

The engine is optimized around **batch-shaped APIs** on every endpoint, even when the backend
only has a single-message primitive. Batching costs nothing when there is nothing to batch: a
route waits for one message and then takes whatever else is already queued, so a busy pipeline
fills large batches while an idle one still ships each message as it arrives. Parallelism is
the knob that stays opt-in.

Two different starting points are worth knowing:

- **Library / config-mode routes** default to `batch_size: 512`, `concurrency: 1` — the engine
  primitive default.
- **The `copy` CLI and the MCP server** default to `batch_size: 1024`, `concurrency: 4`
  (`--batch-size` / `--concurrency` on `copy`), because they exist for bulk moves. The
  benchmark numbers below use `copy`.

When throughput matters, **`batch_size` is almost always the first knob to try.**

## `batch_size` — the single most important knob

`batch_size` is the maximum number of messages a route gathers per iteration before handing the
batch to the sink. Larger batches amortize per-call overhead (network round-trips, SQL
statement prep, file syscalls) across many rows.

| Field | Default (library) | Default (`copy`) |
|---|---|---|
| `batch_size` | `512` | `1024` |

**When bigger helps:** row-oriented and high-latency sinks — SQL `INSERT`, HTTP POST, object
storage, ClickHouse. These pay a fixed cost per call, so moving from 1 to 128–1024 rows per
call is often a 10–100x throughput change. Databases especially: one multi-row insert beats a
thousand single-row inserts.

**When bigger hurts:**

- **Memory.** Nothing caps a batch by bytes — only by message count. The whole batch is held in
  memory (and, over IPC, written as one frame — frames over 100 MB are rejected), so on a route
  carrying MB-scale payloads the limit is set by payload size and `batch_size` has to come down.
- **Redelivery granularity.** A nack redelivers at batch granularity on some transports, so a
  big batch means more replayed work after a failure — and a batch that fails as a whole takes
  more healthy messages down with it.
- **Latency under load.** A route never waits to fill a batch, so an idle bridge is unaffected.
  Once the source is faster than the sink, though, a message queued behind a large in-flight
  batch waits for that batch to ship. Where that matters, keep `batch_size` small — or use a
  [`buffer`](../engine/reference.md#buffer) middleware with a `max_delay_ms` bound.

### Choosing `batch_size`

| Workload | Recommended `batch_size` |
|---|---|
| Bulk DB → file / file → DB ETL | `512`–`1024` |
| Broker → DB sink (row inserts) | `128`–`512` |
| Low-latency request/response or event bridge | `1`–`16` |
| Large payloads (MB-scale) | keep small; watch peak RSS and the 100 MB IPC frame cap |

## `concurrency` — sequential vs. worker pool

`concurrency` is the number of route worker tasks that process batches in parallel.

| Field | Default (library) | Default (`copy`) |
|---|---|---|
| `concurrency` | `1` | `4` |

**When it helps:** high-latency **handlers** or **sinks** where a worker spends most of its time
awaiting I/O — HTTP calls, remote databases. More workers keep the pipeline full while others
wait.

**Ordering implications.** With `concurrency > 1`, batches are processed in parallel, so
**strict per-key ordering across the route is not guaranteed.** Commits are still sequenced for
cumulative-ack brokers (a later batch cannot ack over an earlier unresolved one), but the *order
of side effects* at the sink can interleave. If you need ordering, keep `concurrency: 1` or
partition upstream so each key lands on one route.

**When it does *not* help.** Concurrency does not speed up sources that fetch serially. A
**MongoDB source** fetches batches serially — `concurrency` only widens the downstream side, not
the read. A single-connection cursor read is likewise source-bound. Raising `concurrency`
against a serial source just adds idle workers.

### Choosing `concurrency`

| Workload | Recommended `concurrency` |
|---|---|
| CPU-light copy, fast local sink | `1`–`4` |
| Per-message HTTP call / remote sink (I/O-bound) | `4`–`16` |
| Order-sensitive stream | `1` |
| MongoDB / serial-read source | `1` (widen downstream instead) |

`commit_concurrency_limit` (default `4096`) caps in-flight commit operations, whether queued
through ordered sequencing or run concurrently for independent-ack transports. Rarely needs
changing.

## Connection pooling / reuse

Publishers targeting the same server **share one underlying transport client by default** —
consolidating TCP connections, background threads, and batching, following each driver's own
guidance (one shared producer / client / pool per application). Sharing applies to **Kafka,
NATS, MongoDB, SQLx, HTTP, and gRPC**, keyed by connection-level settings (URL, auth, TLS),
never by topic/subject/collection.

Set `shared: false` on a publisher for a dedicated connection when sharing works against you:

- **Kafka** — isolate a latency-sensitive topic from a high-throughput one so they don't share
  one internal send queue (head-of-line blocking).
- **SQLx** — a shared pool's `max_connections` is a budget spread across every route on that
  database; give a hot route its own pool.
- **gRPC** — a shared channel multiplexes over one HTTP/2 connection; at very high concurrency
  its max-concurrent-streams cap can bottleneck.

See [Connection sharing](../engine/configuration.md#connection-sharing).

## Retry & backoff

The [`retry`](../engine/reference.md#retry) middleware retries only `Retryable` and
connection errors, with exponential backoff (`initial_interval_ms`, `multiplier`,
`max_interval_ms`). Tuning notes:

- Backoff caps at `max_interval_ms`, so retries don't stall a route indefinitely; pick a cap
  that matches your latency budget.
- Retry interacts with [`dlq`](../engine/reference.md#dlq): once attempts are exhausted the
  error is marked permanent so a following `dlq` captures it. **Put `dlq` after `retry`** on the
  output (last = outermost). Without a `dlq`, an exhausted message is dropped.
- Several endpoints already retry connection/timeout errors internally, so `retry` adds backoff
  *on top* — you don't need huge `max_attempts`.

## Compression & encryption cost

- **Compression** trades CPU for smaller output, whether it's the `file` / `object_store` batch
  field (`compression: none|gzip|lz4|zstd`) or the
  [`compression` middleware](../engine/reference.md#compression) compressing payloads over the
  wire. `lz4` is cheapest; `zstd` compresses best; `gzip` is roughly 33% slower than zstd at
  similar ratios in practice. The middleware frames **per message**, so it pays its codec cost
  far more often than the batch field — prefer the endpoint field when the sink is a file or
  object. See the [Compression](../cookbook/compression.md) recipe.
- **Encryption** costs an AEAD seal/open per batch. Do **not** stack the
  [`encryption`](../engine/reference.md#encryption) middleware on top of a sink's batch
  `compression` — ciphertext does not compress; use the file endpoints' own compress-then-encrypt
  fields instead. See [Encryption at rest](../cookbook/encryption.md).
- A `buffer` middleware in front of a compressing sink increases batch size, which improves
  compression ratio and amortizes the codec cost.

## Measuring

Never trust a single number without its methodology. Measure with a **release build** (a debug
build reports dramatically slower rates — if you're driving the [MCP server](../MCP.md),
call `server_info` first to confirm the build profile), record CPU/cores/RAM, and report the
exact `batch_size` / `concurrency` next to every figure.

- Attach the [`metrics`](../engine/reference.md#metrics) middleware and scrape the
  [Prometheus endpoint](observability.md) for live throughput/latency/error rates.
- For the MCP server, read `average_messages_per_second` for a **finished** job and
  `messages_per_second` for a **running** one (see [MCP route status](../MCP.md#route-status)).
- The like-for-like ETL/CDC methodology and fixed parameters are in
  [`benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/benches/etl/README.md).

### Reference numbers

Measured through `mq-bridge-app`'s `copy` CLI (the zero-code path) on an Apple M1, 8 cores,
8 GB RAM. **Numbers are hardware-dependent — treat them as shape, not guarantees.**

<!-- Pulled from the benchmark harness README, which is the single source of truth for
     every measured number. Do not paste a copy here — update it there. -->
{{#include ../../../benches/etl/README.md:reference_numbers}}

All rows measured with the mimalloc allocator used by the shipped binaries. It is
the default-on `mimalloc` cargo feature (also implied by `bench`); build with
`--no-default-features` and without it in the feature list to fall back to the
system allocator on platforms where mimalloc is unsupported. The `Version` column
matters: the rows are not all from one measurement session.

Two things the table shows:

- **Typing has a real but modest cost.** Adding a `transform` that coerces `id` to an integer
  and decodes an embedded JSON document costs ~0.26 µs/row — CSV→JSONL drops from 2.82M to 1.64M
  rows/s, about 1.73x, but every output record is fully typed.
- **Peak RSS does not scale with dataset size**, because rows stream in batches rather than
  being buffered whole — at the fixed batch size and concurrency above, ~28 MiB for a
  passthrough copy however large the input. It is not a constant: batch size, connector-side
  buffering, allocator retention and transforms all move it. The
  typing `transform` is the exception: its per-row JSON decode and buffering push peak RSS to
  ~68 MiB, still far leaner than tools that materialize the dataset.

> **Keyset cursor needs an index.** The Postgres bulk-copy reader uses keyset pagination
> (`WHERE id > $cursor ORDER BY id LIMIT batch`). Without an index on the cursor column it does
> a full scan per batch (near-quadratic). `CREATE INDEX ON <table>(id)` first.

## A tuning checklist

1. Start from the defaults. Confirm correctness before tuning.
2. Raise `batch_size` (128 → 512 → 1024) and re-measure. This is usually the biggest win.
3. If the sink or handler is I/O-bound and order doesn't matter, raise `concurrency`.
4. Add `buffer` if the source emits singles but the sink prefers batches.
5. For hot Kafka/SQLx/gRPC publishers colliding with others, set `shared: false`.
6. Add `retry` + `dlq` for resilience; keep `max_attempts` modest.
7. Measure on a release build, with metrics, and record the parameters.
