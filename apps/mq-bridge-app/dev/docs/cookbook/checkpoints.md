# Checkpoints & resumable copies

A **checkpoint** is a durable record of how far a source has been read, so a restart continues
where the last run stopped instead of re-copying from the beginning. It is what turns a one-shot
`copy` into a repeatable **incremental sync** you can put on a timer.

Two settings control it, on the **source** endpoint:

| Setting | Meaning |
|---|---|
| `cursor_id` | The checkpoint's key. **Without it, nothing is persisted** — the source still reads correctly, but every restart begins from scratch (a warning is logged). |
| `checkpoint_store` | Where the position is stored. Optional on most sources; see [Picking a store](#picking-a-store). |

Positions are namespaced as `<source>:<cursor_id>`, so several routes may share one store without
colliding. `checkpoint_store` may embed credentials and is treated as a secret.

## Which sources checkpoint

| Source | Enable with | Position stored |
|---|---|---|
| **SQLx** (PostgreSQL / MySQL / MariaDB / SQLite) | `cursor_column` + `cursor_id` | Last value of `cursor_column` |
| **ClickHouse** | `cursor_column` + `cursor_id` + external `checkpoint_store` | Last value of `cursor_column` |
| **MongoDB** (`consume: capture_new` / `capture_all`) | `cursor_id` | Change-stream resume token. For `capture_all`, only the change-stream phase is checkpointed; the initial snapshot is not resumable. |
| **Object store** (`s3://`, `gs://`, `az://`) | `cursor_id` + external `checkpoint_store` | Last fully-acked object key |
| **Postgres CDC** | (automatic) | Confirmed LSN — the replication **slot** is authoritative; `cursor_id` only adds a local copy |

An SQLx source with **no** `cursor_column` is a destructive work queue, not a resumable read —
progress is the deletion of claimed rows, so there is nothing to checkpoint. See
[Endpoints](../reference/endpoints.md).

## Picking a store

`checkpoint_store` selects the backend by URL scheme; a value with **no** scheme is a plain
table/collection name in the source datastore.

| Value | Backend | Use when |
|---|---|---|
| *(absent)* | Source datastore, `mqb_cursors_<source>` | Default. You can write to the source database. |
| `my_cursors` or `/my_cursors` | Source datastore, that name | Same, with a name you choose. |
| `file:///var/lib/mqb/cursors.json` | Local JSON file | The source is read-only, or a dev/CLI one-off. |
| `postgres://…/db/table`, `mysql://…` | External SQL table | Shared operational store; the table name is optional. |
| `mongodb://host/db/collection` | External MongoDB collection | Same, for Mongo shops. |
| `s3://bucket/prefix` (`gs://`, `az://`, `abfs://`) | Cloud object store, one object per cursor | Ephemeral/containerized runners with no local disk. |

Notes:

- **ClickHouse requires an external store.** It cannot cheaply upsert cursor rows, so a
  source-datastore checkpoint is rejected; `cursor_id` without a `checkpoint_store` disables
  resume with a warning. Use `--resume` when a missing resumable configuration must be fatal,
  or `--no-resume` when a full re-copy is intentional and resume diagnostics should be ignored.
- **Object-store sources** must point `checkpoint_store` at a *different* bucket or prefix than
  they read — a cursor object written under the source prefix would be listed and re-read as
  data. The source rejects an overlapping location.
- A file store is written atomically (temp file + rename) and concurrent writers in one process
  are serialized, so several routes may share one file.
- Cloud object-store checkpoints need the `object-store` feature compiled in.

## Using it with `copy`

Pass `--no-resume` to make an intentional full copy without checkpoint diagnostics. For SQL and
ClickHouse, `cursor_column` is still required to page through the source; the flag only ignores
optional `cursor_id` / `checkpoint_store` state. It currently applies to cursor-based SQL and
ClickHouse sources, MongoDB change streams, and object-store sources. It does not change native
queue or CDC offsets and cannot be combined with `--resume`; for sources such as Kafka, NATS,
and Postgres CDC, the flag is intentionally a no-op.

`cursor_id`, `cursor_column`, and `checkpoint_store` are ordinary endpoint config fields, so on
the CLI they are just query parameters on `--from`:

```bash
# Incremental table → table sync. Re-run it as often as you like: each run copies
# only rows whose `id` is greater than the last successfully written row.
mqb copy \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id&cursor_id=orders_sync' \
  --to   'clickhouse://localhost:8123?table=orders&database=analytics' \
  --drain
```

```bash
# Read-only source: keep the cursor next to the job instead of in the source DB.
mqb copy \
  --from 'mysql://ro_user:pass@reporting/app?table=events&cursor_column=event_id&cursor_id=events_export&checkpoint_store=file%3A%2F%2F%2Fvar%2Flib%2Fmqb%2Fcursors.json' \
  --to   'file:///data/events.jsonl' \
  --drain
```

```bash
# MongoDB bulk read that survives a restart mid-copy.
mqb copy \
  --from 'mongodb://localhost:27017/app?collection=orders&consume=capture_all&cursor_id=orders_dump' \
  --to   'file:///data/orders.jsonl'
```

A `checkpoint_store` URL inside a URI must be percent-encoded (`://` → `%3A%2F%2F`), since it is
a query-parameter value. In a YAML config it is written plainly:

```yaml
orders_sync:
  input:
    postgres:
      url: "postgres://user:pass@localhost/app"
      table: orders
      cursor_column: id
      cursor_id: orders_sync
      checkpoint_store: "file:///var/lib/mqb/cursors.json"
  output:
    clickhouse: { url: "http://localhost:8123", table: orders, database: analytics }
```

### `--drain` and checkpoints

`--drain` exits when the source yields an empty batch — i.e. when the checkpoint has caught up
with the table. That is exactly the shape you want for a cron/systemd-timer job: each invocation
drains the backlog since last time and exits 0. Without `--drain`, the same command runs forever,
polling for new rows every `polling_interval_ms` (100 ms by default) — or backing off
exponentially up to `max_polling_interval_ms` while drained, if you set it.

MongoDB `capture_all` follows a change stream once its initial read is done, so a `--drain` run
ends when that stream goes quiet rather than at a known end of data. `capture_new` only ever
emits changes made after it starts and is continuous by nature. For a one-shot read with a real
end, use `consume: snapshot` — non-destructive, no replica set, and not resumable (it rejects
`cursor_id`).

An **object-store** source ends a drain run when it reaches the end of the objects it listed,
which can be before the prefix is exhausted — the checkpoint makes this safe rather than lossy:
each run resumes at the last fully-acked object key, so repeated runs advance until one reports
zero messages. Loop the job until it moves nothing if you need a single pass to cover everything.

## Delivery semantics

Checkpoints are **at-least-once**, never at-most-once:

- The position advances only after the sink acknowledges, and only across the contiguous run of
  acks from the front of the batch. The first nack stops the advance.
- On a partial failure the in-memory read cursor rolls back to the committed boundary, so nacked
  rows are re-read on the next poll rather than skipped until a restart.
- A crash between "rows written" and "checkpoint flushed" replays that batch. **Make the sink
  idempotent** — see [Upserts & insert-if-absent](upserts.md) and
  [Deduplication](deduplication.md).
- If persisting the cursor fails, the route logs a warning and keeps running; rows may be
  reprocessed on restart.

## Gotchas

- **`cursor_column` must be monotonic and non-decreasing** for new rows (`WHERE col > $last ORDER
  BY col ASC`). An autoincrement id or an append-only timestamp works; a mutable `updated_at`
  does not give you deletes, and a column that can go backwards loses rows.
- **Cursor polling captures appends only.** Updates and deletes to already-copied rows are not
  observed. For those, use CDC (`postgres_cdc`, MongoDB `capture_*`).
- **Equal-value groups must fit in a batch.** If more rows share one `cursor_column` value than
  `batch_size`, the reader refuses to advance rather than skipping the remainder. It reports
  `cursor_column '…' has a group of equal values larger than batch_size` and **retries that poll
  indefinitely** rather than exiting, so a `--drain` job hangs instead of failing — watch for the
  repeating log line. Raise `batch_size` above the largest group, or pick a more unique column.
- **The cursor column must be integer or text.** Other types the SQL `Any` driver can't decode
  fail permanently; expose the column as `BIGINT`/`TEXT` through a view.
- **`cursor_column` and `delete_after_read` are mutually exclusive** — one is non-destructive,
  the other consumes.
- **Changing `cursor_id` or `checkpoint_store` starts over.** The position is keyed by both; a
  new key means a full re-copy. Reuse the same pair to continue a sync, and give unrelated jobs
  distinct `cursor_id`s.
- **`capture_all`'s initial snapshot is not incrementally checkpointed.** What gets persisted is
  the change-stream resume token, written once streaming begins; a run interrupted during the
  snapshot re-snapshots from the beginning. Size the sink's idempotency accordingly.
- **ClickHouse and SQL Server**: ClickHouse is polling-only and needs an external store;
  `cursor_column` mode is not supported on Microsoft SQL Server at all.

## See also

- [Endpoints (concepts)](../reference/endpoints.md) — read modes and CDC
- [PostgreSQL parameters](../reference/postgres.md) · [MongoDB](../reference/mongodb.md) ·
  [ClickHouse](../reference/clickhouse.md) · [Postgres CDC](../reference/postgres-cdc.md)
- [CLI commands](../reference/cli.md) — `copy` flags and URI grammar
