# Query a stream in DuckDB

There is no DuckDB connector, and for analytics you rarely need one: the
[`object_store`](../connectors/object-store.md) sink writes Parquet, and DuckDB queries Parquet
files where they lie, whether in a local directory or a bucket. The same works in reverse: Parquet
that DuckDB exports can be read back into a route.

`format: parquet` needs the `parquet` feature, which the app's default build includes.

## Export a stream as Parquet

```yaml
orders_to_lake:
  batch_size: 10000
  input:
    kafka: { topic: "orders", url: "localhost:9092" }
  output:
    object_store:
      url: "file:///var/lib/mqb/lake/orders"   # or s3://bucket/orders, gs://…, az://…
      format: parquet
      compression: zstd                        # the Parquet column codec
```

The same export from the CLI:

```bash
mqb copy --drain \
  'file:///data/orders.jsonl?format=payload' \
  'local-store:///var/lib/mqb/lake/orders?format=parquet&compression=zstd'
```

- Each message payload must be a **JSON object**; its top-level fields become the columns.
  Message metadata is not written.
- The sink writes **one Parquet file per flushed batch**, so `batch_size` sets the file size.
  Thousands of tiny files slow every DuckDB scan, so keep batches large.
- The schema is inferred per batch. A field whose values disagree in type within one batch is
  written as JSON text in that file rather than failing the batch.

## Query it

```sql
SELECT country, sum(amount)
FROM read_parquet('/var/lib/mqb/lake/orders/*.parquet', union_by_name = true)
GROUP BY country;
```

**Always pass `union_by_name = true`.** Because each file has its own inferred schema, a field
that first appears in a later batch is missing from the earlier files. Without the option DuckDB
takes the columns from the first file and **silently drops** the new one; with it, the columns are
merged by name, missing values read as `NULL`, and a column that is `BIGINT` in one file and
`VARCHAR` in another becomes `VARCHAR`.

For a bucket, point the same glob at `s3://…` and give DuckDB credentials through its `httpfs`
extension.

## Choose the file layout

Where the input carries a replay position (Kafka, Postgres CDC, an SQL cursor, a MongoDB change
stream, or a `file` in `consume` mode), the default `name_by: auto` names each file after the
source range it holds: `part-orders-0000000003-…-….parquet`. A restart or replay then skips the
ranges already written, so **re-running the export never duplicates rows**. These names are
written flat, without date folders. A route with a middleware that drops messages (`filter`,
`deduplication`, …) falls back to write-time names under `auto`, to avoid one file per gap; set
`name_by: source_position` to keep replay safety anyway. See
[Files & object storage](../engine/delivery.md#files--object-storage--name_by) for the details.

To partition by date instead, ask for write-time names and the Hive layout:

```yaml
    object_store:
      url: "file:///var/lib/mqb/lake/orders"
      format: parquet
      name_by: write_time
      date_partition_style: hive   # year=YYYY/month=MM/day=DD/<uuidv7>.parquet
```

```sql
SELECT year, month, day, count(*)
FROM read_parquet('/var/lib/mqb/lake/orders/**/*.parquet',
                  hive_partitioning = true, union_by_name = true)
GROUP BY ALL;
```

DuckDB reads `year` as a number but keeps the zero-padded `month` and `day` as text, so filter
with `month = '09'`. The dates are the write time (UTC), not a field of the record. The two
layouts exclude each other: write-time names do not recognise a replay.

## Feed DuckDB results back into a route

Export a query result to Parquet in a directory of its own, then read it with an `object_store`
source. Each row arrives as one JSON-object message:

```sql
COPY (SELECT id, country, amount * 2 AS doubled FROM read_parquet('lake/*.parquet', union_by_name = true))
TO '/var/lib/mqb/outbox/2026-09-28.parquet' (FORMAT parquet);
```

```yaml
duckdb_results_to_nats:
  input:
    object_store:
      url: "file:///var/lib/mqb/outbox"
      format: parquet
      cursor_id: "duckdb-outbox"
      checkpoint_store: "file:///var/lib/mqb/checkpoints/outbox.json"
  output:
    nats: { subject: "orders.scored", url: "nats://localhost:4222" }
```

The source reads files in name order and checkpoints the last one it finished, so give each export
a name that sorts after the previous one, such as a date or timestamp.

## Not covered

Writing into a `.duckdb` database file (tables, upserts, transactions) needs a native DuckDB
endpoint, which does not exist. Parquet covers the common case of making a stream queryable.
