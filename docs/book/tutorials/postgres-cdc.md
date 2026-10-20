# Postgres CDC → JSONL

Stream every insert/update/delete from a PostgreSQL table into a newline-delimited
JSON file, continuously. This is change-data-capture (CDC): instead of reading a
table snapshot, `mq-bridge` follows the write-ahead log via logical replication, so
new changes keep flowing until you stop it.

## Prerequisites

- PostgreSQL with logical replication enabled (`wal_level = logical`).
- A publication on the source table:

  ```sql
  CREATE PUBLICATION mqb_pub FOR TABLE orders;
  ```

  `publication` must already exist; `slot_name` is created automatically if missing
  (it is a permanent, resumable replication slot).

## The one-liner

```bash
mqb copy \
  --from 'postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot' \
  --to file:///data/orders.jsonl?format=json
```

`--from` uses the `postgres-cdc://` scheme (alias `pgcdc://`); the URL underneath is a
plain Postgres URL. `--to` writes each change as one JSON object per line
(`format=json`); the path comes from the URI path itself, not a query param.

Each change arrives as a `CanonicalMessage` whose payload is the flat row and whose
`postgres.operation` metadata marks the operation (insert/update/delete) — the same
convention as MongoDB CDC, so typed handlers work identically across both.

## Equivalent config file

The same thing as a route in `mq-bridge.yaml`, for the [config-driven run
forms](../getting-started/run-forms.md):

```yaml
orders_cdc:
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost:5432/app"
      publication: "mqb_pub"     # CREATE PUBLICATION mqb_pub FOR TABLE orders;
      slot_name: "mqb_slot"      # created if missing (permanent slot, resumable)
  output:
    file:
      path: "/data/orders.jsonl"
      format: "json"
```

## Notes

- **Resumability.** The named replication slot persists progress on the server, so a
  restart resumes from the last confirmed LSN rather than re-copying from the start.
- **This runs forever.** CDC is a continuous stream; the process stays up tailing new
  changes. For a one-shot bounded copy of an existing table, use a plain
  `postgres://…?table=…` source instead (see [Quick start](../quick-start.md)).
- **Performance.** For sustained CDC/ETL throughput and how batching interacts with
  file sinks, see [Performance tuning](../operations/tuning.md).

## See also

- [PostgreSQL CDC connector](../connectors/postgres.md#postgresql-cdc) — URL format and examples.
- [`postgres-cdc` parameter reference](../reference/postgres-cdc.md) — every recognised field.
- [File connector](../connectors/file.md) — formats and options for the sink.
- [Keep a vector index in sync with Postgres](../use-cases/sync-postgres-to-qdrant.md) — the same CDC input feeding embeddings and Qdrant.
