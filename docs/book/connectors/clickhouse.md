# ClickHouse

Reads from or bulk-inserts into a ClickHouse table over ClickHouse's HTTP
interface (port 8123 by default; 8443 for HTTPS).

## URL format

```text
clickhouse://host[:port]?table=<name>[&database=<name>]
```

`clickhouse://` is rewritten to `http://` (and `clickhouses://` to
`https://`) before being handed to the ClickHouse client — the scheme only
selects the endpoint kind on the CLI.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `clickhouse:`.

```yaml
output:
  clickhouse: { url: "http://localhost:8123", database: "analytics", table: "orders" }
```

The `url` is the HTTP interface (`http://` or `https://`); `clickhouse://` is CLI-only.

## Examples

**Bulk insert from a full-table Postgres read, one-shot:**

```bash
mqb copy --drain \
  --from postgres://user:pass@localhost/app?table=orders \
  --to 'clickhouse://localhost:8123?table=orders&database=analytics'
```

**Async insert for high-throughput streaming writes, continuous:**

```bash
mqb copy \
  --from kafka://kafka.local:9092?topic=events \
  --to 'clickhouse://user:pass@ch.local:8123?table=events&database=analytics&async_insert=true'
```

**Resumable, non-destructive read of an existing table into Kafka:**

```bash
mqb copy \
  --from 'clickhouse://localhost:8123?table=events&database=analytics&cursor_column=id&cursor_id=events_export' \
  --to kafka://kafka.local:9092?topic=events
```

Each restart resumes from the last `id` seen instead of re-reading from the
start. (Per-column mapping via `columns` is a map field, so it can't be set
from a query param — use a YAML route config for that.)

## Key options

| Option | Purpose |
|---|---|
| `table` | **Required.** May be schema-qualified (`db.table`). |
| `database` | Defaults to `default`. |
| `columns` | Map target columns to `${payload:field}` / `${metadata:key}` tokens instead of inserting the whole JSON payload as one row. |
| `async_insert` | Server-side buffered inserts for higher publisher throughput. |
| `cursor_column` + `cursor_id` | Non-destructive, resumable reads of an existing table. |
| `checkpoint_store` | (Consumer, `cursor_column` mode) Where to persist the resume cursor. ClickHouse can't do per-row cursor upserts, so a durable checkpoint needs an **external** store URL: `file://`, `postgres://`/`mysql://`, `mongodb://`, or `s3://`/`gs://`/`az://`/`abfs://`. Treated as a secret since it may embed credentials. |
| `request_timeout_ms` | Limit for one request. Unset means none, which suits very large inserts; set it when a stalled server must not stall the route. |
| `tls` | `ca_file`, `cert_file` + `key_file` (client certificate, unencrypted key), `accept_invalid_certs`. `required: true` refuses an `http://` URL. |

**Rows ClickHouse refuses.** An insert is one statement, so ClickHouse answers for the
batch, not per row. When it refuses the data (HTTP 400, for example a string in a number
column), the batch is halved and sent again until the refused rows stand alone. Those fail
as non-retryable: add a `dlq` to keep them, otherwise they are logged and dropped. The
other rows are inserted. With `async_insert` and `wait_for_async_insert: false` the server
answers before it looked at the rows, so a bad row is never reported.

Full field list: [reference/clickhouse.md](../reference/clickhouse.md).
