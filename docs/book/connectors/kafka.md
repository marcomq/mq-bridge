# Kafka

Produces to or consumes from a Kafka topic via `librdkafka`.

## URL format

```text
kafka://broker[:port]?topic=<topic>
```

`kafka://` is stripped before being handed to `librdkafka`'s
`bootstrap.servers`, which expects a bare host:port list, not a URI — the
scheme only selects the endpoint kind on the CLI. For a multi-broker
cluster, use the `?url=` escape hatch (see
[Quick Start](../quick-start.md#escape-hatch-driver-options-and-full-connection-strings)):
`kafka://_/?url=broker1:9092,broker2:9092&topic=orders`.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `kafka:`.

```yaml
input:
  kafka: { url: "localhost:9092", topic: "orders", group_id: "mqb-orders-sync" }
```

In config, `url` is the bare `host:port` bootstrap list (comma-separated for a cluster); `kafka://` is CLI-only.

## Examples

**Forward an MQTT stream into a Kafka topic, continuous:**

```bash
mqb copy \
  --from mqtt://broker.local:1883?topic=sensors/+/temperature \
  --to kafka://kafka.local:9092?topic=sensor-readings
```

**Consume with a durable consumer group, continuous:**

```bash
mqb copy \
  --from 'kafka://kafka.local:9092?topic=orders&group_id=mqb-orders-sync' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

Without `group_id`, the consumer runs in ephemeral **subscriber mode**
(unique group ID, starts from the latest offset).

**SASL-authenticated broker:**

```bash
mqb copy \
  --from postgres://user:pass@localhost/app?table=orders \
  --to 'kafka://kafka.local:9093?topic=orders&username=svc&password=secret'
```

TLS (an object-typed field, not a scalar) can't be set via a query param;
use the `?url=` escape hatch to pass a librdkafka connection string with TLS
options embedded.

## Key options

| Option | Purpose |
|---|---|
| `topic` | Topic to produce to or consume from. |
| `group_id` | Consumer group ID; omit for ephemeral subscriber mode. |
| `username` / `password` | SASL authentication. |
| `tls` | TLS configuration (object; not settable via a scalar query param — use `?url=` to pass driver-level TLS options). |
| `delayed_ack` | Publisher-only: don't wait for broker acknowledgement. |
| `create_topic` | Publisher-only: create the topic at start if it is missing (default `true`). The topic gets `partitions` partitions (default 6) and the cluster's `default.replication.factor`. Set `false` (URI: `?create_topic=false`) on a cluster where topics are managed elsewhere. |

Full field list: [reference/kafka.md](../reference/kafka.md).
