# Quick Start

`mqb copy SOURCE TARGET` moves data between two endpoints described as
URIs. The scheme picks the connector; `?query=params` configure it. The existing
`--from SOURCE --to TARGET` form is equivalent and remains supported.

```bash
mqb copy \
  'postgres://localhost/app?table=users' \
  'clickhouse://localhost:8123?table=users&database=analytics'
```

This copies every row currently in `app.users` (PostgreSQL) into
`analytics.users` (ClickHouse). No config file, no UI — just a source and a
destination. Add `--drain` to exit once the source is empty instead of
running as a continuous bridge (see
[Continuous vs. one-shot](#continuous-vs-one-shot) below).

The common copy controls are deliberately small:

```text
mqb copy SOURCE TARGET [--filter EXPR] [--resume | --no-resume] [--drain] [--wait SECS]
```

`--filter` evaluates a readable expression against each top-level JSON payload,
for example `amount > 100` or `status == "paid"`. A false result intentionally
drops and acknowledges the message; malformed JSON and invalid expressions are
errors, while a field that is absent or not a scalar counts as no match and is
warned about once. It is an in-process filter and is not translated into a
database query. Filtering into cloud object storage also changes how the objects
are named — see [Filtering](./reference/cli.md#filtering).

`--resume` asks the source to use its native durable position and fails before
the route starts when that is not safe. The generated state identity includes
the credential-redacted source, destination, and filter, so changing pipeline
semantics starts a new checkpoint while rotating a password does not.
`--no-resume` explicitly ignores optional cursor/checkpoint state and its
diagnostics for cursor-based sources; it is a no-op for native queue/CDC offsets.

The examples below are complete, working commands. Each links to the
full [connector page](./connectors/) for that endpoint, which lists every
available option; the [generated URL reference](./reference/) is the
authoritative source for every parameter's type, default, and description.

## PostgreSQL → ClickHouse

```bash
mqb copy --drain \
  --from 'postgres://user:pass@localhost/app?table=orders' \
  --to 'clickhouse://localhost:8123?table=orders&database=analytics'
```

Reads all rows from the `orders` table and bulk-inserts them into ClickHouse's
HTTP interface. For a resumable non-destructive scan, add
`&cursor_column=id` on `--from` and pass `--resume`; the CLI supplies the stable
cursor id and the SQL source stores the checkpoint in its own database. An
explicit `cursor_id` or `checkpoint_store` in the URI still takes precedence. See
[PostgreSQL](./connectors/postgres.md) and [ClickHouse](./connectors/clickhouse.md).

## Filtered, resumable Kafka copy

```bash
mqb copy \
  'kafka://localhost:9092?topic=orders' \
  'postgres://localhost/app?table=orders' \
  --filter 'status == "paid"' \
  --resume
```

The generated Kafka consumer group is stable for this source, destination, and
filter. Kafka offsets advance only after the destination succeeds, or after a
message is intentionally filtered out.

## PostgreSQL CDC → PostgreSQL

```bash
mqb copy \
  --from 'postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot' \
  --to 'postgres://user:pass@otherhost/replica?table=orders&auto_create_table=true'
```

Streams inserts/updates/deletes from a PostgreSQL logical-replication
publication into another PostgreSQL table, continuously (CDC is a change
stream, so this command doesn't drain — run it as a long-lived process). See
[PostgreSQL CDC](./connectors/postgres.md#postgresql-cdc).

## MQTT → Kafka

```bash
mqb copy \
  --from mqtt://broker.local:1883?topic=sensors/+/temperature \
  --to kafka://kafka.local:9092?topic=sensor-readings
```

Subscribes to an MQTT topic (wildcards supported) and republishes every
message to a Kafka topic, continuously. See [MQTT](./connectors/mqtt.md) and
[Kafka](./connectors/kafka.md).

## RabbitMQ → HTTP

```bash
mqb copy \
  --from rabbitmq://guest:guest@localhost:5672/%2f?queue=orders \
  --to http://internal-api.local/ingest?method=POST
```

Consumes messages from a RabbitMQ queue and POSTs each one to an HTTP
endpoint, continuously. See [RabbitMQ](./connectors/rabbitmq.md) and
[HTTP](./connectors/http.md).

## File (CSV) → MongoDB

```bash
mqb copy --drain \
  --from file:///data/customers.csv?format=csv \
  --to 'mongodb://localhost?database=app&collection=customers'
```

Reads a CSV file (first row = header) and inserts one document per row into
a MongoDB collection, then exits since the source is a finite file. See
[File](./connectors/file.md) and [MongoDB](./connectors/mongodb.md).

## Continuous vs. one-shot

Without `--drain`, `copy` runs as a continuous bridge until Ctrl-C — the
right mode for message brokers (MQTT, Kafka, RabbitMQ) and CDC sources, which
never "end". With `--drain`, `copy` exits once the source yields an empty
batch — the right mode for finite sources (a file, or a full-table read from
a database). `--concurrency` and `--batch-size` tune throughput on both
modes.

`--wait SECS` covers the case in between: a source another process is still
filling. It is long polling over a drain — it implies `--drain`, waits up to
SECS for the first message, and returns as soon as one arrives, so the value is
a ceiling rather than a delay. Without it a drain that starts a moment too early
sees an empty source and exits reporting nothing to do.

```bash
# Blocks until another process writes to the spool, then drains it and exits.
mqb copy --from spool:///var/spool/incoming --to postgres://... --wait 300
```

`--wait` waits by retrying the drain, so it lets go of the source between
attempts. That is invisible for a file or a table, but a source that allows only
one reader at a time — a `dir_spool` queue — is claimable by someone else in the
gap. Where that matters, use `agent-listen`, which holds its source open for the
whole wait.

Each retry is a fresh connection, roughly four a second. That is free for a spool
or a file, but a long `--wait` against a broker or a database reconnects thousands
of times; prefer a short wait there, or a continuous bridge.

## Escape hatch: driver options and full connection strings

Any query parameter that isn't a recognised config field (e.g. `sslmode`,
`replicaSet`) is left on the connection URL untouched, so driver-specific
options just work — including object-typed fields like `tls`, which can
never be set from a single scalar query param and so always stays on the URL
(e.g. `mongodb://host/?tls=true&database=appdb` passes `tls=true` straight
through to the MongoDB driver). If you already have a complete connection
string (copied from elsewhere, or one whose own options would otherwise be
mis-parsed as config), skip decomposition entirely and pass it verbatim with
`?url=<url-encoded string>`:

```bash
mqb copy \
  --from 'mongodb://_/?url=mongodb%3A%2F%2Fuser%3Apass%40host%2Fdb%3Ftls%3Dtrue&collection=orders' \
  --to null:
```

See the [generated reference](./reference/) for each connector's recognised
field names.
