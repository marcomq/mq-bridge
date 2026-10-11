# MQTT

Publishes to or subscribes from an MQTT broker (v5 by default, v3 supported).

## URL format

```text
mqtt://[user:pass@]host[:port]?topic=<topic>
```

`mqtt://` is rewritten to `tcp://` (and `mqtts://` to `ssl://`) before being
handed to the MQTT client — the scheme only selects the endpoint kind on the
CLI. MQTT topic wildcards (`+`, `#`) are supported on the source side.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `mqtt:`.

```yaml
input:
  mqtt: { url: "mqtt://localhost:1883", topic: "sensors/+/temperature" }
```

## Examples

**Subscribe to a wildcard topic and forward to Kafka, continuous:**

```bash
mqb copy \
  --from mqtt://broker.local:1883?topic=sensors/+/temperature \
  --to kafka://kafka.local:9092?topic=sensor-readings
```

**Publish a file's lines to a topic, one-shot:**

```bash
mqb copy --drain \
  --from file:///data/events.jsonl?format=json \
  --to mqtts://user:pass@broker.local:8883?topic=events
```

**Fixed client ID and QoS 2 for exactly-once delivery semantics:**

```bash
mqb copy \
  --from 'mqtt://broker.local:1883?topic=alerts&client_id=mqb-alerts-01&qos=2' \
  --to null:
```

## Key options

| Option | Purpose |
|---|---|
| `topic` | MQTT topic (wildcards on the source side). |
| `client_id` | Fixed client ID. Without it a route's consumer is `mq-bridge-<route>` (persistent session) and its publisher `mq-bridge-<route>-out`. |
| `qos` | Quality of Service (0, 1, or 2). Defaults to 1. |
| `protocol` | `V3` or `V5`. Defaults to `V5`. |
| `delayed_ack` | Consumer-only: ack after processing instead of on receipt (default). |

A source can lose a few in-flight messages when the broker restarts abruptly: MQTT guarantees
QoS 1/2 redelivery only across a session that survives, and a message the broker dropped never
reaches the consumer to be retried. The sink is not affected; it re-publishes until confirmed.

Full field list: [reference/mqtt.md](../reference/mqtt.md).
