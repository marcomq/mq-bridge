# Troubleshooting

Common failure modes and how to read them. When something "silently does nothing", the cause is
almost always one of the drop/drain behaviours below.

## Messages disappear and the route still ends `completed`

By design, a message that fails **permanently** (a type/data error the sink rejects, a poison
payload a handler rejects) is logged at `error` level and **dropped** — the route keeps
processing the rest of the batch. So a *systematic* failure (e.g. every row hitting a
column-type mismatch) drains the input while delivering nothing to the sink and still ends `completed`.
The terminal route status is not clean, however: its error reports the number of dropped
messages and the last rejection cause.

- Watch for a burst of `Dropping message … due to non-retryable error` in the logs — that's the
  signal.
- Check the route status/error in the UI, CLI, or MCP response for the retained drop count and
  cause; do not treat `outcome: completed` alone as proof that every message was delivered.
- Add a [`dlq`](../engine/reference.md#dlq) to capture the failures for inspection/replay.
- `retry` alone does **not** retain a failed message — pair it with `dlq` (dlq last). See
  [Dead-letter queues](../cookbook/dlq.md).

## `--drain` / `exit_on_empty` behaviour

- `--drain` (CLI) / `exit_on_empty` (MCP/route) exits once the source yields an **empty batch** —
  right for finite sources (a file, a full-table read). For brokers and CDC sources that never
  "end", omit it and run continuously.
- **A drained Redis Streams source reports an error** at the end: `"healthy": false` with
  `Redis XREAD failed: timed out`, even though every message moved. Cosmetic — verify the sink,
  not the final status. NATS does not behave this way.
- **Finished MCP routes are not reaped**: a route started with `exit_on_empty` stays in
  `list_routes` after draining. Check `"finished"` / `"outcome"` on the entry, or call
  `stop_route` to clear it. See [MCP known limitations](../MCP.md#known-limitations).

## IPC: publisher fails to connect or blocks

The `memory` endpoint's IPC transport is **unidirectional, publisher → consumer**, and the
**consumer is the server**:

- The **consumer must be running before the publisher connects** — otherwise the publisher fails
  with a connection error. Start the consumer-side route first.
- `send_batch` **blocking is normal backpressure**, not a fault: socket buffers are small (8 KiB
  on macOS), so a batch larger than the buffer only completes once the consumer drains it. After
  5 s blocked the publisher logs a warning naming the socket and keeps waiting.
- A consumer that has accepted but **stopped draining** stalls the publisher indefinitely — make
  sure the consumer side is actually reading.
- Named `ipc://name` resolves to different paths for different users/services (the
  `/run` → `$XDG_RUNTIME_DIR` → `/tmp` fallback). When both sides must agree, use an **explicit
  path** (`ipc:///run/myapp/queue.sock`).
- A **second publisher** on the same socket connects without an error but is only read after
  the first disconnects. Its sends block once the socket buffer is full.
- A send is acknowledged once it is in the socket buffer, and IPC redelivery is
  **consumer-local**: neither survives a consumer crash. Use a
  [directory spool](../connectors/dir-spool.md) or a broker for a durable hand-over. See
  [Cross-process IPC bridge](../tutorials/ipc-bridge.md).

## Request/reply timeouts and dropped responses

- The `response` output requires an input that carries a reply channel (`http`, `websocket`,
  `grpc`, or a request/reply `nats`/`mongodb`/`memory`). If the input does **not** support
  responses (File, SQLx, …), the message sent to `response` is dropped.
- Configure a generous timeout on the **requester** side — bridge processing adds latency.
- Middleware that drops metadata (like `correlation_id`) breaks the response chain. Keep the
  correlation metadata intact through the route.
- MongoDB's reply pattern uses emulation that waits for messages; misconfigured timeouts can
  cause severe stalls. Test it before relying on it.

## Ordering looks wrong

With `concurrency > 1`, batches process in parallel, so **strict per-key ordering across the
route is not guaranteed** — side effects at the sink can interleave. Commits are still sequenced
for cumulative-ack brokers. If you need ordering, set `concurrency: 1` or partition upstream so
each key lands on one route. See [Performance tuning](tuning.md#concurrency--sequential-vs-worker-pool).

## NATS JetStream: "no stream found for given subject"

When mq-bridge auto-creates a JetStream stream it scopes it to `{stream}.>`, so the **subject
must be prefixed with the stream name**: `stream: "orders_stream"` needs a subject like
`orders_stream.foo`. Also, `stream` is **required even in Core NATS mode** (`no_jetstream:
true`) — pass any placeholder string. See
[NATS JetStream notes](../engine/configuration.md#nats-jetstream-notes).

## MongoDB source: "capture_all needs a replica set"

Both `capture_*` modes read the oplog, so they need a replica set — a single-node one is
enough — and refuse to start without one rather than falling back. The removed fallback paged
by `_id`, which only ever returns documents above its high-water mark, so anything a concurrent
writer committed below that mark was dropped silently.

- One-shot, non-destructive read of a standalone `mongod`: `consume: snapshot`.
- Work queue (destructive, competing readers): `consume: consumer`.
- Turning the standalone into a single-node replica set (`replSet` + `rs.initiate()`) restores
  `capture_all`, and with it resumable reads.

## ZeroMQ peers stopped understanding each other after upgrading

0.4.0 changed the `zeromq` default `format` from `json` to `raw_framed`. A 0.4 peer and a 0.3
peer no longer interoperate on the same socket until they are pinned to the same format, and the
fix belongs on the **0.4 peer**: set `format: json` there. Setting it on the 0.3 peer changes
nothing — `json` is already its default, and the 0.4 peer stays on `raw_framed`. (Pinning both to
`raw_framed` works too, since 0.3 also understands it.) The symptom is a peer that receives frames
but reads them as garbage, or a payload that arrives as JSON text instead of the decoded message.

`format` does not apply to REQ/REP replies: a REP peer always answers with a JSON array of
canonical messages and a REQ publisher always decodes one, whatever `format` is set to.

## SQLx source: "reconnecting forever" or rejects the table

An SQLx source with **no** `cursor_column` is a competing-consumers **work queue** and requires
the queue schema (`id`, `payload`, `locked_until`). A plain table without `locked_until` fails
fast with a permanent error. To read an arbitrary table non-destructively, set `cursor_column`
to switch to cursor polling. See [Endpoints](../reference/endpoints.md#database-sources-change-capture-vs-polling).

## Encrypted / compressed file source ends immediately or `failed`

A file **source** must declare the same `compression`/`encryption` the data was written with. A
mismatch (wrong key, wrong codec, missing field) is a permanent decode failure — the route ends
`failed` with the error in its status. Reading a compressed file with **no** `compression` set is
rejected up front by magic-byte sniffing. See [Encryption at rest](../cookbook/encryption.md).

## Postgres bulk copy is slow / near-quadratic

The Postgres keyset-cursor reader does `WHERE id > $cursor ORDER BY id LIMIT batch`. **Without an
index on the cursor column it does a full scan per batch.** Create one:
`CREATE INDEX ON <table>(id)`. See [Performance tuning](tuning.md#reference-numbers).

## Throughput numbers look wrong

Measure on a **release build** — a debug build reports dramatically slower rates. Through the
MCP server, call `server_info` first to confirm the build `profile`. See
[Measuring](tuning.md#measuring).

## Float values change in the last bit

Numbers move through payloads as JSON, and serde_json's default parser shifts ~1 ULP on ~19% of
17-significant-digit doubles. For bit-exact float round-tripping across every endpoint, build
with the `float-roundtrip` feature. See
[middleware `encryption` notes](../engine/reference.md#encryption).
