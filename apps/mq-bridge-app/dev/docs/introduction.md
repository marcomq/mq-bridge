# Introduction

<p align="center">
  <img src="images/logo.png" alt="mq-bridge-app" width="128" height="128">
</p>
<p style="margin-top:-12px" align="center"><em>crossing streams</em></p>

`mq-bridge-app` is a **fast, single-command ETL and data-movement tool** built in Rust — and,
on top of the same engine, a multi-protocol bridge and traffic workbench for messaging.

It ships in **three forms that share one engine and one config format**: a **desktop app**
(visual workbench), a **CLI / server** (headless bridge and one-line `copy`), and a
**library** (embed the engine in Rust, Python, or Node.js). Design a route once, run it
anywhere — no rewrite in between.

Supported integrations include **Kafka**, **RabbitMQ (AMQP)**, **NATS**, **AWS SQS**,
**MQTT**, **IBM MQ** (optional), **HTTP**, **gRPC**, **ZeroMQ**, **MongoDB**, **Redis
Streams**, **ClickHouse**, **Postgres CDC**, **sqlx (MySQL, MariaDB, PostgreSQL, SQLite)**,
cloud object storage, and filesystem endpoints.

## A quick taste

At its core is a zero-config `copy` command that moves data between databases, queues, and
files in a single line of bash — no YAML, no pipeline definition, no code:

```bash
mqb copy \
  --from 'postgres://user:pass@localhost/db?table=src&sslmode=disable' \
  --to   'file://out.jsonl?format=raw' \
  --drain
```

The **scheme selects the endpoint** and **query parameters configure it**, so any source→sink
pair is just one URL each. And it's quick: in benchmarks a 1,000,000-row Postgres → JSONL job
sustained **421,220 rows/s** at **~46 MiB peak RSS** (mq-bridge 0.4.12) — see
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

- New here? Start with [Installation](INSTALL.md) and the
  [Quick start](quick-start.md).
- Want the end-to-end walkthroughs? See the [Tutorials](tutorials/postgres-cdc.md).
- Looking for a specific task? The [Cookbook](cookbook/upserts.md) has short recipes.
- Need exact fields and defaults? The [Reference](reference/endpoints.md) is authoritative.
- Running it in production? See [Operations](operations/deploying.md), especially the
  [Performance tuning](operations/tuning.md) page.
- Driving it from an AI agent? The same binary is an [MCP server](MCP.md) — the rows
  move without entering the model's context.

## App vs. engine

`mq-bridge` is the **engine/library**; `mq-bridge-app` is the **application** — desktop app +
CLI/server + library distribution — built on that engine. This book is the user-facing home;
the engine's deep API reference lives on [docs.rs](https://docs.rs/mq-bridge). The
[library bindings](reference/bindings.md) let you embed the same engine in Rust, Python, or
Node.js.
