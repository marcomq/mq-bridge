# Introduction

<p align="center">
  <img src="images/logo.png" alt="mq-bridge-app" width="128" height="128">
</p>
<p style="margin-top:-12px" align="center"><em>crossing streams</em></p>

**mq-bridge** moves data reliably between message brokers, databases, files, and HTTP — with
batching, retries, dead-letter queues, deduplication, and change data capture built in. One Rust
engine, one config format, two ways to use it:

| | **mq-bridge** — the library | **mq-bridge-app** — the application |
| :--- | :--- | :--- |
| **What** | The engine, embedded in your own service | The same engine as a desktop app, CLI / server, and MCP server |
| **For** | Code that moves data and adds business logic in handlers | Moving data by config alone — no code, no build step |
| **Languages** | Rust, Python, Node.js | YAML, environment variables, or one-line `mqb copy` |
| **Start** | [Quick start: library](getting-started/library-quick-start.md) | [Quick start: `copy`](quick-start.md) |

Almost everything in this book applies to both: connectors, middleware, delivery guarantees, and
tuning share the same settings. A CLI URL's query parameters are the same fields you write in a
library's YAML config.

Supported integrations include **Kafka**, **RabbitMQ (AMQP)**, **NATS**, **AWS SQS / SNS**,
**MQTT**, **IBM MQ**, **HTTP**, **WebSocket**, **gRPC**, **ZeroMQ**, **MongoDB**, **Redis
Streams**, **ClickHouse**, **Postgres CDC**, **SQLx (MySQL, MariaDB, PostgreSQL, SQLite)**,
cloud object storage, and filesystem endpoints — plus Pulsar, Meilisearch, and Redpanda Connect
through [plugins](reference/endpoints.md#plugin-endpoints).

## A quick taste

In code — the same route runs in Rust, Python, or Node.js:

```python
from mq_bridge import Route
Route.from_file("routes.yaml", "kafka_to_nats").run()
```

Without code — `mqb copy` moves data between databases, queues, and files in a single line of
bash:

```bash
mqb copy \
  --from 'postgres://user:pass@localhost/db?table=src&sslmode=disable' \
  --to   'file://out.jsonl?format=raw' \
  --drain
```

The **scheme selects the endpoint** and **query parameters configure it**, so any source→sink
pair is just one URL each. And it's quick: in benchmarks a 1,000,000-row Postgres → JSONL job
sustained **413,907 rows/s** at **~55 MiB peak RSS** (mq-bridge 0.4.20) — see
[Performance tuning](operations/tuning.md) for the full table and the conditions each row
was measured under.

## Philosophy

The project has one main bias: **move data reliably without forcing the rest of the
application to care too much about the transport.** Kafka offsets, RabbitMQ nacks, HTTP
responses, MongoDB polling, WebSocket frames, and file rows are all different in real life, but
route code should still be able to receive a batch, process it, publish it, and commit it.

- **Fast by default.** Every endpoint is optimized around batch-shaped APIs, and the headless
  surfaces ship tuned for throughput: the `copy` CLI and the MCP server default to
  `batch_size: 1024` and `concurrency: 4`. The library/config primitive defaults to
  `batch_size: 512`, `concurrency: 1`: batches fill opportunistically — a route takes whatever
  is already queued rather than waiting — so throughput comes for free while parallelism stays
  a deliberate per-route choice.
- **Reliability is built in, not bolted on.** Retries, dead-letter queues, deduplication, rate
  limiting, and cookie/session persistence wrap any endpoint. Ack/nack behaviour and retry/DLQ
  handling were designed to work *with* batching, including commit sequencing for
  cumulative-ack brokers.
- **Not a framework.** It is not a domain framework, an actor runtime, or a full stream
  processor. It cares about transport, routing, and delivery behaviour, not about prescribing
  your domain model.

## Where to go next

- Writing code? Start with the [library quick start](getting-started/library-quick-start.md),
  then [Embed the library](tutorials/embedding.md).
- Moving data without code? Start with [Installation](INSTALL.md) and the
  [`copy` quick start](quick-start.md).
- Starting from a problem? The [Use cases](use-cases/README.md) solve one each, such as
  keeping a RAG index in sync with Postgres or routing tickets with an LLM, with a runnable
  example.
- Want the end-to-end walkthroughs? See the [Tutorials](tutorials/postgres-cdc.md).
- Looking for a specific task? The [Cookbook](cookbook/upserts.md) has short recipes.
- Need exact fields and defaults? The [Reference](reference/endpoints.md) is authoritative.
- Running it in production? See [Operations](operations/deploying.md), especially the
  [Performance tuning](operations/tuning.md) page.
- Driving it from an AI agent? The same binary is an [MCP server](MCP.md) — the rows
  move without entering the model's context.

## Library vs. app

`mq-bridge` is the **engine/library**; `mq-bridge-app` is the **application** built on it. This
book covers both; the engine's Rust API reference lives on [docs.rs](https://docs.rs/mq-bridge),
and the [language bindings API](reference/bindings.md) covers Python and Node.js.
