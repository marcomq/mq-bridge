# Endpoints (connectors)

An **endpoint** is a source (consumer) or sink (publisher) for messages. Every endpoint —
whatever the underlying protocol — is driven behind the same `receive_batch` / `send_batch`
shape, so any source can feed any sink. In config, an endpoint is a single-key object naming
the connector:

```yaml
input:  { kafka:   { url: "localhost:9092", topic: "orders" } }
output: { mongodb: { url: "mongodb://localhost:27017", database: "app", collection: "orders" } }
```

This page catalogs the transport endpoints and their behaviour, for library configs and the
CLI alike. Each connector has its own [connector page](../connectors/README.md) with YAML and
URL examples, plus a generated parameter table (name, type, default, description).
Structural endpoints (`ref`, `fanout`, `switch`, `response`, …) are documented separately in
[Structural endpoints](../engine/reference.md).

## Supported transports

Kafka, NATS, AMQP (RabbitMQ), MQTT, MongoDB, **Postgres CDC** (logical replication),
PostgreSQL / MySQL / SQLite / MariaDB (SQLx), ClickHouse, HTTP, WebSocket, gRPC, ZeroMQ,
Redis Streams, AWS SQS/SNS, cloud object storage (S3 / GCS / Azure), IBM MQ, files, and
in-memory channels (`memory`, in-process and cross-process IPC).

### Plugin endpoints

These live in their own repositories and load as [native plugins](../extending/plugins.md), so
the core library carries none of their dependencies:

| Plugin | What it does | In `mqb` |
| :--- | :--- | :--- |
| **[Pulsar](../connectors/pulsar.md)** | Apache Pulsar input and output | Built in |
| **[Meilisearch](../connectors/meilisearch.md)** | Document sink into a search index; index scan as input | Built in |
| **[Connect](https://github.com/marcomq/mq-bridge-connect)** | Redpanda Connect's 56 inputs, 68 outputs (`connect+mqtt://…`) and 86 processors; per-message processors also run as middleware | Separate install (size) |

The Connect plugin has its own [page](../connectors/connect.md), with every linked
[input](../connectors/connect-inputs.md), [output](../connectors/connect-outputs.md) and
[processor](../connectors/connect-processors.md).

Outside `mqb` (Python, Node.js, your own Rust host), or for Connect, install the plugin with
`brew install marcomq/tap/<repo>` or `conda install -c marcomq <repo>`; it is then discovered
automatically.

## Consumer vs. subscriber, and nack support

One table per question (which inputs acknowledge, which commit in order, which outputs
reply, where TLS is configured) is in
[CAPABILITIES.md](https://github.com/marcomq/mq-bridge/blob/main/docs/CAPABILITIES.md). It is
rendered from the table the engine exposes in code and checked by tests.

Endpoints default to a **Consumer** pattern (a queue: messages are distributed among
workers). To get **Subscriber** (pub/sub) behaviour, per-backend config is required.

| Backend | Subscriber Config (Pub/Sub) | Request-Reply | Nack Support |
| :--- | :--- | :--- | :--- |
| **AMQP** | Set `subscribe_mode: true` | Emulated (Property) | **Yes** (Basic.nack) |
| **AWS** | N/A (Use SNS) | No | **Yes** (Visibility Timeout) |
| **File** | Set `mode: subscribe` | No | Simulated (In-Memory) |
| **gRPC** | N/A | No | No |
| **HTTP** | N/A | **Native** (Implicit) | **Yes** (HTTP 500) |
| **IBM MQ** | Set `topic` | No | **Yes** (Tx Rollback) |
| **Kafka** | Omit `group_id` | Emulated (Header) | Eventual (Skip Offset) |
| **Memory** (in-process) | Set `subscribe_mode: true` | Emulated (Metadata) | **Yes** (Re-queue), by default **disabled** |
| **Memory** (IPC: `ipc://`, `unix://`, `pipe://`) | Not supported | Not supported | **Yes** (Re-queue), by default **enabled**, consumer-local |
| **MongoDB** | Set `consume: capture_new` (replica set) | Emulated (Metadata) | **Yes** (Unlock) |
| **MQTT** | Set `clean_session: true` | Emulated (Property) | Eventual (Skip Ack) |
| **NATS** | Set `subscriber_mode: true` | **Native** (Inbox) | **Yes** (JetStream Nak) |
| **Postgres CDC** | N/A (streams committed changes) | No | **Yes** (confirmed LSN not advanced) |
| **Redis Streams** | Set `subscriber_mode: true` | No | Eventual (PEL, un-acked) |
| **Sled** | Set `delete_after_read: false` | No | **Yes** (Tx Rollback) |
| **SQLx** | Not supported | No | Eventual (Skip Delete) |
| **WebSocket** | N/A | No | No |
| **ZeroMQ** | Set `socket_type: "sub"` | **Native** (REQ/REP) | No |

- **Request-Reply** — *Native* uses protocol-level correlation (HTTP connection, NATS reply
  subject). *Emulated* publishes a new message to a reply destination (the `reply_to`
  metadata field) carrying a `correlation_id`.
- **Nack** — *Yes* is explicit negative acknowledgement triggering redelivery. *Eventual*
  means redelivery depends on timeout or a connection drop. *Simulated* is handled in-memory.
  *Consumer-local* means the nacked message is retried inside the consumer process and is lost
  if that process dies — the producer is never notified.

## Database sources: change capture vs. polling

Databases have no native pub/sub, so a database source is read one of two ways:

- **Change Data Capture (CDC)** tails the database's own change log — inserts **and**
  updates/deletes — and resumes from a durable log position.
  - **PostgreSQL** — the `postgres_cdc` endpoint streams a logical-replication slot
    (`pgoutput`). It emits flat JSON rows tagged with `postgres.operation` metadata
    (`insert`/`update`/`delete`/`truncate`). Acking a batch confirms the LSN back to the
    server; a nack or interrupted run does **not** advance the confirmed LSN, so replication
    resumes from the last acknowledged position — at-least-once. Enable with the
    `postgres-cdc` feature; requires `wal_level = logical` and a publication. See the
    [Postgres CDC → JSONL](../tutorials/postgres-cdc.md) tutorial.
  - **MongoDB** — `consume: capture_all` (the default) reads existing documents first and then
    keeps capturing (no gap, at-least-once); `consume: capture_new` watches an existing
    collection for changes from now on. Both emit `insert`/`update`/`replace`/`delete` tagged
    with `mongodb.operation` and checkpoint under `cursor_id`. Change streams need a replica
    set; without one, use `consume: snapshot` for a one-shot read.
  - **MySQL, MariaDB, SQL Server, Oracle, Db2** — no native endpoint. Run Debezium Server
    beside mq-bridge and read its output; see [CDC via Debezium](../cookbook/debezium.md).
- **Cursor polling** pages an existing table by a monotonic `cursor_column`
  (`WHERE col > $last ORDER BY col ASC`), persisting the last read value under `cursor_id`.
  Captures **appends only** — updates and deletes are not observed. Available on **SQLx**
  (PostgreSQL / MySQL / MariaDB / SQLite) and **ClickHouse**; the poll interval backs off
  exponentially between `polling_interval_ms` and `max_polling_interval_ms`. **SQLite** and
  **ClickHouse** are polling-only.

> **SQLx read mode is chosen by config, not by driver.** With **no** `cursor_column`, an SQLx
> source is a **competing-consumers work queue**: it atomically claims rows via a `locked_until`
> lease and deletes them on ack. This requires the table to carry the queue schema — an `id`, a
> `payload`, and a `locked_until` column — on **every** driver; it is **not** a plain full-table
> read. To read an arbitrary table non-destructively (the ETL path), set **`cursor_column`** to
> switch to cursor polling. A source table missing `locked_until` fails fast with a permanent
> error rather than reconnecting forever.

### MongoDB consume modes

| `consume` | mechanism | modifies source | ends on drain | needs a replica set | use for |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `capture_all` (default) | `_id` snapshot, then change stream | no | yes — when the stream goes quiet | **yes** | bulk read / ETL |
| `capture_new` | change stream, new changes only | no | no — continuous by nature | **yes** | ongoing CDC |
| `snapshot` | pages the collection by `_id`, one shot | no | yes | no | reading a standalone `mongod` |
| `consumer` | claim → lock → re-fetch → delete | **yes** | yes | no | work queues, competing readers |

These are not interchangeable. `consumer` buys exclusivity via four round trips per batch and
**deletes what it reads**, so it is for work queues, not for reading a collection; where you
need a single-reader bulk read or ETL pass, the default `capture_all` is roughly 5x faster
(500k docs: `consumer` → `null` 23,667 rows/s vs. `capture_all` → jsonl 120,308 rows/s). Note
`concurrency` does **not** speed up a MongoDB source; batches are fetched serially.

Both `capture_*` modes read the oplog and therefore **require a replica set** — a single-node
one is enough — and refuse to start without one. On a standalone `mongod`, use `snapshot` for a
one-shot non-destructive read. `snapshot` delivers what exists when the run starts and is not a
tail: it pages by `_id`, which is assigned client-side and so does not follow commit order, so
it rejects `cursor_id` rather than resuming above a stored `_id` and silently skipping whatever
a concurrent writer commits below that mark. Incremental reads need commit order — i.e. the
oplog, i.e. a replica set.

> **Removed in 0.4.0:** `consume: subscriber`. It polled `seq > last_seq` and advanced its
> watermark to the highest seq it had seen, so a batch whose seq block was reserved first but
> committed second was skipped permanently — silent loss, with no error and no gap in the
> delivery count. Use `capture_new` for ephemeral fan-out (it now reads arbitrary collections,
> not only documents written by the bridge's own publisher). The deprecated `change_stream:
> true` boolean resolves to `capture_new`.

## Object storage (local / cloud)

The `object_store` endpoint reads and writes local directories, Amazon S3, Google Cloud Storage,
Azure Blob, Cloudflare R2, and anything else the `object_store` crate speaks. Enable it with the
`object-store` feature. Use `local-store:///absolute/path` in CLI URLs (it becomes
`file:///absolute/path` internally), or use `url: file:///absolute/path` in structured YAML.
Credentials and cloud backend options come from the environment
(`AWS_ACCESS_KEY_ID`, `AWS_REGION`, `GOOGLE_SERVICE_ACCOUNT`, `AZURE_STORAGE_ACCOUNT`, …); the
URL scheme picks the backend (`file://`, `s3://`, `gs://`, `az://`).

- **As a sink**, each flushed batch is encoded with the file formats (`normal` JSONL, `json`,
  `text`, `raw`) and written as **one immutable object** at
  `<prefix>/[YYYY/MM/DD/]<uuidv7>.<ext>`. Objects are write-once.
- **As a source**, objects under the prefix are listed in key order, fetched whole, split on
  the delimiter, and emitted. Progress is a durable cursor holding the last fully-acked object
  key: set `cursor_id` and an external `checkpoint_store` (`file://`, `s3://`, `postgres://`,
  `mongodb://`) so a restart resumes. Objects are **never deleted or rewritten**.

```yaml
archive_to_s3:
  input:  { memory: { topic: "events" } }
  output: { object_store: { url: "s3://my-bucket/events", format: normal } }

replay_from_s3:
  input:
    object_store:
      url: "s3://my-bucket/events"
      cursor_id: "replayer-1"
      checkpoint_store: "file:///var/lib/mqb/s3-cursor.json"
  output: { nats: { subject: "events.replay", url: "nats://localhost:4222" } }
```

> Point `checkpoint_store` at a **different** bucket/prefix than the source reads; a cursor
> object under the source prefix would be listed and re-read as data.

## File and object formats

Both `file` and `object_store` share the encodings: `normal`/`json`/`text` write the
`{message_id, payload, metadata}` wrapper (the message id survives the round trip); `raw`
writes payloads verbatim (bare documents, no wrapper). `csv` is supported as a source (and on
file sinks). The file endpoints also carry their own `compression` and `encryption` fields —
see the [Compression](../cookbook/compression.md) and
[Encryption at rest](../cookbook/encryption.md) recipes.

A source on `normal`/`json`/`text` expects that same wrapper. A line that is valid JSON but not
the wrapper becomes the whole payload (its own `metadata` is discarded, with a warning); use
`format: raw` for plain JSON lines.

In the wrapper, a UTF-8 payload is a plain JSON string under `payload`; a binary one
(compressed, encrypted, Protobuf, …) is base64-encoded under `payload_base64`. The two are
mutually exclusive, as in the
[CloudEvents JSON format](https://github.com/cloudevents/spec/blob/main/cloudevents/formats/json-format.md):

```json
{"message_id":"019f9b12-d786-7ebe-a7ec-a1aa71bc47ae","payload":"{\"order_id\":7}"}
{"message_id":"019f9b12-d78a-7c01-b0f4-2f0f4d6a1c33","payload_base64":"KLUv/SBOAQAA"}
```

Sources still read the older byte-array form (`"payload":[123,34,…]`). A **binary** record
written by this version is not readable by an older mq-bridge; text records are compatible in
both directions.

## Memory endpoint and IPC

The `memory` endpoint is both an in-process channel and a cross-process IPC transport over
Unix domain sockets / Windows named pipes. Its `topic` field (alias `url`) doubles as a
transport URL. See the [Cross-process IPC bridge](../tutorials/ipc-bridge.md) tutorial and
[Learn the architecture](../engine/architecture.md#memory-endpoint-and-ipc-transport)
for the URL schemes, roles (consumer is server, publisher is client), wire format and
backpressure.

## IBM MQ

IBM MQ is included in the `full` feature set via the `ibm-mq` feature, which loads the IBM MQ
client library at runtime via dlopen — **no IBM SDK is needed to build**. The redistributable
client only has to be present at runtime, and only if you actually use an IBM MQ endpoint (it
is loaded lazily on first connect). The loader finds the client via the platform default name,
`MQ_INSTALLATION_PATH` (e.g. `/opt/mqm`), or an explicit `MQB_IBM_MQ_LIB` path. To link
statically at build time, use `ibm-mq-static` (requires the IBM MQ SDK). IBM MQ has its own
TLS shape (`IbmTlsConfig`): `tls.cert_file` (alias `key_repository`) is a CMS key repository
path, not a PEM file. See the
[IBM MQ setup guide](https://github.com/marcomq/mq-bridge/blob/main/docs/book/IBM_MQ_SETUP.md).

## Connection sharing

Publishers that target the same server reuse one underlying transport client by default,
consolidating TCP connections, background threads, and batching. Sharing applies to **Kafka,
NATS, MongoDB, SQLx, HTTP, and gRPC**; the client is keyed by connection-level settings (URL,
auth, TLS, client options), never by topic/subject/collection. Set `shared: false` on a
publisher to give it a dedicated connection — see
[Configuration grammar](../engine/configuration.md#connection-sharing) and
[Performance tuning](../operations/tuning.md#connection-pooling--reuse).

## Idempotent writes

For ETL, at-least-once delivery plus an **idempotent write** gives effective exactly-once. The
most robust place to enforce this is the sink's own unique constraint. See the
[Upserts & insert-if-absent](../cookbook/upserts.md) and
[Deduplication](../cookbook/deduplication.md) recipes for the per-sink mechanisms
(`id_field` on MongoDB, `ON CONFLICT`/`ON DUPLICATE KEY` on SQL, `ReplacingMergeTree` on
ClickHouse, deterministic `message_id` + `postgres.key` on Postgres CDC).
