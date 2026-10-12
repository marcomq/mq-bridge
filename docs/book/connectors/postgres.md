# PostgreSQL / MySQL / MariaDB / SQLite

The `sqlx` connector reads and writes rows in a relational database table.
The same connector backs all four schemes — the URL scheme just selects the
driver. Postgres additionally has a dedicated [CDC connector](#postgresql-cdc)
for streaming change data instead of table reads.

## URL format

```text
postgres://[user:pass@]host[:port]/database?table=<name>
```

The scheme (`postgres`/`postgresql`, `mysql`, `mariadb`, `sqlite`) and
everything up to the query string is passed straight through as the driver
connection string; `table` (and any other recognised field below) is pulled
out of the query string into config. For SQLite, `host`/`database` are
replaced by a file path, e.g. `sqlite:///var/data/app.db?table=orders`.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `sqlx:`.

```yaml
input:
  sqlx: { url: "postgres://user:pass@localhost/app", table: "orders", cursor_column: "id" }
```

The config key is `sqlx` for PostgreSQL, MySQL/MariaDB, and SQLite; the connection string is the `url`.

## Examples

**Full-table read (source), one-shot:**

```bash
mqb copy --drain \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id' \
  --to null:
```

`cursor_column` names a unique, increasing column to read in order. Without it the source
treats the table as an mq-bridge work queue, which needs `id` and `locked_until` columns.

**Write with auto-created table (destination):**

```bash
mqb copy --drain \
  --from file:///data/orders.csv?format=csv \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

**Resumable incremental read, keyed by a monotonic column, continuous:**

```bash
mqb copy \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id&cursor_id=orders_export' \
  --to kafka://kafka.local:9092?topic=orders
```

Each restart resumes from the last `id` seen (persisted via `cursor_id`)
instead of re-copying from the start.

**Table to table, or file to table, by column name:**

```bash
mqb copy --drain \
  --from 'postgres://user:pass@src/app?table=orders&cursor_column=id' \
  --to 'postgres://user:pass@dst/app?table=orders&columns=auto&key=id'

mqb copy --drain \
  --from 'file://orders.csv?format=csv' \
  --to 'sqlite://app.db?table=orders&columns=auto&key=id'
```

`columns=auto` writes each top-level field of a record into the column of the same name; the
table must exist. `key=id` makes a rerun update the rows instead of failing on the duplicate
key. See [Writing by column name](#writing-by-column-name).

**Custom multi-column insert, MySQL:**

```bash
mqb copy --drain \
  --from postgres://user:pass@src/app?table=orders \
  --to 'mysql://user:pass@dst/app?table=orders&insert_query=INSERT+INTO+orders+%28id%2C+sku%2C+qty%29+VALUES+%28%24%7Bpayload%3Aid%7D%2C+%24%7Bpayload%3Asku%7D%2C+%24%7Bpayload%3Aqty%7D%29'
```

(`insert_query` is shown URL-encoded above — the SQL is
`INSERT INTO orders (id, sku, qty) VALUES (${payload:id}, ${payload:sku}, ${payload:qty})`.)

## Key options

| Option | Purpose |
|---|---|
| `table` | **Required.** Table to read from / write to. |
| `cursor_column` + `cursor_id` | Non-destructive, resumable incremental reads instead of a one-shot full-table copy. |
| `checkpoint_store` | (Consumer, `cursor_column` mode) Where to persist the resume cursor. Absent → a `mqb_cursors_<table>` table in the **source** database; a bare name reuses the source datastore with that table; a URL (`file://`, `postgres://`, `mysql://`, `mongodb://`, `s3://`/`gs://`/`az://`/`abfs://`) selects an external backend. Treated as a secret since it may embed credentials. |
| `timestamps` | (Consumer, `cursor_column` mode) `text` (default) renders `timestamptz` as Postgres prints it, `2026-10-04 09:15:23.923277+00`. `rfc3339` renders it in UTC as `2026-10-04T09:15:23.923277Z`, which other systems parse; a `timestamp` without zone gets the `T` but no offset. |
| `auto_create_table` | Publisher creates the destination table if missing. |
| `columns` | (Publisher) `auto` writes each JSON field into the table column of the same name. The table must exist. |
| `key` | (Publisher, with `columns`) Key column(s), comma-separated. A row with the same key is updated; needs a `UNIQUE` or `PRIMARY KEY` on them. |
| `extra_column` | (Publisher, with `columns`) Column that takes the fields without a column of their own, as one JSON object (`jsonb`, `json` or text). |
| `insert_query` | Custom INSERT with `${payload:field}` / `${metadata:key}` tokens for multi-column writes. |
| `bulk_copy` | PostgreSQL only — use `COPY FROM STDIN` for high-throughput bulk loads. |
| `delete_after_read` | Consumer deletes rows after they're processed (mutually exclusive with `cursor_column`). |

Any other query parameter (e.g. `sslmode=disable`) is left on the connection
URL untouched and passed to the driver as-is.

Full field list, types, and defaults: [reference/postgres.md](../reference/postgres.md).

## Reading a table

With `cursor_column` the source pages through the table and leaves it unchanged:

```sql
SELECT … FROM orders WHERE id > <last value> ORDER BY id LIMIT <batch size>
```

Each row becomes one message whose payload is a JSON object with one field per column:

```json
{"id": 42, "sku": "A-1", "qty": 3, "price": "19.90", "created_at": "2026-10-04 09:15:23.923277+00"}
```

**The cursor column** must be unique and only grow, because the next page starts after the
last value read:

- A row whose value equals the last one read is skipped, so a column with repeated values
  (a plain `updated_at`) loses rows.
- A row that appears later with a smaller value is never read. This happens with a sequence
  when a long transaction commits after a shorter one that took a higher number.
- Updates and deletes of rows already read are not seen. Use [PostgreSQL CDC](#postgresql-cdc)
  for those.
- The column can be an integer or a text type. On PostgreSQL it can also be a `timestamp`,
  `timestamptz`, `date`, `numeric` or `uuid`.

**Column types.** Integers, floats, booleans and text keep their JSON type. The other types
arrive as text:

| Database | As a string | As JSON |
|---|---|---|
| PostgreSQL | `numeric`, `uuid`, dates and timestamps, `interval`, `inet`, enums | `json` / `jsonb` and arrays, embedded as they are |
| MySQL / MariaDB | `decimal`, dates and times, `json`, `bit`, and `tinyint` (so a `BOOLEAN` is `"0"` / `"1"`) | |
| SQLite | columns declared `BOOLEAN`, `DATE`, `TIME`, `DATETIME`, `TIMESTAMP` | |

Binary columns (`bytea`, `blob`) are written as a hex string. To get numbers or timestamps in
another form, add a [`transform`](../cookbook/transform.md), or set `timestamps: rfc3339` on
PostgreSQL.

**Progress.** `cursor_id` stores the last value, by default in a table `mqb_cursors_<table>`
of the source database. For a read-only user, point `checkpoint_store` somewhere else; see
[Checkpoints & resumable copies](../cookbook/checkpoints.md). Without `cursor_id` the position
is kept for the current run only.

**Polling.** With `--drain` (`exit_on_empty`) the run ends at the last row. Otherwise the
source asks for new rows every `polling_interval_ms` (default 100); set
`max_polling_interval_ms` to let the interval grow while the table is idle.

## Queue tables

Without `cursor_column` the table is used as a queue between mq-bridge routes: one route writes
messages into it, another takes them out. This is not a way to read an existing table.

A sink without `columns` and `insert_query` writes the whole payload into a `payload` column.
`auto_create_table: true` creates the table; on PostgreSQL it is:

```sql
CREATE TABLE jobs (
  id BIGSERIAL PRIMARY KEY,
  payload BYTEA NOT NULL,
  locked_until TIMESTAMPTZ,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
```

```yaml
enqueue:
  input:  { http: { url: "0.0.0.0:8080" } }
  output: { sqlx: { url: "postgres://localhost/app", table: "jobs", auto_create_table: true } }

work:
  input:  { sqlx: { url: "postgres://localhost/app", table: "jobs", delete_after_read: true } }
  output: { kafka: { url: "localhost:9092", topic: "jobs" } }
```

- The source claims a batch of rows for 60 seconds (`locked_until`) and skips rows another
  consumer has claimed, so several consumers can share one table.
- With `delete_after_read: true` an acknowledged row is deleted. A row that failed stays and
  is delivered again when its claim ends.
- **Set `delete_after_read: true` for a queue.** Without it a row is only hidden for the
  60 seconds of its claim: nothing marks it as done, so it is delivered again and again, once
  a minute, for as long as the route runs. The default is off so that a route deletes nothing
  unless told to; it is only useful if something else removes the processed rows.
- **To copy a table instead of emptying it, use `cursor_column`** ([Reading a table](#reading-a-table)).
  The `mqb copy` command warns when a source has neither setting; a route in a config file
  starts without a warning.
- Only the payload is stored. Message metadata does not pass through the table.

## Writing

| Goal | Setting |
|---|---|
| Fields into the columns of the same name, with upsert | `columns: auto`, `key`; see [Writing by column name](#writing-by-column-name) |
| Your own statement, renamed fields, casts, `ON CONFLICT … DO NOTHING` | `insert_query` with `${payload:field}` tokens; see [Upserts](../cookbook/upserts.md) |
| Fastest load into PostgreSQL | `bulk_copy: true`, with `columns: auto` or a token `insert_query` |
| The whole payload in one column | neither; see [Queue tables](#queue-tables) |

A row the database rejects for a reason that will not change (a type error, a violated
constraint) fails as non-retryable and goes to a [`dlq`](../cookbook/dlq.md) if one is
configured. Connection errors and deadlocks are reported as retryable, for a
[`retry`](../cookbook/retries.md) middleware to repeat.

## Connection, TLS and pool

- **Credentials** can be in the URL or in `username` / `password`, which take precedence. Keep
  them out of the config file with [secrets](../cookbook/secrets.md).
- **TLS.** Either put the driver's option into the URL (`?sslmode=verify-full` on PostgreSQL,
  `?ssl-mode=VERIFY_IDENTITY` on MySQL/MariaDB), or set `tls.required: true`, which selects
  exactly these modes. `tls.ca_file` names a private CA, `tls.cert_file` and `tls.key_file` a
  client certificate; the key file must be unencrypted. The drivers' default mode falls back
  to plain text when the server offers no TLS.
- **Pool.** Endpoints with the same URL share one pool of at most `max_connections` (10)
  connections. Raise it when a route's `concurrency` is higher, and set `shared: false` to
  give an endpoint a pool of its own.

## Database notes

- **PostgreSQL.** A table in another schema is `table: "sales.orders"`. `bulk_copy` and the
  type casts of `columns: auto` exist only here.
- **MySQL / MariaDB.** Both schemes use the same driver. A `key` needs a `UNIQUE` or
  `PRIMARY KEY` index and becomes `ON DUPLICATE KEY UPDATE`.
- **SQLite.** `sqlite:///var/data/app.db` is an absolute path, `sqlite://app.db` a relative
  one. A sink creates the file when it does not exist.

## Recipes

- [Upserts & insert-if-absent](../cookbook/upserts.md): `ON CONFLICT` and `ON DUPLICATE KEY`.
- [Enrichment & lookups](../cookbook/lookup.md): `lookup_query` reads a row for each message.
- [Checkpoints & resumable copies](../cookbook/checkpoints.md): where the cursor is stored and
  how to reset it.
- [Deduplication](../cookbook/deduplication.md) and [Running aggregates](../cookbook/aggregate.md)
  can keep their state in a SQL table.
- [Postgres CDC → JSONL](../tutorials/postgres-cdc.md), and
  [MySQL, SQL Server & Oracle CDC via Debezium](../cookbook/debezium.md).

## PostgreSQL CDC

A separate connector for streaming logical-replication changes (insert/
update/delete) instead of reading a table snapshot. Uses `postgres-cdc://`
(alias `pgcdc://`) to select the endpoint kind; the connection URL underneath
it is a plain Postgres URL.

```text
postgres-cdc://[user:pass@]host[:port]/database?publication=<name>&slot_name=<name>
```

In config, the key is `postgres_cdc`:

```yaml
input:
  postgres_cdc: { url: "postgres://user:pass@localhost:5432/app", publication: "orders_pub", slot_name: "mqb_orders" }
```

**Stream changes from a publication into Kafka, continuous:**

```bash
mqb copy \
  --from 'postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot' \
  --to kafka://kafka.local:9092?topic=app-changes
```

**Replicate a table into another PostgreSQL instance, continuous:**

```bash
mqb copy \
  --from 'postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot' \
  --to 'postgres://user:pass@otherhost/replica?table=orders&auto_create_table=true'
```

`publication` must already exist on the source (`CREATE PUBLICATION mqb_pub
FOR TABLE orders;`); `slot_name` is created automatically if missing.

The server must be PostgreSQL 14 or newer and run with `wal_level = logical`.

TLS follows the URL (`?sslmode=verify-full&sslrootcert=/ca.pem`, with `sslcert` and
`sslkey` for a client certificate) or the `tls` block; both connections the source
opens use the same settings. `sslmode=prefer`, the default, tries TLS
first and falls back to plain text, so it does not guarantee TLS.

A nacked change makes the source reconnect and deliver again from the last confirmed
position, so the changes after it come twice. Rows of one transaction are confirmed
together, when its last row is acked.

### What a change event contains

The payload is the row as a JSON object; the metadata names the change: `postgres.operation`
(`insert`, `update`, `delete`, `truncate`), `postgres.schema`, `postgres.table`,
`postgres.lsn` and `postgres.key` (the primary key values as a JSON array).

- An `update` that changes the primary key also carries `postgres.old_key`. A sink that
  upserts by key has to delete that row itself, or the old row stays. With
  `split_key_change: true` the source does it: it sends a `delete` for the old key first,
  then the `update`. That is two events for one change, and the events after it in the
  same transaction get other message ids than without the option, so do not switch it on
  a route whose deduplication state must stay valid across the change.
- A `delete` carries the key columns only, the other columns are `null`. Set
  `REPLICA IDENTITY FULL` on the table to get the whole old row.
- A large value that an update did not change (an unchanged TOAST column) is not in the
  log; its field is left out of the payload, which is different from `null`.
- `numeric` is a string, to keep every digit. `bigint` is a JSON number: a reader that
  holds numbers as doubles loses precision above 2^53. Numbers inside `json` / `jsonb`
  are read as doubles too.
- `bytea` is base64. Dates, times, `uuid` and `interval` are the text Postgres prints.
  `NaN` and `Infinity` are strings. One-dimensional arrays of the common types are JSON
  arrays; other arrays, ranges, enums and composite types are their Postgres text.

Full field list: [reference/postgres-cdc.md](../reference/postgres-cdc.md).

## Writing by column name

With `columns: auto` the sink reads the table's columns at start and builds the INSERT from each
record. It works on PostgreSQL, MySQL/MariaDB and SQLite.

- **Matching.** A field goes into the column with the same name; if there is none, a name that
  differs only in case matches (`ID` → `id`). When a record has both spellings, the exact
  one is written and the other counts as a field without a column. Fields without a column are not written, and the
  first one is logged as a warning.
- **`extra_column`.** Names a column that takes those fields instead, as one JSON object:
  with `extra_column=extra`, `{"id":1,"color":"red"}` writes `id = 1` and
  `extra = {"color":"red"}`. A record with no such field leaves the column out. When a
  record fills the column itself with an object (or with JSON text holding one, as a SQL
  source delivers it), the collected fields are added to it and its own keys win. An upsert replaces the stored object; it does not merge into it.
- **Missing fields.** A column a record does not name is left out of the statement: on insert it
  gets its default, on update it keeps its value. An explicit `null` writes `NULL`.
- **Types.** On PostgreSQL every value is cast to the column's type, so a `numeric`,
  `timestamptz`, `uuid`, `jsonb` or enum column accepts the text a SQL source or a CSV file
  delivers, a JSON array goes into an array column, and `0`/`1` go into a `boolean`. A nested object or array is written as
  JSON text. A value the column cannot take fails the batch with the database's message.
- **`key`.** Generates `ON CONFLICT (key) DO UPDATE` (PostgreSQL, SQLite) or
  `ON DUPLICATE KEY UPDATE` (MySQL/MariaDB). When one batch has several records with the same
  key, the last one wins.
- **`bulk_copy`** (PostgreSQL) works with `columns: auto`, but not together with `key`. A batch
  whose records name different columns is written in one transaction.
- **A record that cannot be mapped** — not a JSON object, or no field is a column — is rejected
  alone; the rest of the batch is written.

`columns` cannot be combined with `insert_query` or `auto_create_table`.
