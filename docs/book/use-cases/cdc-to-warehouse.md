<!-- description: Stream Postgres changes into a warehouse table such as Snowflake: one route with mq-bridge's Postgres CDC, a Redpanda Connect Bloblang mapping and a faucet sink, existing rows copied first, rejected rows to a DLQ. Runnable docker-compose example. -->

# How to stream Postgres changes into a warehouse (CDC → Bloblang → Snowflake)

One mq-bridge route copies a Postgres table and then follows its changes into an append-only
warehouse table: one row per insert, update and delete, with the operation and the commit
position. It combines three ecosystems in one process: mq-bridge's own Postgres CDC source, a
Bloblang mapping from the [Connect plugin](../connectors/connect.md) and a sink from the
[faucet plugin](https://github.com/marcomq/mq-bridge-faucet). It needs no Kafka and no code.

Runnable example: [`examples/postgres-cdc-to-warehouse`](https://github.com/marcomq/mq-bridge/tree/main/examples/postgres-cdc-to-warehouse).
It writes to a second Postgres, so it runs offline; for Snowflake only the sink block changes.

## The problem: the warehouse lags behind and loses history

The analytics team wants the `orders` table in the warehouse, current within seconds, with
every change kept, not only the latest state:

- **A nightly export** is a day old and shows only the state at export time. A row that was
  created and deleted in between never appears.
- **Polling an `updated_at` column** needs that column on every write path and never sees a
  delete.
- **A managed ELT service** is often billed per row and runs on its schedule, not on commit.

Postgres logical replication reports every committed change, whoever made it, in commit order
and with a position (the LSN) that a reader can resume from.

## The solution: one route, three ecosystems

```mermaid
flowchart LR
  PG[(Postgres<br/>orders)] -- logical replication --> C
  subgraph B[one mq-bridge process]
    direction LR
    C[postgres_cdc<br/>native] --> M[connect_mapping<br/>Connect plugin] --> F[faucet sink<br/>faucet plugin]
  end
  F -- append --> W[(warehouse<br/>orders_changes)]
  F -. rejected rows .-> D[DLQ file]
```

The source table needs `wal_level = logical` and a publication
(`CREATE PUBLICATION orders_pub FOR TABLE orders;`). The route, as the example runs it:

```yaml
orders_to_warehouse:
  input:
    postgres_cdc:
      url: "postgres://app:app@postgres:5432/app"
      publication: "orders_pub"
      slot_name: "orders_to_warehouse"
      consume: capture_all                             # copy existing rows, then follow changes
      checkpoint_store: "file:///app/checkpoint.json"  # a restart does not repeat the copy
      source_metadata: true                            # a change's position in its transaction
    middlewares:
      - connect_mapping: |
          root = this.without("amount_cents")
          root.amount_eur = if this.amount_cents != null { this.amount_cents / 100 }
          root._op = metadata("postgres.operation").or("snapshot")
          root._lsn = metadata("postgres.lsn")
          root._ord = metadata("mqb.src.postgres_ordinal")
          root._synced_at = now()
  output:
    middlewares:
      - retry: { max_attempts: 5, initial_interval_ms: 200 }
      - dlq:                                           # after retry: gets what retry gave up on
          endpoint:
            file: { path: "/app/dlq.jsonl", format: json }
    custom:
      name: faucet
      config:
        connector: postgres
        flush: batch
        config:
          connection_url: "postgres://dw:dw@warehouse:5432/dw"
          table_name: orders_changes
          create_table: false
          column_mapping: auto_map
```

How it fits together:

- The [`postgres_cdc`](../connectors/postgres.md#postgresql-cdc) input with
  `consume: capture_all` reads the existing rows first, then the replication slot. The payload
  is the row as JSON; the operation and the LSN are in the `postgres.operation` and
  `postgres.lsn` metadata keys. Copied rows have no operation, so the mapping marks them
  `snapshot`. `checkpoint_store` remembers that the copy finished.
- [`connect_mapping`](../connectors/connect.md#processors-as-middleware) runs a Bloblang mapping
  in the same process: it converts cents to euros, drops the source column and adds the
  operation, the LSN, the change's position in its transaction and a load timestamp. A `transform` would cover the renames; Bloblang is
  there for logic such as the conditional, and for mappings a team already has.
- The faucet sink appends each row to `orders_changes`. `column_mapping: auto_map` writes each
  field to the column of the same name. `flush: batch` writes before every acknowledgement, so
  an acknowledged change is in the table; the default `flush: periodic` writes once a second
  and suits a warehouse better, see below.
- `retry` repeats a failed write; the [`dlq`](../cookbook/dlq.md) after it keeps a row the
  warehouse refuses and lets the route go on.

### Load into Snowflake instead

The input and the mapping stay as they are; the sink block becomes faucet's `snowflake`
connector, with key-pair authentication:

```yaml
    custom:
      name: faucet
      config:
        connector: snowflake
        config:
          account: "xy12345.eu-central-1"
          warehouse: LOAD_WH
          database: ANALYTICS
          schema: RAW
          table: ORDERS_CHANGES
          role: LOADER                                 # optional; else the user's default role
          auth:
            type: key_pair
            config:
              user: MQB_LOADER
              private_key_pem: "${SNOWFLAKE_PRIVATE_KEY}"
```

The fields are those of faucet's Snowflake sink. Without `create_table: false` it creates the
table on the first write, with every column as a nullable `STRING`; create the table yourself
for typed columns. The sink collects rows across batches and commits them at each flush, every
second with the default `flush: periodic`, and in between every 100,000 rows or about 256 MiB.
Leave `flush: batch` out here: one warehouse commit per small batch is slow and expensive. **This block was not run against Snowflake**; the
example runs the `postgres` sink, and faucet's README lists the connectors it has run.

The same input and mapping feed faucet's other sinks, such as `bigquery`, `redshift`,
`databricks`, `iceberg` or `delta`, or a native one: [ClickHouse](../connectors/clickhouse.md),
or Parquet files in object storage for [Snowflake's own `COPY INTO`](../cookbook/snowflake.md).

## Runnable example

```bash
git clone https://github.com/marcomq/mq-bridge && cd mq-bridge/examples/postgres-cdc-to-warehouse
docker compose up -d --build
docker compose exec warehouse psql -U dw -d dw -c "SELECT * FROM orders_changes"
```

The stack is two Postgres 16 instances (source and warehouse) and an image with mq-bridge-app
1.2.0 and the two plugins from their release archives. `./test.sh` checks the copy, an update,
a delete and an insert in commit order, two updates of one row in one transaction, a change made while the bridge was stopped, and a row
the warehouse rejects through a `CHECK` constraint; it runs in CI.

The example's README explains the two details of plugins in a container: the libraries must be
owned by root, and faucet needs a glibc from 2.39 on, newer than in the official image.

## Failure semantics

| Event | What happens |
| :--- | :--- |
| **mq-bridge restarts** | The replication slot keeps the position, so changes made while the bridge was down arrive after the restart. The copy is not repeated. |
| **Duplicates** | Delivery is [at-least-once](../engine/delivery.md): after a crash or a failed write, changes since the last confirmed position arrive again. A repeated row has the same `id`, `_lsn` and `_ord`. `_lsn` is the commit position, shared by every change of a transaction, so two changes to one `id` in one transaction differ only in `_ord`, their position in it: keep one row per `(id, _lsn, _ord)`. The copy has no LSN; a crash during the copy repeats it. |
| **Deletes** | A delete arrives as a row with `_op = delete` and only the key columns set. Set `REPLICA IDENTITY FULL` on the source table to get the whole old row. |
| **Warehouse unavailable** | `retry` repeats the write; after `max_attempts` the change goes to the DLQ, so a longer outage fills the DLQ instead of stopping the route. Raise `max_attempts` or leave the `dlq` out to make the route wait instead. |
| **A row the warehouse refuses** | When faucet reports the record as the cause, as for a constraint or type error, the write is not retried; the row goes to the DLQ file and the route continues. |
| **The bridge is down for a long time** | The slot keeps WAL on the source until the bridge confirms it, and the source's disk fills. Set `max_slot_wal_keep_size` on Postgres to cap it, at the price of a new copy once the slot is lost. |

### Exactly once

faucet's `exactly_once: true` commits each batch together with its highest LSN in the
warehouse and skips what is already there after a restart; the Snowflake and Postgres sinks
support it. It also needs `source_metadata: true` on the input and a `state_key`. It does
not combine with a DLQ: the position commits past a rejected row, so a crash before the DLQ
saw it would lose the row. Choose one, exactly-once writes, or a DLQ for rejected rows and
deduplication by `(id, _lsn, _ord)` in the warehouse, as the example does.

Limits of this recipe:

- **faucet is alpha.** Its README says the plugin has not been tested against real systems
  yet; test it against your warehouse before relying on it.
- **Schema changes are not followed.** A new source column appears in the payload. The
  Snowflake sink adds it as a nullable `STRING` column; with `create_table: false` add it to
  the table yourself.
- **The copy is not resumable.** A crash during `capture_all` starts the copy again. For a
  large table, copy it once with [`mqb copy`](../reference/cli.md) and follow with
  `consume: capture_new`.

## When not to use mq-bridge for this

- **The warehouse ingests Postgres itself**, for example Snowflake's Postgres connector or a
  managed replication feature of your warehouse. Then there is nothing to run.
- **Many tables with schema changes** to replicate one-to-one. A dedicated replication tool
  that maps types and follows `ALTER TABLE` does that with less configuration than one route
  per table.
- **The changes must reach many consumers.** Write them to Kafka (a
  [transactional outbox](transactional-outbox-postgres-kafka.md) or the CDC input with a
  `kafka` output) and load the warehouse from there.

## Alternatives

| | mq-bridge | Debezium + Kafka Connect | Redpanda Connect | Airbyte |
| :--- | :--- | :--- | :--- | :--- |
| Postgres changes | native `postgres_cdc`, existing rows first | yes | [`postgres_cdc` input](https://docs.redpanda.com/redpanda-connect/components/inputs/postgres_cdc/), under its enterprise license | yes, in scheduled syncs |
| Transform in flight | `transform`, Bloblang through the Connect plugin, Rust/Python handlers | single message transforms | Bloblang | after loading, in SQL |
| Snowflake | faucet sink, or Parquet for `COPY INTO` | Snowflake's Kafka connector | [`snowflake_put` output](https://docs.redpanda.com/redpanda-connect/components/outputs/snowflake_put/), under its enterprise license | yes |
| Runs as | one process, or a library in Rust, Python or Node.js | Kafka, Connect workers | one binary | a platform with scheduler and database |

mq-bridge's part here is the glue: a native source, a mapping in the language the team already
uses, a sink from another library, and retry and DLQ around all of it, in one process and one
YAML file.
