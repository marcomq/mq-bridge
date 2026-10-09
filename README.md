<p align="center">
  <img src="apps/mq-bridge-app/crates/desktop/icons/icon.png" alt="mq-bridge" width="128" height="128">
</p>
<h1 align="center">mq-bridge</h1>
<p align="center"><em>crossing streams · library</em></p>

<p align="center">
<strong>One Rust engine to move data reliably between brokers, databases, files and HTTP — embedded in your Rust, Python or Node.js service.</strong>
</p>

[![Crates.io](https://img.shields.io/crates/v/mq-bridge.svg)](https://crates.io/crates/mq-bridge)
[![Docs.rs](https://docs.rs/mq-bridge/badge.svg)](https://docs.rs/mq-bridge)
[![Book](https://img.shields.io/badge/docs-book-orange)](https://marcomq.github.io/mq-bridge/)
[![Benchmark](https://github.com/marcomq/mq-bridge/actions/workflows/benchmark.yml/badge.svg)](https://marcomq.github.io/mq-bridge/dev/bench/)
![Linux](https://img.shields.io/badge/Linux-supported-green?logo=linux)
![Windows](https://img.shields.io/badge/Windows-supported-green?logo=windows)
![macOS](https://img.shields.io/badge/macOS-supported-green?logo=apple)
[![Supply chain](https://img.shields.io/badge/supply%20chain-cargo--deny-blue)](deny.toml)
[![Security](https://img.shields.io/badge/security-advisory%20analysis-brightgreen)](SECURITY.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE)

<p align="center">
🖥️ <a href="apps/mq-bridge-app">mq-bridge-app</a> ·
📊 <a href="apps/mq-bridge-app/benches/etl/README.md">ETL benchmarks</a> ·
🏗️ <a href="docs/ARCHITECTURE.md">Architecture</a> ·
⚙️ <a href="docs/CONFIGURATION.md">Config</a> ·
📚 <a href="docs/REFERENCE.md">Middleware &amp; endpoint reference</a>
</p>

## What is mq-bridge?

`mq-bridge` is an asynchronous Rust library that moves messages and data between systems: Kafka, NATS, RabbitMQ, MQTT, MongoDB, PostgreSQL/MySQL/SQLite, Postgres CDC, ClickHouse, Redis Streams, HTTP, WebSocket, gRPC, ZeroMQ, AWS SQS/SNS, IBM MQ, cloud object storage, files, and in-memory channels. Every endpoint sits behind the same batch-shaped `receive_batch` / `send_batch` traits, so any source can feed any sink.

A **route** connects one input to one output. In between, it can transform, filter, fan out, retry, rate-limit, deduplicate, or answer a request, all by configuration. Where configuration isn't enough, you add a handler in Rust, Python or Node.js. Batching, ack/nack, commit ordering and broker I/O stay in the Tokio-based core, so your code works with plain `CanonicalMessage`s and doesn't handle Kafka offsets or AMQP nacks.

It is a **library you embed in your own service**, not a daemon or platform you operate. If you'd rather not write code, [`mq-bridge-app`](apps/mq-bridge-app) runs the same engine and config as a CLI, server, desktop app, and MCP server.

## Use cases

| Problem | Guide with a runnable example |
| :--- | :--- |
| A RAG or semantic-search index drifts from its Postgres source | [Keep a vector index in sync with Postgres: CDC → embeddings → Qdrant](https://marcomq.github.io/mq-bridge/use-cases/sync-postgres-to-qdrant.html) |
| A service must update its database and publish an event atomically | [Transactional outbox with Postgres and Kafka](https://marcomq.github.io/mq-bridge/use-cases/transactional-outbox-postgres-kafka.html) |
| Tests for Kafka, NATS or RabbitMQ consumers need a running broker | [Test message handlers without a broker](https://marcomq.github.io/mq-bridge/use-cases/test-without-a-broker.html) |

## Quick Start

Install the library for your language:

| Language | Package | Install |
| :--- | :--- | :--- |
| Rust | [`mq-bridge`](https://crates.io/crates/mq-bridge) | `cargo add mq-bridge --features kafka,nats,yaml` |
| Python | [`mq-bridge`](python/mq-bridge-py/README.md) ([PyPI](https://pypi.org/project/mq-bridge/)) | `pip install mq-bridge` |
| Node.js | [`mq-bridge`](node/mq-bridge-node/README.md) ([npm](https://www.npmjs.com/package/mq-bridge)) | `npm install mq-bridge` |
| C / C++ | [`mq_bridge.h`](c/mq-bridge-c/README.md) | `cargo build --release -p mq-bridge-c` (build from source) |

Describe a route — here Kafka → NATS with retries — in `routes.yaml`:

```yaml
kafka_to_nats:
  input:
    kafka: { url: "localhost:9092", topic: "orders", group_id: "bridge" }
  output:
    middlewares:
      - retry: { max_attempts: 5 }
    nats: { url: "nats://localhost:4222", subject: "orders.processed" }
```

Run it:

```python
from mq_bridge import Route
Route.from_file("routes.yaml", "kafka_to_nats").run()  # blocks; use start() to keep going
```

```js
import { Route } from "mq-bridge";
Route.fromFile("routes.yaml", "kafka_to_nats").start();
```

```rust
mq_bridge::deploy_file("routes.yaml").await?; // every route in the file, in the background
```

Attach a handler (`with_handler` / `withHandler`) when you need business logic in between. **No code at all?** [`mq-bridge-app`](apps/mq-bridge-app) runs the same engine from the command line or a desktop UI:

```bash
mqb copy 'kafka://localhost:9092?topic=orders' 'nats://localhost:4222?subject=orders.processed'
```


## Capabilities at a glance

Every row works from the Rust crate, the Python and Node.js packages, and the zero-code app; the Cargo feature is only needed when you build the Rust crate yourself. The linked [book](https://marcomq.github.io/mq-bridge/) documents the library as well as the app.

| Capability | How (endpoint / middleware · Cargo feature) | Docs |
| :--- | :--- | :--- |
| **Message brokers**: Kafka, NATS (JetStream), RabbitMQ (AMQP), MQTT, Redis Streams, AWS SQS/SNS, IBM MQ, ZeroMQ | `kafka`, `nats`, `amqp`, `mqtt`, `redis_streams`, `aws`, `ibmmq`, `zeromq` endpoints · features `kafka`, `nats`, `amqp`, `mqtt`, `redis-streams`, `aws`, `ibm-mq`, `zeromq` | [Connectors](https://marcomq.github.io/mq-bridge/connectors/index.html) |
| **Change Data Capture (CDC)**: stream Postgres or MongoDB changes to Kafka, files, a warehouse, … | `postgres_cdc` (logical replication, `pgoutput`) · `postgres-cdc`; `mongodb` with `consume: capture_all` / `capture_new` (change streams) · `mongodb` | [Postgres CDC tutorial](https://marcomq.github.io/mq-bridge/tutorials/postgres-cdc.html) · [MongoDB](https://marcomq.github.io/mq-bridge/connectors/mongodb.html) |
| **SQL databases**: PostgreSQL, MySQL, MariaDB, SQLite as source (cursor polling) or sink (insert / upsert) | `sqlx` endpoint · `sqlx` | [PostgreSQL / MySQL / SQLite](https://marcomq.github.io/mq-bridge/connectors/postgres.html) · [Upserts](https://marcomq.github.io/mq-bridge/cookbook/upserts.html) |
| **MongoDB** as source or sink | `mongodb` endpoint · `mongodb` | [MongoDB](https://marcomq.github.io/mq-bridge/connectors/mongodb.html) |
| **ClickHouse** bulk insert and resumable reads | `clickhouse` endpoint · `clickhouse` | [ClickHouse](https://marcomq.github.io/mq-bridge/connectors/clickhouse.html) |
| **Search engines** and other bulk document APIs (Meilisearch, Typesense, Elasticsearch, Qdrant, PostgREST / Supabase) | `http_bulk` endpoint · `http-bulk` | [HTTP bulk](https://marcomq.github.io/mq-bridge/connectors/http-bulk.html) |
| **Object storage**: S3 (and S3-compatible, e.g. R2), GCS, Azure Blob, or a local directory, as **Parquet**, **JSONL** or **CSV** | `object_store` endpoint with `format: parquet \| raw \| json \| csv` · `object-store`, `parquet` | [Object storage](https://marcomq.github.io/mq-bridge/connectors/object-store.html) |
| **Files**: CSV and JSONL, read and write | `file` endpoint · built in | [File](https://marcomq.github.io/mq-bridge/connectors/file.html) |
| **Analytics / warehouses**: Snowflake, BigQuery, Databricks / Spark, Athena / Trino, DuckDB **via Parquet on S3/GCS/Azure**. These are not native connectors: mq-bridge writes the Parquet files, the warehouse loads or queries them | `object_store` with `format: parquet` · `parquet` | [Snowflake](https://marcomq.github.io/mq-bridge/cookbook/snowflake.html) · [BigQuery](https://marcomq.github.io/mq-bridge/cookbook/bigquery.html) · [Databricks / Spark](https://marcomq.github.io/mq-bridge/cookbook/databricks.html) · [Athena / Trino](https://marcomq.github.io/mq-bridge/cookbook/athena.html) · [DuckDB](https://marcomq.github.io/mq-bridge/cookbook/duckdb.html) |
| **HTTP, gRPC, WebSocket** as server or client, including request/reply | `http`, `grpc`, `websocket` endpoints · `http`, `grpc`, `websocket` | [HTTP](https://marcomq.github.io/mq-bridge/connectors/http.html) · [gRPC](https://marcomq.github.io/mq-bridge/connectors/grpc.html) · [Request / reply](https://marcomq.github.io/mq-bridge/tutorials/request-reply.html) |
| **Schema validation and type coercion** (JSON Schema subset, field mapping) | `transform` middleware · built in | [Transform & schema mapping](https://marcomq.github.io/mq-bridge/cookbook/transform.html) |
| **Reliability**: retries, dead-letter queue, deduplication, rate limiting | `retry`, `dlq`, `deduplication`, `limiter` middleware · `dedup` for deduplication | [Middleware reference](docs/REFERENCE.md) · [Delivery guarantees](docs/DELIVERY.md) |
| **Broker-free tests**: run a route or handler against in-memory channels, no Docker | `memory` endpoint · built in | [Embed the library](https://marcomq.github.io/mq-bridge/tutorials/embedding.html) |
| **ETL without code** | [`mq-bridge-app`](apps/mq-bridge-app): `mqb copy <from> <to>`, YAML routes, desktop UI, MCP server | [Quick start: `mqb copy`](https://marcomq.github.io/mq-bridge/quick-start.html) |
| **More systems through plugins**: Apache Pulsar, Meilisearch, and Redpanda Connect's inputs, outputs and processors | native plugins · `plugin` | [Plugin endpoints](https://marcomq.github.io/mq-bridge/reference/endpoints.html#plugin-endpoints) · [Connect plugin](https://marcomq.github.io/mq-bridge/connectors/connect.html): [inputs](https://marcomq.github.io/mq-bridge/connectors/connect-inputs.html), [outputs](https://marcomq.github.io/mq-bridge/connectors/connect-outputs.html), [processors](https://marcomq.github.io/mq-bridge/connectors/connect-processors.html) |

The Connect plugin links only Redpanda Connect components free of Redpanda Community License code (bundled code is Apache-2.0 and MIT), so some upstream components, such as its Kafka, AWS and Snowflake ones, are not included; see [what is not included](https://marcomq.github.io/mq-bridge/connectors/connect.html#what-is-not-included).

For AI agents: [`llms.txt`](llms.txt) is a curated index of the documentation.

## Benchmarks

Like-for-like ETL comparisons, measured through the zero-code [`mq-bridge-app`](apps/mq-bridge-app); every number, baseline and command is in the [ETL benchmark harness](apps/mq-bridge-app/benches/etl/README.md):

| Scenario | mq-bridge | Compared with |
| :--- | :--- | :--- |
| CSV → JSONL, 1M mixed-type rows (~116 MiB) | **3,134,796 rows/s**, ~29 MiB RAM | Meltano (`tap-csv` → `target-jsonl`): 9,771 rows/s, ~444 MiB RAM — mq-bridge **~321x faster**<br>DuckDB, all cores: 2,109,704 rows/s — mq-bridge **~1.5x faster**, ~18x less memory |
| Kafka → file, 1M rows, no transform | **878,105 rows/s**, ~160 MiB RAM | Sea Streamer, mimalloc build: 492,465 rows/s, ~894 MiB RAM — mq-bridge **~1.8x faster** (also ~1.8x vs. its default-allocator build) |

The CSV row reuses an existing, independently published 1M-row CSV → JSONL workload, on a byte-identical fixture: 7 mixed-type columns, seed 42, 121,981,421 bytes, SHA-256 `a84894e0…0c45b221` ([full definition](apps/mq-bridge-app/benches/etl/README.md#workload-definition)). Whole-process wall-clock, no transformation, Meltano run on the same machine and file.

All figures: mq-bridge 0.4.20. DuckDB is a throughput ceiling for the conversion itself, not an ETL tool.


**External benchmark:** both the Rust library ([`mq-bridge`](https://www.http-arena.com/#sort=rps:-1&q=rust)) and the Python binding ([`mq-bridge-py`](https://www.http-arena.com/#sort=rps:-1&q=python)) are entries on the third-party [http-arena.com](https://www.http-arena.com/) leaderboard, which compares HTTP frameworks by requests per second (live, so rankings shift over time). It measures mq-bridge's HTTP (and WebSocket) serving path, which is one endpoint among many, not broker or ETL throughput; those are covered under [Benchmarks](#benchmarks). See [the Python analysis notes](python/mq-bridge-py/README.md#analysis) for the local comparison harness.


## Why mq-bridge

What you get from one engine, whichever language you call it from:

*   **20+ native transports, 100+ more via plugin, one API**: every connector listed above, plus `dir_spool` (a crash-safe directory FIFO queue), behind the same `receive_batch` / `send_batch` shape.
*   **Redpanda Connect reach**: the [Connect plugin](https://github.com/marcomq/mq-bridge-connect) adds Redpanda Connect's production-proven components — **56 inputs and 68 outputs as endpoints, plus 86 processors**, most of which also run as middleware on any endpoint — while mq-bridge keeps routing, batching, retries, DLQ and deduplication.
*   **Change Data Capture**: stream row-level changes from **Postgres** (logical replication / `pgoutput`) and **MongoDB** (change streams) as flat rows with an operation marker.
*   **Restart-safe delivery**: batch-aware ack/nack with commit sequencing for cumulative-ack brokers; the integration suite shows **no data loss during in-flight broker restarts**, including a Postgres CDC restart-safety test.
*   **Reliability middleware, not a framework**: retries, dead-letter queues, deduplication, rate limiting, and cookie/session persistence wrap any endpoint.
*   **Polyglot on one engine**: the same Rust core ships as native **Python** and **Node.js** bindings; routing, batching, and broker I/O stay in Rust.
*   **TLS everywhere, one config shape**: a single `TlsConfig` block (CA bundle, client cert/key for mTLS, insecure-skip) is reused across transports.
*   **Self-hosted, no daemon**: generate config in the optional UI, paste it into your code, run it in-process. No hosted control plane, no separate scheduler.

**Prefer not to write code?** [`mq-bridge-app`](apps/mq-bridge-app) runs the exact same engine as a **standalone, zero-code ETL service** configured entirely by **YAML or environment variables**. It ships a **Postman-style UI** to build, send, and inspect messages against a route, and can **import Postman collections and AsyncAPI documents** to scaffold routes and endpoints. Everything about the app — install, CLI, UI, cookbook, tuning — is in the [📖 book](https://marcomq.github.io/mq-bridge/).

### When to use mq-bridge
*   **Hybrid Messaging**: Connect systems speaking different protocols (e.g., MQTT to Kafka) without writing a custom adapter for every pair.
*   **Batch-heavy Pipelines**: Increase throughput by moving messages in batches while keeping per-message ack/nack decisions.
*   **Infrastructure Abstraction**: Write business logic against `CanonicalMessage`s and swap the underlying transport later.
*   **Resilient Pipelines**: Apply retry, DLQ, deduplication, limiter, and cookie/session behavior consistently around endpoints.
*   **Database Integration**: Combine databases with message brokers, for example by ingesting messages into SQL/MongoDB or forwarding outbox rows to a broker.
*   **Sidecar / Gateway**: Run the bridge beside another service to ingest, filter, and route messages before they reach the core application.

### When NOT to use mq-bridge
*   **Stateful Stream Processing**: For windowing, joins, or complex aggregations over time, dedicated stream processing engines are more suitable.
*   **Domain Aggregate Management**: If you need a framework to manage the lifecycle, versioning, and replay of domain aggregates (Event Sourcing), use a specialized library. `mq-bridge` handles the *bus*, not the *entity*.
*   **Protocol-Specific Power Features**: `mq-bridge` intentionally exposes a common subset: publish/consume, pub/sub where possible, request-reply where possible, batching, middleware, and ack/nack handling. If your application depends on highly specific broker features, using that broker's native client directly may be better.

## Documentation

The **[📖 mq-bridge book](https://marcomq.github.io/mq-bridge/)** covers the library and the app. Connectors, middleware and
tuning use the same settings in both.

| I want to… | Read |
| :--- | :--- |
| Embed the library, write handlers, publish from code | [Quick start: library](https://marcomq.github.io/mq-bridge/getting-started/library-quick-start.html) · [Embed the library](https://marcomq.github.io/mq-bridge/tutorials/embedding.html) · [Bindings API](https://marcomq.github.io/mq-bridge/reference/bindings.html) · [docs.rs](https://docs.rs/mq-bridge) |
| Move data without code | [Quick start: `mqb copy`](https://marcomq.github.io/mq-bridge/quick-start.html) · [Desktop UI](https://marcomq.github.io/mq-bridge/getting-started/desktop-ui.html) · [CLI](https://marcomq.github.io/mq-bridge/reference/cli.html) · [MCP server](https://marcomq.github.io/mq-bridge/MCP.html) |
| Configure a connector | [Connectors](https://marcomq.github.io/mq-bridge/connectors/index.html), with YAML and URL examples for each |
| Know what each backend supports (subscriber mode, request-reply, nack, CDC vs. polling) | [Overview & capabilities](https://marcomq.github.io/mq-bridge/reference/endpoints.html) |
| Add retries, DLQ, dedup, transform, routing | [Cookbook](https://marcomq.github.io/mq-bridge/cookbook/upserts.html) · [Middleware & structural endpoints](docs/REFERENCE.md) |
| Understand delivery guarantees and idempotent sinks | [Delivery guarantees](docs/DELIVERY.md) |
| Build request/reply or CQRS-style flows | [Request / reply](https://marcomq.github.io/mq-bridge/tutorials/request-reply.html) · [Embed the library: CQRS](https://marcomq.github.io/mq-bridge/tutorials/embedding.html#cqrs-style-flows) |
| Tune throughput | [Performance tuning](https://marcomq.github.io/mq-bridge/operations/tuning.html) |
| Write my own endpoint, middleware or plugin | [EXTENDING.md](docs/EXTENDING.md) · [PLUGINS.md](docs/PLUGINS.md) |
| Understand the internals | [ARCHITECTURE.md](docs/ARCHITECTURE.md) · [CONFIGURATION.md](docs/CONFIGURATION.md) |

## Status

`mq-bridge` was created in 2025. Its reliability behavior is exercised by an automated integration and performance suite across supported endpoints, in each of the queue and subscriber modes that endpoint supports:

*   All endpoints showed **no data loss during in-flight broker restarts**; MQTT publish confirmation was hardened until a chaos test drove in-flight loss to **zero**.
*   Postgres CDC has a **restart-safety test**: an un-acked, in-flight batch is redelivered after a database restart with no loss and no gap.

It is used as rust library in [armature](https://github.com/quinnjr/armature) and as python lib in [omniload](https://github.com/panodata/omniload). 

## Running Tests
The project includes integration and performance tests. Most backend tests require Docker.

To run the performance benchmarks for all supported backends:
```sh
cargo test --test integration_test --release -- --ignored --nocapture --test-threads=1
```

To run the criterion benchmarks:
```sh
cargo bench --features "full"
```
Criterion numbers vary with the machine they run on; for comparable throughput figures use the integration performance test above, or see the [ETL benchmarks](apps/mq-bridge-app/benches/etl/README.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup notes, code style, and pull request guidelines.

## AI Disclaimer

This library was written with a lot of AI assistance. The core started as my own code; many endpoints and docs were expanded with Gemini, CodeRabbit, Claude, and Codex, which made adding transports much faster once the endpoint traits were stable.

Generated code can look plausible while missing important details, so I review every commit manually, clean up and refactor the output, and rely on the integration suite to catch regressions. **I trust the current code as much as if I had written it myself.** Given the large feature set there may still be open issues; the current focus is testing and documentation.

## License
`mq-bridge` is dual-licensed under either the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.

Binary distributions carry their applicable third-party terms in
`THIRD_PARTY_LICENSES.txt`. Their current maintenance process is documented in
[docs/THIRD_PARTY_LICENSES.md](docs/THIRD_PARTY_LICENSES.md).

### Contribution
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `mq-bridge` by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
