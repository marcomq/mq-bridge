# Upserts & insert-if-absent

For ETL, at-least-once delivery plus an **idempotent write** at the sink gives you effective
exactly-once: a replayed or retried record must not create a duplicate row. The most robust
place to enforce this is the sink's own **unique constraint** — it's already shared across every
writer, so no extra state store is needed.

## MongoDB — `id_field`

Point `id_field` at a top-level payload field and its value becomes the document `_id`.
Re-inserting the same business key then hits the unique `_id` index and is treated as an
idempotent success (the duplicate is skipped, not errored):

```yaml
orders_to_mongo:
  input:  { kafka: { topic: "orders", url: "localhost:9092" } }
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "shop"
      collection: "orders"
      format: json
      id_field: "order_id"   # payload {"order_id": "A-1", ...} → _id = "A-1"
```

The field's JSON type is preserved (a number stays a BSON integer). The payload must contain the
field, otherwise the message is dead-lettered rather than written with a random `_id`. Use
`id_field` on **sink** collections only — a business-key `_id` is incompatible with the
`delete` competing-consumer mode, which requires a UUID `_id`.

## SQL (`sqlx`) — `ON CONFLICT` / `ON DUPLICATE KEY`

For a table whose columns are the record's fields, `columns: auto` with `key: id` generates the
upsert for you; see [Writing by column name](../connectors/postgres.md#writing-by-column-name).
The rest of this section is for a hand-written `insert_query`.

`insert_query` is user-supplied, so you write the dialect's upsert directly. This requires a
pre-existing `UNIQUE`/`PRIMARY KEY` on the key column, and is incompatible with `bulk_copy`
(COPY cannot express `ON CONFLICT`) — so you trade peak throughput for deduplication.

```sql
-- PostgreSQL — insert if absent (drop duplicates):
INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON CONFLICT (id) DO NOTHING

-- PostgreSQL — upsert (last write wins):
INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body})
  ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body

-- MySQL / MariaDB:
INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body})
  ON DUPLICATE KEY UPDATE body = VALUES(body)

-- SQLite:
INSERT INTO orders (id, body) VALUES (${payload:id}, ${payload:body}) ON CONFLICT (id) DO NOTHING
```

A plain `INSERT` without a conflict clause instead fails the row as a **non-retryable** error
(captured by a [`dlq`](dlq.md), or logged and dropped without one). `${payload:field}` binds a
typed value from the JSON payload; `${metadata:key}` binds a metadata string.

## ClickHouse — `ReplacingMergeTree`

ClickHouse has no unique constraints; dedup is a table-engine property. Create the target as
`ReplacingMergeTree(version)` keyed by your business key via `ORDER BY`, using a monotonic column
as the version (an ingest timestamp, or `postgres.lsn` from a CDC source):

```sql
CREATE TABLE orders (id UInt64, body String, version UInt64)
ENGINE = ReplacingMergeTree(version) ORDER BY id;
-- mq-bridge just inserts rows; duplicates for the same id collapse on merge.
SELECT * FROM orders FINAL;
```

Read with `FINAL` (or `argMax`) to see the deduplicated result. ClickHouse also natively dedupes
identical *re-inserted blocks* (default one-hour window) — treat that only as retry-safety, and
rely on `ReplacingMergeTree` for logical dedup.

## Branch on insert vs. duplicate (MongoDB `report_outcome`)

To *act* on whether a record was newly inserted or already existed, set `report_outcome: true`;
the Mongo publisher tags the returned message with `mongodb.outcome` = `inserted` or `existed`.
Wrap it in a [`request`](../engine/reference.md#request) endpoint to forward that tagged message
into a [`switch`](switch.md) that routes on the outcome:

```yaml
orders_upsert_branch:
  input: { kafka: { topic: "orders", url: "localhost:9092" } }
  output:
    request:
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
            inserted: { ref: "enrich_new_order" }
            existed:  { ref: "handle_duplicate" }
```

`report_outcome` is sink-only and pairs with `id_field`. See also
[Deduplication](deduplication.md) for the middleware-based complement, the
[Postgres CDC tutorial](../tutorials/postgres-cdc.md) for CDC-specific idempotency. When the
same key can change more than once in a transaction, enable `source_metadata: true` and order by
both `mqb.src.postgres_lsn` and `mqb.src.postgres_ordinal`; an LSN-only version cannot order those
changes. The linked deduplication guide shows the complete predicate. Also see
[Delivery guarantees](../engine/delivery.md) for which source/sink pairings add up to
effectively-once.
