# RabbitMQ (AMQP)

Publishes to or consumes from a RabbitMQ queue over the AMQP 0-9-1 protocol.

## URL format

```text
rabbitmq://[user:pass@]host[:port]/<vhost>?queue=<name>
```

`amqp://`/`amqps://` (the native AMQP scheme) and `rabbitmq://`/
`rabbitmqs://` (an alias) are both accepted; `rabbitmq(s)` is rewritten to
`amqp(s)` before being handed to the driver. The default vhost `/` must be
percent-encoded as `%2f` in the URL path, per the AMQP URI spec.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `amqp:`.

```yaml
input:
  amqp: { url: "amqp://localhost:5672/%2f", queue: "orders" }
```

The config key is `amqp`; `rabbitmq://` is a CLI-only alias for `amqp://`.

## Examples

**Consume a queue and forward each message to an HTTP endpoint, continuous:**

```bash
mqb copy \
  --from rabbitmq://guest:guest@localhost:5672/%2f?queue=orders \
  --to http://internal-api.local/ingest?method=POST
```

**Publish to an exchange instead of a default-exchange queue:**

```bash
mqb copy --drain \
  --from file:///data/events.jsonl?format=json \
  --to 'amqp://guest:guest@localhost:5672/%2f?exchange=events&queue=events'
```

The exchange and a binding for the routing key (the `queue` value) must exist: mq-bridge
declares the queue, not the exchange or the binding. A message the broker can route to no
queue fails the send; it is not dropped.

**Fan-out subscriber mode (ephemeral queue bound to the exchange):**

```bash
mqb copy \
  --from 'amqp://guest:guest@localhost:5672/%2f?exchange=events&subscribe_mode=true' \
  --to kafka://kafka.local:9092?topic=events
```

## Key options

| Option | Purpose |
|---|---|
| `queue` | Queue to consume from / publish to. |
| `exchange` | Exchange to publish to or bind the queue to. |
| `subscribe_mode` | Consumer-only: fan-out (ephemeral queue) instead of point-to-point. |
| `prefetch_count` | Consumer-only: messages to prefetch. Defaults to 100. |
| `no_persistence` | Non-durable queues / non-persistent messages. |
| `tls` | TLS configuration (object; set with a JSON literal `?tls={...}`). An `amqps://` URL uses it; `tls.required: true` makes an `amqp://` URL `amqps://` (port 5671 unless the URL names one). |

Full field list: [reference/rabbitmq.md](../reference/rabbitmq.md).
