# Deduplication

Two complementary ways to keep replayed or retried records from creating duplicates: the
[`deduplication`](../engine/reference.md#deduplication) middleware (filters *before* the sink),
and the sink's own unique constraint (the robust choice for multi-writer ETL, covered in
[Upserts & insert-if-absent](upserts.md)).

## The `deduplication` middleware

Drops messages whose ID was already seen within a TTL. **Input only.** Requires the `dedup`
feature:

```yaml
input:
  middlewares:
    - deduplication: { store: "sled:///var/lib/mq-bridge/dedup", ttl_seconds: 3600 }
  kafka: { topic: "orders", url: "localhost:9092" }
```

### Picking a store

`store` selects the backend by URL scheme, and the scheme decides whether deduplication is
process-local or **shared across every instance of the route**:

| `store` | Scope | Extra feature |
|---|---|---|
| `sled:///path` (or a bare path) | per-process only | — |
| `memory://[name][?max_keys=N]` | per-process, forgotten on restart | — |
| `mongodb://host/db[/collection]` | shared between instances | `mongodb` |
| `postgres` / `mysql` / `mariadb` / `sqlite` `://…[/table]` | shared between instances | `sqlx` |

The collection/table defaults to `mqb_dedup_<route>`. Under `mqb copy`, whose route name
is new on every run, it defaults to `mqb_dedup_copy_<id>`: the id is derived from source,
destination and `--filter` without credentials, so the same command finds its keys again.
A `sqlite://` URL has no place for a table name and keeps the per-run default. Point a shared store at the deployment
your sink already uses rather than standing up extra infrastructure. `sled_path` is the legacy
spelling of a local sled store, equivalent to `store: "sled://<path>"`. `memory://` is the
fastest store (~3x sled) but forgets every key on restart, so redeliveries after a restart go
through again; it holds at most `max_keys` keys (default 1,000,000), evicting the oldest first.

```yaml
# Shared across every instance of this route.
- deduplication: { store: "mongodb://localhost:27017/etl", ttl_seconds: 3600 }
```

A `sled` store is **per-process, not cluster-wide**, so for multi-writer pipelines use either a
shared store above or the sink constraint (below). Even with a shared store, the sink's own
unique constraint remains the more robust choice when the sink has one — it is already the
authority, with no second write.

## Sink-side dedup (the robust path)

The most robust place to dedup is the sink's own unique constraint — it's already shared across
every writer:

- **MongoDB** — `id_field` maps a business key to the unique `_id`; a duplicate is an idempotent
  skip.
- **SQL** — `ON CONFLICT (key) DO NOTHING` / `ON DUPLICATE KEY UPDATE`.
- **ClickHouse** — `ReplacingMergeTree(version)` collapses duplicates by sort key at merge time.
- **File / object storage** — no constraint, so the file name does the job:
  `name_by: source_position` names each part after the source range it holds, and a restart
  skips ranges already written. Needs a replayable input (Kafka, Postgres CDC, SQL cursor,
  MongoDB change stream, `file` in `consume` mode); `object_store` picks it by default for one.

```yaml
output:
  object_store:
    url: "s3://my-bucket/orders"
    name_by: source_position   # part-orders-<partition>-<start>-<end>.jsonl
```

Full examples in [Upserts & insert-if-absent](upserts.md) and, for files,
[Files & object storage](../engine/delivery.md#files--object-storage--name_by).

## Deduplicating CDC replays

A `postgres_cdc` change event's `message_id` is a **stable hash** that includes the table,
key, operation, commit LSN, and intra-transaction ordinal. Replayed changes therefore deduplicate
through the `deduplication` middleware while distinct changes in one transaction remain distinct —
but only within that middleware's reach: a replay arriving more than `ttl_seconds` after the
original is no longer remembered, and a local `sled` store only remembers what *this* route
instance saw. The sink's own constraint (`id_field` / `ON CONFLICT`) is the durable guarantee;
the middleware only saves the sink the write.

An LSN-only sink predicate is not enough when one transaction changes the same key more than once:
those changes share a commit LSN, so the first accepted row can block a later row. Enable
`source_metadata: true` on the `postgres_cdc` source and order sink versions by the pair
`(mqb.src.postgres_lsn, mqb.src.postgres_ordinal)` instead. Persist both metadata values and
compare the pair lexicographically in the upsert predicate:

Both columns must be **typed and `NOT NULL`**. `lsn` as `text` sorts `0/9…` above `0/16…`, which
is backwards, and a `NULL` on either side makes the whole `WHERE` predicate `NULL`, so the update
is skipped and the row silently stops advancing:

```sql
ALTER TABLE orders
  ADD COLUMN lsn pg_lsn NOT NULL DEFAULT '0/0',
  ADD COLUMN ordinal bigint NOT NULL DEFAULT 0;
```

On a table that already has these columns as `text` or as nullable, backfill before switching the
predicate on — `UPDATE orders SET lsn = '0/0' WHERE lsn IS NULL` (same for `ordinal`), then
`ALTER COLUMN … TYPE pg_lsn USING lsn::pg_lsn` and `SET NOT NULL`. A `'0/0'` floor means the first
change event for each existing row wins, which is what a re-snapshot should do.

```sql
INSERT INTO orders (id, body, lsn, ordinal)
VALUES (${payload:id}, ${payload:body}, ${metadata:mqb.src.postgres_lsn}::pg_lsn, ${metadata:mqb.src.postgres_ordinal}::bigint)
ON CONFLICT (id) DO UPDATE
SET body = EXCLUDED.body, lsn = EXCLUDED.lsn, ordinal = EXCLUDED.ordinal
WHERE (EXCLUDED.lsn, EXCLUDED.ordinal) > (orders.lsn, orders.ordinal)
```

The row comparison decides on `lsn` and only consults `ordinal` when the two LSNs are equal.

See the [Postgres CDC → JSONL](../tutorials/postgres-cdc.md) tutorial for the full CDC idempotency
picture, and [Delivery guarantees](../engine/delivery.md) for what identity each source provides
and which sinks absorb a duplicate write.
