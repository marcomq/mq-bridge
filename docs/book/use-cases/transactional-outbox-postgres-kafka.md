<!-- description: Implement the transactional outbox pattern with Postgres and Kafka: write events in the business transaction, relay them from the WAL with logical replication. Runnable docker-compose example. -->

# How to implement the transactional outbox pattern with Postgres and Kafka

The application inserts an event row into an `outbox` table in the same transaction as its
business change, and one mq-bridge route relays those inserts from the Postgres write-ahead log to
Kafka. Events are published if and only if the transaction committed; delivery is at-least-once.

Runnable example: [`examples/transactional-outbox`](https://github.com/marcomq/mq-bridge/tree/main/examples/transactional-outbox).

## The problem: the dual write

A service that updates its database and then publishes an event performs two writes that cannot be
committed together:

- **Commit, then publish.** If the process dies or the broker is unreachable between the two, the
  order exists and the event never does. Other services never hear about it.
- **Publish, then commit.** If the transaction rolls back, subscribers have acted on an order that
  does not exist.
- **Publish inside the transaction.** The broker call still succeeds or fails independently of the
  commit, and it holds the transaction open across a network call.

The [transactional outbox](https://microservices.io/patterns/data/transactional-outbox.html)
pattern replaces the second write with an insert into a table in the same database. A separate
relay publishes the rows. Reading them from the WAL instead of polling the table means no polling
query, no `published` flag to update and no lock contention with the application.

## The solution: relay the outbox table from the WAL

```mermaid
flowchart LR
  A[Application] -- "one transaction:<br/>orders + outbox" --> PG[(Postgres)]
  PG -- logical replication --> B[mq-bridge route]
  B -- "key = aggregate_id" --> K[[Kafka topic]]
  K --> C[Consumers<br/>dedupe on event id]
```

The outbox table and a publication that carries only its inserts (Postgres needs
`wal_level = logical`):

```sql
CREATE TABLE outbox (
  id           bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  aggregate_id text  NOT NULL,
  event_type   text  NOT NULL,
  payload      jsonb NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);

-- Only inserts are relayed, so the application can delete old outbox rows freely.
CREATE PUBLICATION outbox_pub FOR TABLE outbox WITH (publish = 'insert');
```

The application writes both rows in one transaction:

```sql
BEGIN;
INSERT INTO orders (sku, qty) VALUES ('book-42', 2);
INSERT INTO outbox (aggregate_id, event_type, payload)
VALUES ('order-1', 'order.placed', '{"order_id": 1, "sku": "book-42", "qty": 2}');
COMMIT;
```

The relay (`routes.yaml`):

```yaml
outbox_to_kafka:
  input:
    postgres_cdc:
      url: "postgres://app:app@postgres:5432/app"
      publication: "outbox_pub"   # publishes inserts on the outbox table only
      slot_name: "outbox_relay"
    middlewares:
      # Copies the aggregate id into the `mqb.id` metadata key ...
      - id: "${payload:aggregate_id}"
  output:
    middlewares:
      - retry: { max_attempts: 10 }
    kafka:
      url: "kafka:9092"
      topic: "order-events"
      partition_key: "mqb.id"     # ... so events of one aggregate share a partition, in order
```

How it fits together:

- [`postgres_cdc`](../connectors/postgres.md#postgresql-cdc) emits one message per inserted outbox
  row. The Kafka record value is that row as JSON, with the `jsonb` column as a nested object.
- The [`id` middleware](../engine/reference.md) puts the aggregate id into metadata, and the Kafka
  output's `partition_key` uses that metadata field as the record key.
- A rolled-back transaction never reaches the WAL's committed stream, so nothing is published.

For NATS, replace the output with a [`nats`](../connectors/nats.md) endpoint. The runnable example
and its test cover Kafka only.

### Run the relay from Rust

```toml
[dependencies]
mq-bridge = { version = "1", features = ["postgres-cdc", "kafka", "yaml"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mq_bridge::deploy_file("routes.yaml").await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

Running the relay inside the service that owns the outbox is possible, but then every replica of
the service would start a relay on the same replication slot, and a slot serves one reader. Run it
as one separate process or one designated instance.

### Run the relay from Python

```python
from mq_bridge import Route

Route.from_file("routes.yaml", "outbox_to_kafka").run()
```

### Run it without code

The example uses the `mq-bridge-app` container: `mq-bridge-app --config routes.yaml`.

## Runnable example

```bash
git clone https://github.com/marcomq/mq-bridge && cd mq-bridge/examples/transactional-outbox
docker compose up -d --wait
./test.sh
```

The stack is Postgres 16, Apache Kafka 3.9.0 and mq-bridge-app 0.4.18. `./test.sh` commits one
transaction, rolls one back, deletes the outbox rows, restarts the relay and checks what is on the
topic; it runs in CI. The example's README shows the same steps by hand.

## Failure semantics

| Event | What happens |
| :--- | :--- |
| **Transaction rolls back** | No event. |
| **Relay is down** | The application is unaffected: it only writes to Postgres. Events accumulate in the replication slot and are published after the relay restarts. |
| **Relay restarts** | It resumes from the slot's confirmed position. Events that were already published can be published again. With the 0.4.18 container, a plain `docker compose stop` and `start` republished earlier events in testing, so expect duplicates on every restart, not only after a crash. |
| **Duplicates** | Delivery is [at-least-once](../engine/delivery.md); mq-bridge cannot commit the source position and the Kafka write atomically. Consumers must be idempotent. The outbox row's `id` is in every record and identifies the event. |
| **Ordering** | Events with the same `aggregate_id` get the same Kafka key and therefore the same partition. There is no ordering across aggregates. |
| **Kafka unavailable** | `retry` repeats the send. A change is confirmed to Postgres only after Kafka accepted it, so nothing is skipped. |
| **Outbox rows deleted** | Not published: the publication carries inserts only. Delete rows on any schedule; the relay reads the WAL, not the table. |
| **Rows written before the relay first started** | Not published. The default mode, `capture_new`, sees changes made after the replication slot was created. Start the relay before the application writes events, or use `consume: capture_all` with a `checkpoint_store` to copy existing rows first. |
| **Relay stays down** | Postgres keeps the WAL the slot has not confirmed, and the disk fills eventually. Monitor the slot's lag; drop the slot if you retire the relay. |

## When not to use mq-bridge for this

- **You need exactly-once delivery to Kafka.** This relay is at-least-once. If consumers cannot
  deduplicate, this pattern with any CDC relay is the wrong tool.
- **You cannot enable logical replication.** Some managed databases or permission models rule out
  `wal_level = logical` or a replication role. A polling relay that selects unpublished rows works
  with plain SQL access.
- **The database is not Postgres.** mq-bridge reads the WAL of Postgres and the change streams of
  MongoDB. For MySQL, SQL Server or Oracle see [CDC via Debezium](../cookbook/debezium.md).
- **You want events routed to a topic per aggregate type from one config.** The example publishes
  every event to one topic. Debezium's outbox router does this routing by default.

## Alternatives

| | mq-bridge | Debezium outbox event router | Redpanda Connect |
| :--- | :--- | :--- | :--- |
| How it reads the outbox | `postgres_cdc` endpoint (logical replication) | Debezium Postgres connector plus the [Outbox Event Router](https://debezium.io/documentation/reference/stable/transformations/outbox-event-router.html) transformation | [`postgres_cdc` input](https://docs.redpanda.com/redpanda-connect/components/inputs/postgres_cdc/); its page states "This component requires an enterprise license" |
| Outbox conventions | None built in: you choose the columns and map the key with `id` and `partition_key` | Expects `id`, `aggregatetype`, `aggregateid` and `payload` columns by default; routes to a topic per aggregate type and sets the key | None built in |
| How it runs | Library in a Rust, Python or Node.js process, or one binary | A transformation inside Kafka Connect ("[most commonly](https://debezium.io/documentation/reference/stable/architecture.html)"); also Debezium Server | One binary, YAML config |
| Other databases | Postgres and MongoDB only | MySQL, SQL Server, Oracle, Db2 and more | See its component list |

### Why mq-bridge can be the better fit here

- **No Kafka Connect cluster.** The relay is one native binary or a library call in a Rust, Python
  or Node.js process. Debezium's outbox router is a transformation that runs inside Kafka Connect.
- **Your outbox schema, not a prescribed one.** Any columns work; the key is whichever field you
  name in the `id` middleware. Debezium's router expects its default column names unless
  reconfigured.
- **The broker is a config line.** Replacing the `kafka` output with `nats`, `amqp`, `mqtt` or
  `aws` relays the same outbox to another broker, with the same retry and dead-letter middleware.
- **Postgres CDC is part of the open-source core.** mq-bridge is licensed
  [MIT OR Apache-2.0](https://github.com/marcomq/mq-bridge/blob/main/LICENSE), CDC included.
  Redpanda Connect's `postgres_cdc` input states that it requires an enterprise license (linked
  above).
- **The consumers of the events can be tested without a broker**, with the same library; see
  [Test without a broker](test-without-a-broker.md).

Where the alternatives are the better fit: Debezium's router implements the outbox conventions
(topic per aggregate type, event id header, key) out of the box and supports more databases.
A polling publisher (a job that selects new outbox rows and marks them as sent) needs no
replication privileges and no extra component, at the cost of polling load, added latency and
write contention on the outbox table.

### Measured: memory of the relay

Relaying a backlog of 500,000 outbox rows to Kafka, mq-bridge-app peaked at 51 to 142 MiB of
container memory and Debezium Server at 562 to 603 MiB (two passes each, the same Kafka producer
settings, Docker Desktop on an Apple M1, 2026-10-05).

Other measurements of mq-bridge are on the
[benchmark dashboard](https://marcomq.github.io/mq-bridge/dev/bench/) and in the
[tuning page](../operations/tuning.md).
