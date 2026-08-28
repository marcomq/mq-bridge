# mq-bridge library

[![Crates.io](https://img.shields.io/crates/v/mq-bridge.svg)](https://crates.io/crates/mq-bridge)
[![Docs.rs](https://docs.rs/mq-bridge/badge.svg)](https://docs.rs/mq-bridge)
[![Benchmark](https://github.com/marcomq/mq-bridge/actions/workflows/benchmark.yml/badge.svg)](https://marcomq.github.io/mq-bridge/dev/bench/)
![Linux](https://img.shields.io/badge/Linux-supported-green?logo=linux)
![Windows](https://img.shields.io/badge/Windows-supported-green?logo=windows)
![macOS](https://img.shields.io/badge/macOS-supported-green?logo=apple)
[![Supply chain](https://img.shields.io/badge/supply%20chain-cargo--deny-blue)](deny.toml)
[![Security](https://img.shields.io/badge/security-advisory%20analysis-brightgreen)](SECURITY.md)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)


```text
      ┌────── mq-bridge-lib ──────┐
──────┴───────────────────────────┴──────
            crossing streams
```

`mq-bridge` is an asynchronous message library for Rust. It connects message brokers, databases, files, HTTP/WebSocket endpoints, and in-memory channels behind one small set of traits.

It is not only a forwarder. A route can transform, filter, fan out, retry, rate-limit, deduplicate, or turn a request into a response before the message reaches the next system. The core is built on Tokio and keeps the transport details at the edge, so application code can mostly work with `CanonicalMessage`s and handlers.

## Move data from A to B — inside your own service

If you need to move data or events reliably between systems and you write code (Rust, Python, or Node), `mq-bridge` is a strong default. It is a **library you embed**, not a daemon or control plane you operate.

**Prefer not to write code?** [`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) runs the exact same engine as a **standalone, zero-code ETL service** configured entirely by **YAML or environment variables** — move data from A to B without writing a line. It ships a **Postman-style UI** to build, send, and inspect messages against a route, and can **import Postman collections and AsyncAPI documents** to scaffold routes and endpoints for you.

*   **16+ transports, one API**: Kafka, NATS, AMQP (RabbitMQ), MQTT, MongoDB, **Postgres CDC** (logical replication), PostgreSQL / MySQL / SQLite (SQLx), ClickHouse, HTTP, WebSocket, gRPC, ZeroMQ, Redis Streams, AWS SQS/SNS, cloud object storage (S3 / GCS / Azure), IBM MQ, files, and in-memory channels — all behind the same `receive_batch` / `send_batch` shape.
*   **Change Data Capture**: stream row-level changes from **Postgres** (logical replication / `pgoutput`) and **MongoDB** (change streams) as flat rows with an operation marker.
*   **Restart-safe delivery**: batch-aware ack/nack with commit sequencing for cumulative-ack brokers; the integration suite shows **no data loss during in-flight broker restarts**, including a Postgres CDC restart-safety test.
*   **Reliability middleware, not a framework**: retries, dead-letter queues, deduplication, rate limiting, and cookie/session persistence wrap any endpoint.
*   **Polyglot on one engine**: the same Rust core ships as native **Python** and **Node.js** bindings; routing, batching, and broker I/O stay in Rust.
*   **TLS everywhere, one config shape**: a single `TlsConfig` block (CA bundle, client cert/key for mTLS, insecure-skip) is reused across transports.
*   **Self-hosted, no daemon**: generate config in the optional UI, paste it into your code, run it in-process. No hosted control plane, no separate scheduler.

> **Throughput & footprint.** In our own benchmarks, the same engine — driven the zero-code way through [`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) — on a CSV→JSONL file conversion (1,000,000 mixed-type rows, ~116 MiB) sustained **1,133,786 rows/s** at ~22 MiB, about **~58x faster** and **~20x leaner in memory** than Meltano (`tap-csv` → `target-jsonl`, ~19,500 rows/s / ~444 MiB). Full setup, methodology, and the exact parameters are in [`benches/ETL_BENCHMARKS.md`](benches/ETL_BENCHMARKS.md).

> **Kafka → file.** In a 1,000,000-row relay with the default file format and no transform, the engine was up to **65% faster than Sea Streamer**. The [`mq-bridge-app` benchmark](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app/benches/etl) contains the reproducible helper and native file-format caveats.


## Language Bindings

`mq-bridge` is a Rust library, but the same engine ships as native bindings for **Python** and **Node.js**. The Tokio runtime, broker I/O, routing, and batching all stay in Rust; the binding is a thin layer for handlers and configuration.

| Language | Package | Install |
| :--- | :--- | :--- |
| Rust | [`mq-bridge`](https://crates.io/crates/mq-bridge) | `cargo add mq-bridge` |
| Python | [`mq-bridge-py`](python/mq-bridge-py/README.md) ([PyPI](https://pypi.org/project/mq-bridge-py/)) | `pip install mq-bridge-py` |
| Node.js | [`mq-bridge`](node/mq-bridge-node/README.md) ([npm](https://www.npmjs.com/package/mq-bridge)) | `npm install mq-bridge` |

The constructor names are kept aligned across languages, so a config loader reads the same in either binding (Python uses `snake_case`, Node uses `camelCase`):

- `Route.from_file` / `Route.fromFile` — load a route from a YAML/JSON file
- `Route.from_str` / `Route.fromStr` — load from an in-memory YAML/JSON string
- `Route.from_config` / `Route.fromConfig` — load from a dict / JS object
- the matching `Publisher.*` constructors build a publisher endpoint

The `name` argument is optional in both: pass it to select one entry from a `routes:`/`publishers:` document, or omit it to treat the config as a single bare route/endpoint body.

The Python binding also holds up well under load on the third-party [http-arena.com](https://www.http-arena.com/#sort=rps:-1&q=python) requests-per-second HTTP benchmark (live leaderboard — rankings shift over time). See [the Python analysis notes](python/mq-bridge-py/README.md#analysis) for the local HTTP comparison harness.


## Architecture

See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for a detailed overview of the internal design, extensibility, and usage patterns.

**Usage Types:**

1. **Event Handler (TypedHandler):** Communicate between applications using strongly-typed message handlers, optionally with response support.
2. **Compute Handler:** Generally receive and process messages with a custom handler
3. **Direct Endpoint Usage:** Use `publish` / `publish_batch` and `receive` / `receive_batch` directly on endpoints. This mode requires manual commit, batch sequencing, and concurrency handling.

For implementation details and quick start examples for each usage type, see the [Architecture Guide](docs/ARCHITECTURE.md).

## Features

*   **Supported Backends**: Kafka, NATS, AMQP (RabbitMQ), MQTT, MongoDB, SQL Databases (PostgreSQL, MySQL, SQLite via sqlx), **Postgres CDC** (logical replication), ClickHouse, gRPC, HTTP, WebSocket, ZeroMQ, Redis Streams, Files, cloud object storage (S3 / GCS / Azure), AWS (SQS/SNS), IBM MQ, and in-memory channels.
*   **Change Data Capture**: PostgreSQL logical replication (`postgres_cdc`) and MongoDB change streams, surfaced as flat rows with an `*.operation` marker (`insert`/`update`/`delete`/`truncate`).
    > **Note**: IBM MQ is included in the `full` feature set via the `ibm-mq` feature, which loads the IBM MQ client library at runtime via dlopen — **no IBM SDK is needed to build**. The IBM MQ redistributable client only has to be present at runtime, and only if you actually use an IBM MQ endpoint (it is loaded lazily on first connect). If the client is missing, the affected route fails fast with a non-retryable error instead of reconnecting forever. The loader finds the client via the platform default name, `MQ_INSTALLATION_PATH` (e.g. `/opt/mqm`), or an explicit `MQB_IBM_MQ_LIB` path. To link the client statically at build time instead, use the `ibm-mq-static` feature (requires the IBM MQ SDK). See the [mqi crate](https://crates.io/crates/mqi/) for details.
    >
    > **TLS**: IBM MQ has its own TLS config shape (`IbmTlsConfig`), since the native client doesn't consume PEM files. The field names mirror the generic `TlsConfig` for config parity, but carry MQ-native semantics: `tls.cert_file` (alias `key_repository`) is a CMS key repository path (e.g. `/path/to/tls` for `tls.kdb`), not a PEM file. The repository can either be passwordless (backed by a `.sth` stash file next to the `.kdb`) or password-protected via `tls.cert_password` (alias `key_repository_password`; requires an IBM MQ client/server at 9.3.0.0 or later, which is the capability level `ibm-mq`/`ibm-mq-static` build against; 9.2 is EOL).
*   **Configuration**: Routes can be defined via YAML, JSON or environment variables.
*   **Programmable Logic**: Inject custom Rust handlers to transform or filter messages in-flight.
*   **Batching**: Every endpoint uses the same `send_batch` / `receive_batch` shape. Routes default to batches of up to 512 messages, configurable with `batch_size`.
*   **Middleware**:
    *   **Retries**: Exponential backoff for transient failures.
    *   **Dead-Letter Queues (DLQ)**: Redirect failed messages.
    *   **Deduplication**: Message deduplication using `sled`.
    *   **Limiter**: Best-effort throughput limiting in messages per second, including batch-aware pacing.
    *   **Cookie Jar**: Persist and re-inject HTTP-style cookies or selected metadata across requests.
*   **Concurrency**: Configurable concurrency per route using Tokio.

## Philosophy & Focus

The project has one main bias: move data reliably without forcing the rest of the application to care too much about the transport.

That means `mq-bridge` tries to keep the boring parts boring. Kafka offsets, RabbitMQ nacks, HTTP responses, MongoDB polling, WebSocket frames, and file rows are all different in real life, but route code should still be able to receive a batch, process it, publish it, and commit it.

Batching is a big part of that design. Every endpoint is optimized around batch-shaped APIs, even when the backend itself only has a single-message primitive. Routes default to opportunistic batches of up to 512 messages: the consumer waits for the first message, then takes whatever else is already available without adding idle latency. Lower `batch_size` when tighter failure isolation or a smaller per-batch memory footprint matters.

The error handling follows the same idea. Batch publishing can report partial success, retryable failures, and non-retryable failures. Route commits are sequenced for cumulative-ack brokers so later batches cannot acknowledge earlier unresolved messages; transports with independent acknowledgements can commit batches concurrently. In other words: batching is not just a performance trick bolted onto the side; ack/nack behavior and retry/DLQ handling were built to work with it.

What it does not try to be: a domain framework, an actor runtime, or a full stream processor. You can build CQRS-ish flows with it, but the library cares more about transport, routing, and delivery behavior than about prescribing your domain model.

### How it compares

`mq-bridge` overlaps with config-driven ETL / pipeline runners, but comes in two shapes: a **library you embed in your own service**, or [`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) — the **same engine as a zero-code, config-driven ETL service**. You get the developer-first path and the no-code path from one codebase.

*   **Choose the `mq-bridge` library** when you write the code that moves the data — in Rust, Python, or Node — and want batching, retries/DLQ/dedup, request-reply, CDC, and TLS behind one small API you run in-process. No separate scheduler, no daemon to operate.
*   **Choose `mq-bridge-app` for zero-code ETL** when you'd rather move data A→B purely by config: define routes and endpoints in YAML/env, then use its **Postman-style UI** to send test messages and watch them flow — and **import Postman collections or AsyncAPI specs** through the UI to generate the config. Same reliability engine, no build step.
*   **Choose a dedicated stream processor** when you need windowed joins or aggregations over time.

Either way it stays transport-first on purpose: it moves and routes data reliably rather than prescribing a domain model or a platform.

## Status

`mq-bridge` is young (created in 2025) but its reliability behavior is exercised by an automated integration + performance suite across **every** supported endpoint:

*   Integration and performance tests cover all endpoints in both queue and subscriber modes, request-reply where supported, and protocol-specific behavior.
*   All endpoints have automated integration tests that showed **no data loss during in-flight broker restarts**; MQTT publish-confirmation was hardened until a chaos test drove in-flight loss to **zero**.
*   Postgres CDC ships with a **restart-safety integration test**: an un-acked, in-flight batch is redelivered after a database restart with no loss and no gap into already-acknowledged changes.
*   Throughput is tracked continuously on a public [benchmark dashboard](https://marcomq.github.io/mq-bridge/dev/bench/); ETL/CDC-shaped, like-for-like scenarios and their fixed methodology are documented in [`benches/ETL_BENCHMARKS.md`](benches/ETL_BENCHMARKS.md).

Known rough edges (be deliberate here):

- old or very new broker-server versions, or unusual broker settings
- subscribe/event and response patterns where the backend has no native equivalent (emulated)
- NATS without JetStream
- **TLS**: one unified `TlsConfig` shape is reused across transports and exercised by automated tests; per-backend certificate matrices (self-signed / CA-signed / mTLS / expired-cert rejection) are being expanded — treat non-trivial TLS setups as needing your own verification for now.


## Test Notes

- **NATS**: Automated tests are only run with JetStream enabled. Other NATS modes are not covered by integration tests.
- **MongoDB**: The reply pattern was only tested in an automated test and is not yet used in projects; because it uses emulation that wait for messages, it may cause severe issues if timeouts are not configured correctly.
- **Performance Tests**: These are generally executed in non-subscriber (queue) mode for all endpoints.
- **Request-Reply**: Only tested for endpoints that natively support or emulate it (see backend table below for details). Endpoints like SQLx, Files, AWS, IBM MQ, and Sled do not support request-reply and are not tested for this pattern.
- **Subscriber Mode**: You may also completely emulate a subscriber mode, if the subscribers are static, by performing a fanout and manually create an endpoint for each target.


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

## Core Concepts

*   **Route**: A named data pipeline that defines a flow from one `input` to one `output`.
*   **Endpoint**: A source or sink for messages.
*   **Middleware**: Components that intercept and process messages (e.g., for error handling).
*   **Handler**: A programmatic component for business logic, such as transforming/consuming messages (`CommandHandler`) or subscribe them (`EventHandler`).

> **[REFERENCE.md](docs/REFERENCE.md)** lists every middleware and every structural endpoint
> (`ref`, `fanout`, `switch`, `request`, `response`, `reader`, `static`, `stream_buffer`,
> `null`, `custom`) with fields, defaults and working examples.
>
> To add an endpoint of your own, see **[EXTENDING.md](docs/EXTENDING.md)** — or
> **[PLUGINS.md](docs/PLUGINS.md)** to ship a Rust endpoint or middleware as a native
> library that Rust, Python and Node.js hosts all load at runtime, without compiling it
> into mq-bridge.

## Backend Features & Configuration

`mq-bridge` endpoints generally default to a **Consumer** pattern (Queue), where messages are persisted and distributed among workers. To achieve **Subscriber** (Pub/Sub) behavior, specific configuration is required.

The table below summarizes the capabilities and configuration for each backend:

| Backend | Subscriber Config (Pub/Sub) | Request-Reply | Nack Support |
| :--- | :--- | :--- | :--- |
| **AMQP** | Set `subscribe_mode: true` | Emulated (Property) | **Yes** (Basic.nack) |
| **AWS** | N/A (Use SNS) | No | **Yes** (Visibility Timeout) |
| **File** | Set `mode: subscribe` | No | Simulated (In-Memory) |
| **gRPC** | N/A | No | **Yes** for the built-in Bridge protocol (NACK replays); **No** for dynamic services unless their API defines an acknowledgement contract |
| **HTTP** | N/A | **Native** (Implicit) | **Yes** (HTTP 500) |
| **IBM MQ** | Set `topic` | No | **Yes** (Tx Rollback) |
| **Kafka** | Omit `group_id` | Emulated (Header) | Eventual (Skip Offset) |
| **Memory** (in-process) | Set `subscribe_mode: true` | Emulated (Metadata) | **Yes** (Re-queue), by default **disabled** |
| **Memory** (IPC: `ipc://`, `unix://`, `pipe://`) | Not supported | Not supported | **Yes** (Re-queue), by default **enabled**, consumer-local |
| **MongoDB** | Set `consume: subscriber` | Emulated (Metadata) | **Yes** (Unlock) |
| **MQTT** | Set `clean_session: true` | Emulated (Property) | Eventual (Skip Ack) |
| **NATS** | Set `subscriber_mode: true` | **Native** (Inbox) | **Yes** (JetStream Nak) |
| **Postgres CDC** | N/A (streams committed changes) | No | **Yes** (confirmed LSN not advanced) |
| **Redis Streams** | Set `subscriber_mode: true` | No | Eventual (PEL, un-acked) |
| **Sled** | Set `delete_after_read: false` | No | **Yes** (Tx Rollback) |
| **SQLx** | Not supported | No | Eventual (Skip Delete) |
| **WebSocket** | N/A | No | No |
| **ZeroMQ** | Set `socket_type: "sub"` | **Native** (REQ/REP) | No |

### Feature Details
*   **Request-Reply**:
    *   **Native**: Uses protocol-level correlation (e.g., HTTP connection, NATS reply subject).
    *   **Emulated**: Publishes a new message to a reply destination (specified by the `reply_to` metadata field) carrying a `correlation_id` metadata field.
*   **Nack Support**: If "Yes", the backend supports explicit negative acknowledgement triggering redelivery. "Eventual" means redelivery depends on timeout or connection drop. "Simulated" is handled in-memory by the bridge. "Consumer-local" means the nacked message is retried inside the consumer process and is lost if that process dies — the producer is never notified.

### Database Sources: Change Capture vs Polling
Databases have no native pub/sub, so `mq-bridge` reads them as a source in one of two ways:

*   **Change Data Capture (CDC)** — tails the database's own change log, capturing inserts **and** updates/deletes and resuming from a durable log position.
    *   On **PostgreSQL**, the `postgres_cdc` endpoint streams a logical-replication slot (`pgoutput`). It emits flat JSON rows tagged with `postgres.operation` metadata (`insert`/`update`/`delete`/`truncate`). Acking a batch confirms the LSN back to the server (`standby_status_update`); a nack (or an interrupted run) does **not** advance the confirmed LSN, so replication resumes from the last acknowledged position on reconnect — at-least-once, verified by a restart-safety integration test. Enable with the `postgres-cdc` feature; requires `wal_level = logical` and a publication. See below.
    *   On **MongoDB**, set `consume: capture_new` to watch an existing collection for changes from now on, or `consume: capture_all` to read the existing documents first and then keep capturing changes (no gap, at-least-once). It emits `insert`/`update`/`replace`/`delete` events tagged with `mongodb.operation` metadata and checkpoints progress under `cursor_id`. Both use the change stream (full post-image via `updateLookup`) and therefore need a replica set — a single-node one is enough. Without one they refuse to start; use `consume: snapshot` for a one-shot non-destructive read, or `consume: consumer` for a work queue.
*   **Cursor polling** — pages an existing table by a monotonic `cursor_column` (`WHERE col > $last ORDER BY col ASC`), persisting the last read value under `cursor_id`. Captures **appends only** — updates and deletes are not observed. Available on **SQLx** (PostgreSQL / MySQL / MariaDB / SQLite) and **ClickHouse**; while idle the poll interval backs off exponentially between `polling_interval_ms` and `max_polling_interval_ms`. **SQLite** and **ClickHouse** are polling-only — they have no server-side change log.

> **SQLx read mode is chosen by config, not by driver.** With **no** `cursor_column`, an SQLx source (`?table=` / `table:`) is a **competing-consumers work queue**: it atomically claims rows via a `locked_until` lease and deletes them on ack, so several readers drain the same table as competing consumers — each row goes to one reader at a time, at-least-once (a lease that expires before ack, e.g. after a crash, redelivers the row), not exactly once. This requires the table to carry the queue schema — an `id`, a `payload`, and a `locked_until` column — on **every** driver (PostgreSQL, MySQL/MariaDB, and SQLite behave identically here); it is **not** a plain full-table read. To read an existing or arbitrary table non-destructively (the ETL path), set **`cursor_column`** to switch to the cursor polling above. A source table missing `locked_until` fails fast with a permanent error rather than reconnecting forever.

#### Choosing a MongoDB consume mode

These are not interchangeable. The default `capture_all` is a **reader**, like the other `capture_*`
modes: they fan out, so every reader sees every document, and nothing is claimed or removed. Opting
into `consumer` instead gives a **competing-consumers work queue**: its lock-and-delete protocol lets
several readers drain the same collection with each document going to exactly one of them — the right
thing for dispatching jobs or commands, and destructive by design.

Pick by semantics first. Where both would do — a one-shot bulk read or ETL pass with a single
reader — **use `capture_all`**, the default: `consumer`'s four round trips per batch (find ids →
claim → re-fetch → delete on ack) buy exclusivity you aren't using. Set
`consume: consumer` explicitly when you want the destructive work-queue semantics.

| `consume` | mechanism | modifies source | ends on drain | use for |
| :--- | :--- | :--- | :--- | :--- |
| `capture_all` (default) | snapshot, then change stream | no | with drain options | bulk read / ETL |
| `consumer` | claim → lock → re-fetch → delete | **yes** | yes | work queues, competing readers |
| `snapshot` | one `_id`-ordered pass over the collection | no | **always** | reads without a replica set |
| `capture_new` | change stream, new changes only | no | **no** | ongoing CDC |

500k documents, `batch_size: 1024`, release build, standalone MongoDB: `consumer` → `null`
**23,667 rows/s** (`postgres` → `null` reference: 115,924). The previously quoted `capture_all`
figure was measured on that same standalone server, i.e. through the `_id` fallback that has since
been removed as unsound — it is not comparable and needs re-measuring on a replica set.

*   `concurrency` does not speed up a MongoDB source — batches are fetched serially; it only widens
    the downstream side.
*   `consumer` only reads collections written by the mq-bridge MongoDB publisher (UUID `_id` /
    wrapped envelope); other documents are skipped or rejected at startup. `snapshot` and the
    `capture_*` modes read arbitrary collections.
*   `capture_*` need a replica set — a single-node one is enough — and both error without it. With
    `exit_on_empty` / `--drain`, a replica-set reader finishes after the snapshot and any
    immediately following changes, once the drain idle timeout expires.
*   `snapshot` is one-shot by design: it delivers what exists when the run starts and then ends the
    route. It rejects `cursor_id`, because resuming above a stored `_id` would skip anything a
    concurrent writer commits below that mark — see the note below.
*   Wart: `capture_all`'s snapshot also emits the internal `<collection>:sequencer` document.

> **Why `snapshot` does not resume.** A `_id` cursor can only match documents *above* its
> high-water mark, and `_id` is assigned client-side before the insert, so it does not follow
> commit order. Any writer that commits below the mark — concurrent producers, a route with
> `concurrency > 1`, a retried write — is skipped permanently and silently. Incremental reads
> therefore need commit order, which on MongoDB means the oplog, which means a replica set.
> There is no sound `_id`-based substitute.


### Deduplication & idempotent writes

With a durable source and durable checkpoint configuration, `mq-bridge` is **at-least-once** across
crashes: a replay can redeliver, while in-process Memory endpoint state is not crash-durable. Pair a
stable replay identity with an **idempotent sink operation** and that covered sink effect is
effectively exactly-once, however often the message is delivered. Which sink absorbs a duplicate, which source gives you a stable key to
deduplicate on, and what a handler in the route changes about all of this, is covered in full by:

> **[docs/DELIVERY.md](docs/DELIVERY.md) — delivery guarantees.** Per-source identity and
> per-sink idempotency tables, the `deduplication` middleware, Mongo `id_field` and `report_outcome`,
> SQL `ON CONFLICT`, ClickHouse `ReplacingMergeTree`, file/object-store `name_by: source_position`, what
> handlers change, and what `mq-bridge` deliberately does not provide.

The short form: point the sink at a deterministic business key (`id_field` for MongoDB, an
`ON CONFLICT` column for SQL) and let its unique constraint be the authority — it is already shared
across every writer, so no extra state store is needed. Add the `deduplication` middleware on the
**input** only when the sink has no constraint to lean on, or to keep duplicates away from a handler.

### Cloud Object Storage (S3 / GCS / Azure)
The `object_store` endpoint (alias `s3`) reads and writes cloud object stores — Amazon S3, Google Cloud Storage, Azure Blob, Cloudflare R2, and anything else the [`object_store`](https://crates.io/crates/object_store) crate speaks — behind the same `receive_batch` / `send_batch` API. Enable it with the `object-store` feature. Credentials and backend options are read from the process environment (`AWS_ACCESS_KEY_ID`, `AWS_ENDPOINT`, `AWS_REGION`, `GOOGLE_SERVICE_ACCOUNT`, `AZURE_STORAGE_ACCOUNT`, ...); the URL scheme picks the backend (`s3://`, `gs://`, `az://`).

*   **As a sink**, batches are encoded with the same file endpoint formats (`normal` JSONL, `json`, `text`, `raw`) and written as whole immutable objects — write-once, nothing appended or mutated. Under `name_by: write_time` each flushed batch becomes **one object** at `<prefix>/[YYYY/MM/DD/]<uuidv7>.<ext>`; the uuidv7 name already sorts by write time, and the optional `date_partition` prefix (on by default, derived from that same id's timestamp) is a readability / lifecycle-rule convenience. Under `name_by: source_position` a batch becomes **one object per contiguous run of source positions** it covers — usually one, more when the batch spans partitions or skips offsets a replay already wrote — each named for the range it covers and written flat under the prefix, where `date_partition` does not apply. The default `name_by: auto` picks `source_position` whenever the route's input carries a replay position — Kafka, Postgres CDC, a SQL cursor, a MongoDB change stream or a file — so key order equals source order at any `concurrency` without configuring anything (see [Files & object storage — `name_by`](docs/DELIVERY.md#files--object-storage--name_by)).
*   **As a source**, objects under the prefix are listed in key order, fetched whole, split on the delimiter, and emitted. Progress is a durable cursor holding the last fully-acked object key: set `cursor_id` and an external `checkpoint_store` (`file://`, `s3://`, `postgres://`, `mongodb://`) so a restart resumes without re-emitting. Objects are **never deleted or rewritten** — resume is non-destructive and at-least-once at object granularity (a nacked batch is redelivered; the cursor only advances once an object is fully acked). `csv` is supported on the source only.

```yaml
archive_to_s3:
  input:
    memory: { topic: "events" }
  output:
    object_store:
      url: "s3://my-bucket/events"
      format: normal            # JSONL; also json / text / raw

replay_from_s3:
  input:
    object_store:
      url: "s3://my-bucket/events"
      cursor_id: "replayer-1"
      checkpoint_store: "file:///var/lib/mqb/s3-cursor.json"
  output:
    nats: { subject: "events.replay", url: "nats://localhost:4222" }
```

> Point `checkpoint_store` at a **different** bucket or prefix than the source reads; a cursor object written under the source prefix would be listed and re-read as data (the source rejects an overlapping object-store checkpoint location).

> The sink's `format` decides the encoding, not where the message came from: `normal`/`json`/`text`
> always write the `{message_id, payload, metadata}` wrapper, so the message id survives the round
> trip. Use `format: raw` to write payloads verbatim (bare documents, no wrapper). Applies to both
> `file` and `object_store`.
>
> A **source** on those formats expects that same wrapper, `message_id` included. A line that is
> valid JSON but not the wrapper — a hand-written fixture with only `payload` and `metadata`, say —
> is not decomposed: the whole line becomes the payload and the line's own `metadata` is discarded.
> The reader logs a warning naming the wrapper when this happens. For plain JSON lines, use
> `format: raw`.
>
> **Payload encoding in the wrapper.** A UTF-8 payload is written as a plain JSON string under
> `payload`. A binary one (compressed, encrypted, Protobuf, …) is base64-encoded under a separate
> `payload_base64` field; the two are mutually exclusive, as in the
> [CloudEvents JSON format](https://github.com/cloudevents/spec/blob/main/cloudevents/formats/json-format.md).
>
> ```json
> {"message_id":"019f9b12-d786-7ebe-a7ec-a1aa71bc47ae","payload":"{\"order_id\":7}"}
> {"message_id":"019f9b12-d78a-7c01-b0f4-2f0f4d6a1c33","payload_base64":"KLUv/SBOAQAA"}
> ```
>
> Sources still read the older byte-array form (`"payload":[123,34,…]`), so existing files keep
> working. Only the reverse is a break: a **binary** record written by this version is not readable
> by an older mq-bridge. Text records are compatible in both directions.

### Response Endpoint
The `response` output endpoint sends a reply back to the original requester. This is useful for synchronous request-reply flows, for example HTTP-to-NATS-to-HTTP. Use `response: {}` as the output endpoint configuration.

*   **Caveats**:
    *   If the input does not support responses (e.g., File, SQLx), the message sent to `response` will be dropped.
    *   Ensure timeouts are configured correctly on the requester side, as the bridge processing time adds latency.
    *   Middleware that drops metadata (like `correlation_id`) may break the response chain.

## Usage

The [`mq-bridge-app`](apps/mq-bridge-app) workspace packages run mq-bridge as a
standalone CLI, desktop app, or Docker container configured via YAML or
environment variables. Plain `cargo build` still builds only this library;
use `cargo build --workspace` to build every workspace package.

### Configuration-first workflow

`mq-bridge-app` can be used to create and test route and endpoint configurations through a UI. The generated JSON/YAML can then be copied into an application and loaded by the Rust library or the Python bindings.

This does not replace application code or handlers, but it is useful when you want a known-good connection and route shape before pasting the configuration into code. For Python and Node projects, routes and publishers are commonly loaded from JSON/YAML with `Route.from_file`/`fromFile` (a file path) and `Route.from_str`/`fromStr` (a JSON/YAML string), while `Route.from_config`/`fromConfig` takes an already-parsed dict/JS object. The matching `Publisher.*` constructors work the same way.

### Programmatic Handlers

For business logic, `mq-bridge` provides a handler layer separate from transport-level middleware. This is where message-specific code usually belongs.

#### Raw Handlers

*   **`CommandHandler`**: A handler for 1-to-1 or 1-to-0 message transformations. It takes a message and can optionally return a new message to be passed down the publisher chain.
*   **`EventHandler`**: A terminal handler that reads new messages without removing them for other event handlers.

You can chain these handlers with endpoint publishers.

```rust
use mq_bridge::traits::Handler;
use mq_bridge::{CanonicalMessage, Handled};
use std::sync::Arc;

// Define a handler that transforms the message payload
let command_handler = |mut msg: CanonicalMessage| async move {
    let new_payload = format!("handled_{}", String::from_utf8_lossy(&msg.payload));
    msg.payload = new_payload.into();
    Ok(Handled::Publish(msg))
};

// Attach the handler to a route
// let route = Route { ... }.with_handler(command_handler);
```

#### Typed Handlers

For more structured, type-safe message handling, `mq-bridge` provides `TypeHandler`. It deserializes messages into a specific Rust type before passing them to a handler function, so handlers do not need to repeat the same parsing code.

Message selection is based on the `kind` metadata field in the `CanonicalMessage`.

```rust
use mq_bridge::type_handler::TypeHandler;
use mq_bridge::{CanonicalMessage, Handled};
use serde::Deserialize;
use std::sync::Arc;

// 1. Define your message structures
#[derive(Deserialize)]
struct CreateUser {
    id: u32,
    username: String,
}

#[derive(Deserialize)]
struct DeleteUser {
    id: u32,
}

// 2. Create a TypeHandler and register your typed handlers
let typed_handler = TypeHandler::new()
    .add("create_user", |cmd: CreateUser| async move {
        println!("Handling create_user: {}, {}", cmd.id, cmd.username);
        // Logic here...
        // Automatically maps () to Handled::Ack
    })
    .add("delete_user", |cmd: DeleteUser| async move {
        println!("Handling delete_user: {}", cmd.id);
        // Logic here...
        // Automatically maps () to Handled::Ack
    });

// 3. Attach the handler to a route
let route = Route::new(input, output).with_handler(typed_handler);

// 4. To send a message to the route's input, create a publisher for that endpoint.
//    In a real application, you would create this publisher once and reuse it.
let input_publisher = Publisher::new(route.input.clone()).await.unwrap();

// 5. Create a typed command, serialize it, and send it via the publisher.
let command = CreateUser { id: 1, username: "test".to_string() };
let message = msg!(&command, "create_user"); // This sets the `kind` metadata field.
input_publisher.send(message).await.expect("Failed to send message");

// The running route will receive the message, see the `kind: "create_user"` metadata,
// deserialize the payload into a `CreateUser` struct, and pass it to your registered handler.
```

### Programmatic Usage

You can define and run routes directly in Rust code.

```rust
use mq_bridge::{models::Endpoint, stop_route, CanonicalMessage, Handled, Route};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[tokio::main]
async fn main() {
    // Define a route from one in-memory channel to another

    // 1. Create a boolean that is changed in the handler
    let success = Arc::new(AtomicBool::new(false));
    let success_clone = success.clone();

    // 2. Define the Handler
    let handler = move |mut msg: CanonicalMessage| {
        success_clone.store(true, Ordering::SeqCst);
        msg.set_payload_str(format!("modified {}", msg.get_payload_str()));
        async move { Ok(Handled::Publish(msg)) }
    };
    // 3. Define Route
    let input = Endpoint::new_memory("route_in", 200);
    let output = Endpoint::new_memory("route_out", 200);
    let route = Route::new(input, output).with_handler(handler);

    // 4. Run (deploys the route in the background)
    route.deploy("test_route").await.unwrap();

    // 5. Inject Data
    let input_channel = route.input.channel().unwrap();
    input_channel
        .send_message("hello".into())
        .await
        .unwrap();

    // 6. Verify
    let mut verifier = route.connect_to_output("verifier").await.unwrap();
    let received = verifier.receive().await.unwrap();
    assert_eq!(received.message.get_payload_str(), "modified hello");
    assert!(success.load(Ordering::SeqCst));

    stop_route("test_route").await;
}
```

## Patterns: Request-Response

`mq-bridge` supports request-response patterns for interactive services such as web APIs. A client can send a request and wait for the matching response, while the bridge keeps the correlation details away from the handler.

The `response` output is the most direct option and the safest one under concurrency.

### The `response` Output Endpoint (Recommended)

For request-response routes, use the dedicated `response` endpoint in the route's `output`.

**How it works:**
1. An input endpoint that supports request-response (like `http`) receives a request.
2. The message is passed through the route's processing chain. This is where you typically attach a `handler` to process the request and generate a response payload.
3. The final message is sent to the `output`.
4. If the output is `response: {}`, the bridge sends the message back to the original input source, which then sends it as the reply (e.g., as an HTTP response).

The response stays in the same execution context as the request, so concurrent requests do not need to share a reply queue and race on correlation IDs.

#### Example: MongoDB Request-Response

For example, a service can write a request document to MongoDB and wait for a reply. The bridge reads the document, runs the handler, and writes the result back to the reply collection.

**YAML Configuration (`mq-bridge.yaml`):**
```yaml
mongo_responder:
  input:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "requests"
  output:
    # The 'response' endpoint sends the processed message back to the 'requests_replies' collection
    # (or whatever reply_to was set to by the sender).
    response: {}
```

**Programmatic Handler Attachment (in Rust):**
You would then load this configuration and attach a handler to the route's output endpoint in your Rust code.

```rust
use mq_bridge::models::{Config, Handled};
use mq_bridge::CanonicalMessage;

async fn run() {
    // 1. Load configuration from YAML
    // let config: Config = serde_yaml_ng::from_str(include_str!("mq-bridge.yaml")).unwrap();
    // let mut route = config.get("api_gateway").unwrap().clone();

    // 2. Define the handler that processes the request
    let handler = |mut msg: CanonicalMessage| async move {
        // Example: echo the request body with a prefix
        let request_body = String::from_utf8_lossy(&msg.payload);
        let response_body = format!("Handled response for: {}", request_body);
        msg.payload = response_body.into();
        Ok(Handled::Publish(msg))
    };

    // 3. Attach the handler to the output endpoint
    // route.output.handler = Some(std::sync::Arc::new(handler));

    // 4. Run the route
    // route.deploy("api_gateway").await.unwrap();
}
```

## Patterns: CQRS
mq-bridge can be used for CQRS-style flows. With routes and typed handlers, it can act as a command bus and an event bus without becoming a domain framework.
* Command Bus: An input source (e.g., HTTP) receives a command. A TypeHandler processes it (Write Model) and optionally emits an event.
* Event Bus: The emitted event is published to a broker (e.g., Kafka). Downstream routes subscribe to these events to update Read Models (Projections).

```rust
// 1. Command Handler (Write Side)
let command_bus = TypeHandler::new()
    .add("submit_order", |cmd: SubmitOrder| async move {
        // Execute business logic, save to DB...
        // Emit event
        let evt = OrderSubmitted { id: cmd.id };
        Ok(Handled::Publish(
            msg!(evt, "order_submitted")
        ))
});

// 2. Event Handler (Read Side / Projection)
let projection_handler = TypeHandler::new()
    .add("order_submitted", |evt: OrderSubmitted| async move {
        // Update read database / cache...
        // Ok(()) is equivalent to Handled::Ack
        Ok(())
});
```

## Configuration

All routes and endpoints can be defined via a configuration file (for example `mq-bridge.yaml`), JSON, or environment variables. For a complete reference of options, middleware, and examples, see the [Configuration Guide](docs/CONFIGURATION.md).

Important route-level knobs:

*   `batch_size`: maximum messages per route iteration. Defaults to `512`; lower it for tighter failure isolation or large payloads.
*   `concurrency`: number of route workers. Defaults to `1`; useful for high-latency handlers or endpoints.
*   `commit_concurrency_limit`: maximum in-flight commit operations, whether they are queued through ordered commit sequencing or run concurrently for independent-ack transports. Defaults to `4096`.

Middleware can be attached to inputs or outputs. The most commonly used ones are `retry`, `dlq`, `deduplication`, `limiter`, `cookie_jar`, and `transform`. Retry/DLQ are especially useful with batching because partial failures can be retried or sent to a DLQ without treating the entire batch as equally broken.

### Transforming JSON messages

The `transform` middleware reshapes JSON payloads declaratively, so field renaming and type
fixing do not need a custom handler. It runs two optional stages over a single parse:
`mapping` (rename, move, nest) and then `schema` (coerce, apply defaults, validate).

```yaml
csv_to_kafka:
  input:
    file: { path: "users.csv", format: csv }
  output:
    middlewares:
      - transform:
          mapping:
            firstName: "$.first_name"
            lastName: "$.last_name"
            id: "$.user_id"
            "address.city": { path: "$.city", default: "unknown" }
          schema_file: "schemas/user.json"
      # `dlq` comes after `transform`: publisher middlewares are wrapped in list order,
      # so the last entry is the outermost layer and sees the earlier ones' failures.
      - dlq:
          endpoint: { file: { path: "rejected.jsonl" } }
    kafka: { topic: "users", url: "localhost:9092" }
```

With the CSV source above producing `{"first_name":"John","last_name":"Smith","user_id":"42"}`,
the mapping yields `{"firstName":"John","lastName":"Smith","id":"42","address":{"city":"unknown"}}`
(`$.city` is absent, so `address.city` falls back to its `"unknown"` default) and the schema
then coerces `id` to the integer `42`.

Failures are always non-retryable and name the offending field, e.g.
`transform failed at $.items[1].qty [coercion]: cannot coerce string "oops" to integer`.
On an **output** endpoint the message is failed so a following `dlq` captures it; on an
**input** endpoint it is dropped from the batch and acknowledged, which keeps invalid data
out of the route.

See **[REFERENCE.md](docs/REFERENCE.md#transform)** for the full option list, the supported schema
subset, and the exact set of allowed type coercions.

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
The times are not stable yet, it is therefore recommended to perform the integration performance test if you want to measure throughput.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup notes, code style, and pull request guidelines.

## AI Disclaimer

This library has been written with a lot of AI assistance.

The core started as my own code, and many endpoints and docs were expanded with help from Gemini, CodeRabbit, Claude, and Codex. The useful part was speed: once the endpoint traits were stable, adding more transports became much easier. The dangerous part is the usual one: generated code can look plausible while still missing important details. I am aware that in year 2026, AI is still not generating perfect code and sometimes
breaks simple stuff or forgets important lines during refactorings that 
later result in severe bugs.

For that reason I reviewed each commit manually to prevent hard-to-fix architectural issuess and cleaned up, and refactored the generated output.

**I do trust the current code as much as if it would be completely written by myself.**

Due to the large feature set, there may still be unfixed issues. The current focus is testing and documentation.

Some parts of the code are more verbose than I would write by hand, but I kept the readable parts when they worked well. I am not a native English speaker, so AI assistance is also useful for documentation. The important part is that the code is reviewed and tested, not that every sentence or helper function looks hand-typed from the first draft.

## License
`mq-bridge` is licensed under the MIT License.
