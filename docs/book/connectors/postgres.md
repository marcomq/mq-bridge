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
opens use the same settings. `sslmode=prefer`, the default, is plain text.

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
