# MySQL, SQL Server & Oracle CDC via Debezium

mq-bridge captures changes natively from [PostgreSQL](../connectors/postgres.md#postgresql-cdc)
and [MongoDB](../connectors/mongodb.md). For MySQL, MariaDB, SQL Server, Oracle and Db2, run
[Debezium Server](https://debezium.io/documentation/reference/stable/operations/debezium-server.html)
next to it: Debezium reads the database's change log and hands each change to mq-bridge, which
routes, transforms and writes it like any other message.

Debezium Server is a standalone process. It needs no Kafka and no Kafka Connect.

> This recipe has not been run end to end. Verify the Debezium properties against the current
> Debezium Server documentation.

## Hand over by HTTP (no broker)

Debezium's HTTP sink POSTs one change per request, and mq-bridge's
[HTTP input](../connectors/http.md) receives it.

```properties
# Debezium Server: application.properties
debezium.source.connector.class=io.debezium.connector.mysql.MySqlConnector
debezium.source.database.hostname=localhost
debezium.source.database.port=3306
debezium.source.database.user=debezium
debezium.source.database.password=${MYSQL_PASSWORD}
debezium.source.database.server.id=184054
debezium.source.topic.prefix=shop
debezium.source.table.include.list=shop.orders
debezium.source.offset.storage.file.filename=data/offsets.dat
debezium.source.schema.history.internal=io.debezium.storage.file.history.FileSchemaHistory
debezium.source.schema.history.internal.file.filename=data/schema-history.dat

debezium.sink.type=http
debezium.sink.http.url=http://localhost:8080

# Plain JSON rows instead of the schema + before/after envelope
debezium.format.key.schemas.enable=false
debezium.format.value.schemas.enable=false
debezium.transforms=unwrap
debezium.transforms.unwrap.type=io.debezium.transforms.ExtractNewRecordState
debezium.transforms.unwrap.delete.tombstone.handling.mode=rewrite
```

```yaml
mysql_orders_to_postgres:
  input:
    http: { url: "127.0.0.1:8080" }
  output:
    sqlx:
      url: "postgres://user:pass@localhost:5432/warehouse"
      table: "orders"
      insert_query: >-
        INSERT INTO orders (id, status, amount)
        VALUES (${payload:id}, ${payload:status}, ${payload:amount})
        ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, amount = EXCLUDED.amount
```

- The `unwrap` transform makes each payload the row itself (`{"id": 7, "status": "open", …}`).
  Without it the row is under `after`, inside Debezium's change envelope.
- A transient write failure (connection, timeout) answers the request with an HTTP error and
  Debezium sends the change again, so delivery is at-least-once. The upsert makes the repeat
  harmless; see [Upserts & insert-if-absent](upserts.md). A permanent failure (e.g. a type
  error) is dropped and acknowledged like on any route, so Debezium moves on; add a
  [`dlq`](dlq.md) to keep it.
- Debezium keeps the log position in its offset file. Keep that file and the schema history on
  durable storage, or a restart re-reads the initial snapshot.
- For another database, change `connector.class` and the `database.*` properties. The
  mq-bridge route stays the same.

## Hand over through a broker

If Debezium already publishes to Kafka, or mq-bridge should be able to stop without holding
Debezium up, read the change topic instead. Debezium names it `<topic.prefix>.<database>.<table>`:

```yaml
input:
  kafka: { url: "localhost:9092", topic: "shop.shop.orders", group_id: "mqb-orders-sync" }
```

Debezium Server also sinks to NATS JetStream, Redis Streams, Pulsar and RabbitMQ, each of which
mq-bridge reads with its native connector.

## Deletes

With `delete.tombstone.handling.mode=rewrite`, a delete arrives as the last known row with
`"__deleted": "true"`; every other change carries `"__deleted": "false"`. The upsert above
would write a deleted row back, so send deletes to their own statement with a
[`switch`](switch.md), or drop them with a [`filter`](../engine/reference.md#filter).
