# Delivery guarantees

What `mq-bridge` promises about duplicates and loss, what it needs from you, and which
source/sink combinations give you which guarantee.

> **Short version.** `mq-bridge` is **at-least-once**. A message is acked only after the output
> chain reports success, so nothing is lost on a crash — but a crash between the write and the ack
> replays the message. Combine at-least-once delivery with an **idempotent write at the sink** and
> you get *effective exactly-once*: the record lands once no matter how many times it is delivered.
> Everything below is about how to arrange that.

## Enabling effective exactly-once

There is no global `exactly_once` switch. The guarantee follows from ordinary endpoint
configuration: provide a replay-stable identity when the source does not already have one, then
configure an idempotent sink write. At startup, `mq-bridge` inspects the route and reports the
inferred guarantee as `effectively-once` or `at-least-once`; it does not silently change how the
sink writes data.

This table is **advice on how to configure an idempotent sink** — it is not a list of
switches the engine flips. A plain `INSERT` is not idempotent; you make it so by writing
the conflict clause yourself.

| Sink | How to make the write idempotent | Reported as `effectively-once` at startup? |
|---|---|---|
| MongoDB | `id_field` set to a payload field or replay-stable template such as `${metadata:mqb.id}` | Yes |
| PostgreSQL / SQLite | `sqlx.insert_query` uses a unique key with `ON CONFLICT … DO NOTHING` | Yes |
| PostgreSQL / SQLite | `sqlx.insert_query` uses a unique key with `ON CONFLICT … DO UPDATE` (convergent upsert) | No — still idempotent, but the startup line says `at-least-once` |
| MySQL / MariaDB | `sqlx.insert_query` uses a unique key with `ON DUPLICATE KEY UPDATE` assigning replay-stable values (a convergent upsert; an accumulating assignment such as `c = c + 1` is **not** idempotent) | No — still idempotent, but the startup line says `at-least-once` |
| File / object store | `name_by: source_position` (the `object_store` default over a replayable Kafka, Postgres CDC, SQL cursor, MongoDB CDC or `consume`-mode file source). Needs positions that *repeat* across runs — a file source in `subscribe` or `group_subscribe` mode stamps a per-run epoch, so its names never collide and a replay is not recognised | Yes |

The right-hand column only describes the **informational log line** `mq-bridge` emits when a
route starts. The inference is deliberately conservative: it recognises the two forms it can
read off the configuration with certainty, and says `at-least-once` for everything else. A
route in the third or fourth row is still effectively-once in practice — the sink absorbs the
replay either way. Do not read `at-least-once` in the log as "this route creates duplicates".

For a source without stable identity, derive one once and consume it at the sink:

```yaml
input:
  middlewares:
    - id: "${payload:order_id}"       # writes metadata mqb.id
  file: { path: "orders.jsonl" }

output:
  sqlx:
    url: "sqlite://orders.db"
    table: orders
    insert_query: >
      INSERT INTO orders (id, body)
      VALUES (${metadata:mqb.id}, ${payload:body})
      ON CONFLICT (id) DO NOTHING
```

The target column must actually have a `PRIMARY KEY` or `UNIQUE` constraint. `DO NOTHING` gives
insert-once semantics; an appropriate `DO UPDATE` clause gives convergent upsert semantics.

## What exactly-once actually requires

It is four separate properties, and they fail independently:

1. **Deterministic identity** — a key for the record that is the same on every replay. Without
   this, nothing downstream can recognise a duplicate.
2. **An idempotent (or transactional) write** — the sink must absorb a repeat of the same key.
3. **A durable source position tied to the write** — so recovery resumes at the right place.
4. **Fencing** — a restarted or duplicated instance must not race the old one.

`mq-bridge` gives you 1 and 2 across most of the matrix, 3 for two sources, and does not
implement 4. See [What is not provided](#what-is-not-provided).

## Sources: what identity you get

`message_id` is a `u128`. When the source can derive it from the record it carries, it is stable
across replay and usable as a deduplication key. When it cannot, it defaults to a fresh
`fast_uuid_v7` value **per read** — which identifies *this delivery*, not *that record*, and is
therefore useless for dedup across a restart.

| Source | `message_id` stable across replay? | Replayable position (`mqb.src.*`)? |
|---|---|---|
| `kafka` | **Yes** — `mq_bridge.message_id` header, else a 16-byte key, else `partition<<64 \| offset` | **Yes** — topic/partition/offset |
| `postgres_cdc` (and `sqlx` with `publication`) | **Yes** — hash of `schema.table` + replica key + commit LSN | **Yes** — slot/LSN/ordinal |
| `nats` | JetStream: **yes** (stream sequence). Core NATS: only if the producer set `Nats-Msg-Id` | No |
| `mongodb` (`consumer`/`subscriber`) | **Yes** — the stored document `_id` | No |
| `mongodb` (`capture_new`/`capture_all`) | No — fresh id per read | No |
| `amqp` | Only if the producer set the AMQP `message_id` property; the `delivery_tag` fallback resets per channel | No |
| `redis_streams` | Only if the producer wrote a `mq_bridge.message_id` field; the entry ID is **not** used | No |
| `http` / `websocket` / `grpc` | From the request when it carries an id | No (request/reply, not replay) |
| `dir_spool` | **Yes** — the sidecar's `message_id`, when the chunk was written by `mq-bridge`. A foreign producer's payload-only chunk gets a fresh id | No — a drained chunk is deleted |
| `mqtt`, `aws` (SQS), `zeromq`, `ibm_mq`, `file`, `object_store`, `clickhouse` | No — fresh id per read | No |

Two consequences worth internalising:

- A `file` or `object_store` **source** cannot deduplicate on `message_id`. Re-reading the same
  file produces entirely new ids. Derive a business key instead — see
  [Giving a source an identity](#giving-a-source-an-identity).
- `mq-bridge` writes `mq_bridge.message_id` on the sink side for Kafka, NATS and Redis Streams, so
  a `mq-bridge → broker → mq-bridge` hop preserves identity end to end even when the broker itself
  has no id concept.

### Giving a source an identity

For the sources in the bottom rows, the `id` middleware derives one from the message itself and
stores it in the `mqb.id` metadata key:

```yaml
input:
  middlewares:
    - id: "${payload:order_id}"
  file: { path: "orders.jsonl" }
```

Re-reading the same record now yields the same `mqb.id`, even though each read mints a fresh
`message_id`. Three properties make it the right carrier:

- **It is a string**, so it holds the business key in its original form. `message_id` is a `u128`
  and could only hold a hash of it — which a sink cannot then use as a readable `_id`.
- **It propagates.** `mqb.id` sits deliberately outside the `mqb.src.*` namespace, so publishers
  do not strip it: an identity describes the record, not the hop it arrived on. Kafka already
  forwards every non-`mqb.src.*` metadata entry as a header, so it survives that hop today.
- **It is opt-in.** No `id` entry means no wrapper and no cost.

Once set, read it anywhere metadata is available — `${metadata:mqb.id}` in a `deduplication`
key, a Kafka `partition_key`, or a handler.

**Watch the ordering.** Consumer middlewares wrap in reverse, so the entry closest to the *end*
of the list touches an incoming message *first*. Anything reading `mqb.id` must be listed
**before** the `id` that produces it:

```yaml
input:
  middlewares:
    - deduplication: { store: "sled:///var/lib/mqb/dedup", ttl_seconds: 3600, key: "${metadata:mqb.id}" }
    - id: "${payload:order_id}"
  file: { path: "orders.jsonl" }
```

Reversed, `mqb.id` is still unset when `deduplication` reads it, and dedup silently falls back to
`message_id`.

**Identity is not a version.** `mqb.id` answers "which record is this", not "which revision".
Do not point a CDC route's dedup key at it: every update to a row shares one business key, so
they would collapse into one. That is why `postgres_cdc` builds its `message_id` from
`schema.table + key + **lsn**` — dedup on a change stream needs identity *and* version, and
should keep defaulting to `message_id`. Use `mqb.id` for sink keying and correlation.

`id` is input-only; on an output it is a startup error rather than a silent no-op. The key is set
only when every selector in the template resolves — a partial render such as `"acme-"` would hand
one identity to every message missing the field, so it is dropped instead.

## Sinks: how a duplicate write is absorbed

| Sink | Mechanism | How to enable |
|---|---|---|
| `mongodb` | Unique `_id` index; dup-key (11000) is treated as an idempotent success | `id_field` |
| `sqlx` (PostgreSQL / MySQL / SQLite) | The table's own `UNIQUE`/`PRIMARY KEY` | `ON CONFLICT` / `ON DUPLICATE KEY` in `insert_query` |
| `clickhouse` | `ReplacingMergeTree` collapses at merge time | Table DDL — no `mq-bridge` config |
| `file`, `object_store` | Deterministic, sortable part names + covered-range recovery | `name_by: source_position` (needs a source that reproduces the same positions on a re-read — a file source only in `consume` mode; the `object_store` default under `auto`) |
| `dir_spool` | Not idempotent: every write consumes a new `{seq}` value, so a replay creates a new chunk path even when `naming_pattern` contains `{message_id}` | Rely on downstream deduplication or an idempotent sink |
| `kafka` | `enable.idempotence` dedups **producer retries within one session** — this is *not* exactly-once semantics | On by default |
| `nats`, `amqp`, `mqtt`, `redis_streams`, `aws`, `ibm_mq`, `zeromq` | None | Deduplicate at the next consumer instead |

## Picking a combination

**Any source → a database sink.** The easy case. You do not need a replayable source position at
all — the sink's unique constraint is already shared across every writer and is the authority. Give
it a deterministic key (`id_field`, or a `ON CONFLICT` column) and you are done.

**Any source → files or object storage.** A filesystem has no unique constraint, so this route
needs a replayable source position and therefore works **only from `kafka` or `postgres_cdc`**. See
[Files & object storage](#files--object-storage--name_by).

**A source → a broker sink** (Kafka, NATS, MQTT, …). The sink cannot deduplicate. Either filter
before it with the [`deduplication` middleware](#the-deduplication-middleware), or accept
at-least-once and make the *downstream consumer* idempotent.

**A route with a handler.** Sink-side idempotency happens *after* the handler runs. If that matters,
see [Handlers](#handlers).

---

## Deduplication & idempotent writes

For ETL, at-least-once delivery plus an **idempotent write** gives you effective exactly-once: a
replayed or retried record must not create a duplicate row. The most robust place to enforce this is
the sink database's own **unique constraint** — it is already shared across every writer, so no extra
state store is needed. Both database sinks lean on this instead of an application-side cache.

### MongoDB — `id_field`

Point `id_field` at a top-level payload field and its value becomes the document `_id`. Alternatively,
use the template form `id_field: "${metadata:mqb.id}"` to consume an identity produced by the input `id` middleware. Re-inserting
the same business key then hits the unique `_id` index and is treated as an idempotent success (the
duplicate is skipped, not errored):

```yaml
orders_to_mongo:
  input:  { kafka:   { topic: "orders", url: "localhost:9092" } }
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "shop"
      collection: "orders"
      format: json
      id_field: "order_id"   # payload {"order_id": "A-1", ...} → _id = "A-1"
```

The equivalent configuration using the shared identity carrier is:

```yaml
input:
  middlewares:
    - id: "${payload:order_id}"
output:
  mongodb:
    # ... connection and collection ...
    id_field: "${metadata:mqb.id}"
```

A plain `id_field` preserves the payload value's BSON type; the template form renders a string and
accepts only replay-stable payload or metadata tokens.

The field's JSON type is preserved (a number stays a BSON integer). The payload must be JSON and
contain the field, otherwise the message is dead-lettered rather than written with a random `_id` —
silently minting one would defeat deduplication. Use `id_field` on **sink** collections only: a
business-key `_id` is not compatible with the `consumer`/`subscriber` competing-consumer modes, which
require a UUID `_id`.

### SQL (`sqlx`) — `ON CONFLICT` / `ON DUPLICATE KEY`

The `insert_query` is user-supplied, so you write the dialect's upsert directly. This requires a
pre-existing `UNIQUE`/`PRIMARY KEY` on the key column, and is incompatible with `bulk_copy` (COPY
cannot express `ON CONFLICT`) — so you trade peak throughput for deduplication.

```yaml
# PostgreSQL — insert if absent (drop duplicates):
insert_query: "INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON CONFLICT (id) DO NOTHING"

# PostgreSQL — upsert (last write wins):
insert_query: "INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body"

# MySQL / MariaDB:
insert_query: "INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON DUPLICATE KEY UPDATE body = VALUES(body)"

# SQLite:
insert_query: "INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON CONFLICT (id) DO NOTHING"
```

A plain `INSERT` without a conflict clause instead fails the row as a **non-retryable** error: a
configured `dlq` captures it, and without one it is logged and dropped. Add the conflict clause
when replays are expected. `${payload:field}` binds a typed value from the JSON payload;
`${metadata:key}` binds a metadata string.

### ClickHouse — `ReplacingMergeTree`

ClickHouse has no unique constraints; dedup is a table-engine property. Create the target as
`ReplacingMergeTree(version)` keyed by your business key via `ORDER BY`, using a monotonic column as
the version (e.g. an ingest timestamp, or `postgres.lsn` from a CDC source). ClickHouse collapses
rows with the same sort key at merge time, keeping the highest version; read with `FINAL` (or
`argMax`) to see the deduplicated result:

```sql
CREATE TABLE orders (id UInt64, body String, version UInt64)
ENGINE = ReplacingMergeTree(version) ORDER BY id;
-- mq-bridge just inserts rows; duplicates for the same id collapse on merge.
SELECT * FROM orders FINAL;
```

`ReplacingMergeTree` deduplicates by business key at merge time. Separately, ClickHouse also dedupes
identical *re-inserted blocks* natively: a retried `send_batch` that resends the same block is dropped
server-side (default one-hour window) — `insert_deduplication_token` lets you make that explicit, but
mq-bridge does not set one, so rely on `ReplacingMergeTree` for logical dedup and treat block-level
dedup only as retry-safety.

### Postgres CDC — deterministic id + `postgres.key`

The `postgres_cdc` source resumes from the slot's durable `confirmed_flush_lsn`. In-band standby
feedback is asynchronous and is *not* flushed when the stream stops, so the last acks are made durable
by the consumer's `Drop`, which stops the stream and advances the slot synchronously. Re-delivery is
therefore avoided only on a restart that actually runs that teardown — a host process that exits
without dropping the route (or one on a current-thread Tokio runtime, where the blocking advance is
skipped) replays everything since the last asynchronous feedback tick. Set `checkpoint_store` to a
`file://` path for a second, per-ack durable position that survives teardown regardless. Treat the
source as at-least-once and make the sink idempotent.

Each change event carries the full row (so the primary key is in the payload), `postgres.lsn` (a
monotonic version), `postgres.operation`/`schema`/`table`, and — when the table has a primary key /
replica identity — `postgres.key` (the key value). The event's `message_id` is a stable hash of
`schema.table + key + lsn`, so a replayed change deduplicates through the `deduplication` middleware,
and Mongo `id_field` or a SQL `ON CONFLICT` on the key column make the sink write idempotent. Use
`postgres.lsn` as the version to drop stale replays
(`... DO UPDATE ... WHERE excluded.lsn > orders.lsn`).

*Known edge:* if the same primary key is changed twice **within a single transaction**, both events
share that transaction's commit LSN, so they produce the same `message_id`. The `deduplication`
middleware then treats the second as a duplicate and drops it. The sink still converges to the final
row state, but the intermediate change is not delivered — if you need every intra-txn revision, do not
rely on the `message_id`/middleware path for those rows.

### The `deduplication` middleware

The middleware is a complement, not a replacement: it filters duplicates *before* the sink, and it is
**consumer-only** (configuring it on an output is a startup error). Prefer the sink constraint for
multi-writer ETL; reach for the middleware when the sink has no constraint to lean on, or when you
need to keep duplicates away from a handler.

Two things decide whether it actually works:

- **`key`.** The default keys on `message_id`, which most sources mint fresh per read (see the
  [source table](#sources-what-identity-you-get)) — the route then looks configured and dedupes
  nothing. Set `key` to a business key, either directly (`"${payload:order_id}"`) or via
  `"${metadata:mqb.id}"` when an [`id` middleware](#giving-a-source-an-identity) already derived
  one. An unresolvable template also falls back to `message_id` with only a warning, so a typo
  fails silently.
- **`store`.** `sled` is single-instance. Point it at a shared MongoDB or SQL deployment for
  anything scaled out.

```yaml
input:
  middlewares:
    - deduplication:
        store: "mongodb://localhost:27017/shop"
        ttl_seconds: 86400
        key: "${payload:order_id}"
  kafka: { topic: "orders", url: "localhost:9092" }
```

`middlewares` sits **beside** the endpoint type, not nested inside it. Requires the `dedup` feature.

### MongoDB — branch on insert vs. duplicate (`report_outcome`)

Sometimes you need to *act* on whether a record was newly inserted or already existed — enrich only
fresh rows, or reply with the existing entry for duplicates. Set `report_outcome: true` and the Mongo
publisher returns the message tagged with metadata `mongodb.outcome` = `inserted` (fresh write) or
`existed` (dup-key on the unique `_id`). Wrap it in a `request` endpoint to forward that tagged
message into a `switch` that routes on `mongodb.outcome`:

```yaml
orders_upsert_branch:
  input: { kafka: { topic: "orders", url: "localhost:9092" } }
  output:
    request:                       # calls `to`, forwards its response to `forward_to`
      to:
        mongodb:
          url: "mongodb://localhost:27017"
          database: "shop"
          collection: "orders"
          format: json
          id_field: "order_id"     # deterministic _id → insert-if-absent
          report_outcome: true     # → mongodb.outcome = inserted | existed
      forward_to:
        switch:
          metadata_key: "mongodb.outcome"
          cases:
            inserted: { ref: "enrich_new_order" }   # e.g. build entry X, reply
            existed:  { ref: "handle_duplicate" }   # e.g. reply with parts of X
          default:   { file: { path: "orders-unrouted.jsonl" } }
```

`report_outcome` is sink-only and pairs with `id_field`; without a deterministic `_id` there is no
duplicate to detect. Do **not** also set `request_reply: true` — that switches the publisher to the
reply-polling path, which never reports an outcome and times out with nothing answering. Left
unwrapped by `request`, the tagged message is returned as the route's response as usual.

Keep the `default` arm: when the Mongo send *errors*, `request` forwards the **original** message,
which carries no `mongodb.outcome`, and a `switch` with no `default` drops unmatched messages.

*The outcome is only truthful on the first attempt.* If the process dies after the insert committed
but before the branch's downstream send committed, the replay hits a duplicate key, Mongo answers
`existed`, and a genuinely-new record takes the duplicate branch. Write the `existed` branch as
"may or may not have been handled — check and repair", not "definitely already done".

### Files & object storage — `name_by`

> **Check first whether you need any of this.** If your records already carry a business key — an
> `id` field in the payload, or `mqb.id` from the [`id` middleware](#giving-a-source-an-identity) —
> then a key-addressed sink (Mongo `id_field`, SQL `ON CONFLICT`) already gives you effective
> exactly-once, and the order objects happen to land in does not matter. Positional naming below is
> for the case where the *sink itself* has to recognise a replay, or where a downstream reader
> depends on replay **order**. Turning it on when you do not need it only adds restrictions.

A filesystem has no unique constraint, so the `file` and `object_store` sinks get replay safety
from the **name** they write under — which works only as far as the source repeats its positions:
a re-read that numbers the same records differently produces different names, and different names
are not a replay to anything downstream. `name_by` picks the scheme:

| `name_by` | Name | Consequence |
| --- | --- | --- |
| `write_time` | `<uuidv7>.<ext>`, optionally under `YYYY/MM/DD/` | unique per write; sorts by write order |
| `source_position` | `part-<topic>-<partition>-<start>-<end>.<ext>`, zero-padded, flat | repeats exactly where the source repeats its positions, making a re-write a no-op; sorts by source order |
| `auto` (default) | `source_position` where the input carries a replay position, else `write_time` | see below |

Under `source_position` the sink groups each batch into runs of consecutive source positions and
writes one immutable part per run, named for the range it covers:

```yaml
kafka_to_s3:
  input:
    kafka: { topic: "orders", url: "localhost:9092", source_metadata: true }
  output:
    object_store:
      url: "s3://my-bucket/orders"
      name_by: source_position   # → part-orders-<partition>-<start>-<end>.jsonl (zero-padded)
```

**`auto` applies to `object_store` only.** Either scheme leaves that sink writing whole immutable
objects — `write_time` one per flushed batch, `source_position` one per contiguous run within it —
so only the name and the grouping change, and deriving them is safe. The `file` sink is different: `source_position` turns
`path` from a file into a *directory* of part files, which is a change of structure rather than of
name, so a `file` sink stays on `write_time` until you ask for parts explicitly.

All of this belongs to the `source_position` path. Before its first write such a sink lists the
prefix once and parses the ranges out of the part names already there, so a replay skips them
without re-encoding or re-uploading. Filtering is **per record, not per batch**, so batch boundaries
are free to differ across restarts — that is what makes it work without a checkpoint protocol, and
it is why the listing cannot wait for a name to collide: a replay on different boundaries names
different objects and collides with nothing. A repeat that does land on the same name is caught
anyway, since the PUT uses `PutMode::Create` and treats `AlreadyExists` as success. A `write_time`
sink never lists — its names are unique per write, so there is nothing to recognise; on the
`source_position` path a fresh prefix pays one empty listing per publisher. A listing that fails is
logged rather than failing the batch, leaving same-name-only deduplication until it succeeds: a
transient failure is retried on the next batch, a denied `ListObjects` is not retried. Local files
stage to a temp name, `fsync`, then `rename` (atomic within a filesystem); object stores have no
atomic rename, so a single PUT under the final name *is* the commit.

This needs a replayable source position. Today that means:

| Source | Position | Restriction |
| --- | --- | --- |
| `kafka` | topic / partition / offset | — |
| `postgres_cdc` | commit LSN + in-transaction ordinal | `sqlx` with `publication` maps onto the same consumer |
| `mongodb` | cluster time + ordinal; the initial snapshot uses its `_id` scan index | `capture_new` / `capture_all` only — `consumer` and `snapshot` have no position |
| `file` | record index within the file | all modes; only `consume` deduplicates across runs (see below) |
| `sqlx` | the `cursor_column` value | cursor polling only; the column must be a unique, non-negative integer |

A route whose output resolves to `source_position` turns `source_metadata` on for its input
automatically; set it explicitly only when you want the `mqb.src.*` keys for something else. An input
from the table above is what `auto` looks for; anything else keeps the sink on `write_time`. An
*explicit* `name_by: source_position` over an input that has no position is rejected when the route
starts, because there is then no way to honour what was asked for. NATS and AMQP also accept
`source_metadata` and emit provenance keys, but a subject or routing key is not a replayable offset,
so they cannot drive positional naming.

**Under `write_time`, an `object_store` sink names objects in write order**, and key order is the
only order a bucket has. At `concurrency: 1` that is source order; above it the name is minted inside
the worker pool, so it is arrival order, and replaying a change stream through the bucket can reorder
updates to the same key. For an input that carries a replay position, `auto` already avoids this. For
one that does not — NATS, MQTT, HTTP, gRPC, ZeroMQ, IBM MQ, Redis Streams — there is no positional
name to fall back to, and `concurrency: 1` is the only remedy. A concurrent route into a
`write_time` object-store sink logs a warning at startup either way: over an input with a position
it points at `name_by: source_position`, over one without it at `concurrency: 1`.

> **Deprecated:** `idempotency: true|false` is the old spelling of
> `name_by: source_position|write_time`. It is still read, but an explicit `name_by` wins over it.

**Numbers inside a part name are zero-padded** so that ASCII sort equals numeric sort — a bucket is
listed lexicographically and that listing order *is* the replay order. A bucket written by a version
before this padding contains unpadded names; the two forms do not sort correctly against each other,
so do not mix them in one prefix.

**MongoDB `capture_all`** reads the collection, then streams changes. Both phases are numbered into
the key so the snapshot always sorts ahead of the changes; a change can never replay before the
document it modifies. The snapshot does not resume across runs — it re-scans from the start — but the
numbering is deterministic, so a restart reproduces the same names and the covered-range recovery
skips them. The exception is inserts or deletes landing in the collection between the two runs: those
shift the numbering, and some documents are then written twice.

**The `sqlx` cursor source** uses the `cursor_column` value itself as the position — the same shape
Kafka Connect's S3 sink gives a Kafka offset. That only works for a unique, non-negative integer
column: a text cursor pages fine but has no contiguous numeric order, and a repeated value would
resolve two rows to one position and drop one of them. The reader rejects both at read time rather
than naming records wrongly. Consecutive ids coalesce into one part file; gaps split it.

**The `file` source** numbers records by their index in the file, not their byte offset (byte offsets
are not consecutive, so every record would become its own part).

That index repeats across runs only in `consume` mode, which always reads from byte 0 — and repeating
is the point: a re-read produces the same names, so the sink recognises them as already covered and
writes nothing. `subscribe` starts at the current end of the file and `group_subscribe` resumes at a
stored byte offset, so their index restarts at 0 over records an earlier run already numbered. Those
two modes therefore carry a **run epoch** in the object name.

The epoch keeps names distinct and keeps runs in order (a later run reads later records, so its
objects sort after the earlier run's). What it does not give you is deduplication across a restart:
records re-read after a crash are written again under new names. That is ordinary at-least-once, and
it is the honest guarantee for a source with no durable per-record position — these modes are
allowed, not rejected, because that guarantee is fine for plenty of pipelines.

For the `file` sink, `name_by: source_position` changes what `path` means: it is the directory that receives
the part files, not the file that is appended to. The sink creates it on startup, so pointing it at an
existing regular file fails there with "Failed to create part-file sink directory".

`compression` and `encryption` work as usual.

Current limits, all of which the sink rejects or logs rather than silently mishandling:

*   No `csv` (each part would need its own header row).
*   `date_partition` is ignored — parts are written flat under the prefix, since the name already
    carries the range. Logged at startup.
*   **One part file per contiguous run per batch.** There is no size-based rolling yet, so a large
    backfill produces many small files (~1000 per 1M rows at `batch_size: 1024`). For `postgres_cdc`
    this is per-transaction, so a high-commit-rate stream produces one small file per commit. Rolling
    would require buffering across batches the route has already acked, which is not safe today.

---

## Handlers

A handler is not a stage between the consumer and the publisher — it is a **publisher wrapper**. The
order on the output side is:

```text
handler  →  output middlewares  →  sink
```

So every sink-side idempotency mechanism on this page runs *after* the handler has already executed.
For a pure ETL route that is fine. For a handler with side effects — enriching, calling an API,
emitting an event — it means duplicates reach your code.

**To keep duplicates away from a handler, put `deduplication` on the route's input.** It filters them
out of the batch before the route ever calls the publisher chain, and the duplicates are acked
straight back at the source. Set `key` to a business key, as above.

**This is still at-least-once, deliberately.** The middleware reserves a key on receive and only
promotes it to "processed" when the message is acked. That in-flight reservation expires after five
seconds, precisely so that a crash between reserve and commit frees the key for redelivery rather
than losing the message. A crash after the handler ran but before the commit will re-run the handler.

To make a handler *effectively*-once, its own effect has to be idempotent:

- **If the handler writes to a store with a unique key**, you already have it — the write is
  absorbed on replay. The handler may run twice; it cannot land twice.
- **If the handler makes a non-idempotent external call** (charge a card, send mail), `mq-bridge`
  has no primitive for this today. You need an idempotency-key cache around the effect itself.

## What is not provided

- **Exactly-once across systems.** Not achievable without a transaction spanning the source's offset
  commit and the sink's write. Nothing here claims it.
- **Kafka transactional EOS.** `enable.idempotence` is on, but `transactional.id` and
  `send_offsets_to_transaction` are not used, so Kafka→Kafka is not exactly-once.
- **Fencing on checkpoint stores.** The `file`/`s3`/`mongodb`/`postgres` checkpoint stores have no
  lease, owner id or epoch. Kafka consumer groups and Postgres replication slots fence structurally;
  the checkpoint stores do not, so a zombie instance after a partial partition can re-emit.
- **A transactional outbox.** When source position and sink write live in the same database, they
  could be committed together — `mq-bridge` has no way to express that today.

## See also

- [REFERENCE.md](REFERENCE.md) — every middleware and structural endpoint, with fields and defaults.
- [ARCHITECTURE.md](ARCHITECTURE.md) — internal design and extension points.
