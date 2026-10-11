# IBM MQ

Puts to or gets from an IBM MQ **queue** (point-to-point) or **topic**
(publish/subscribe). Requires the IBM MQ client libraries and the optional
IBM MQ build — see [IBM MQ setup](../IBM_MQ_SETUP.md) for prerequisites.

## URL format

```text
ibmmq://host(port)?queue_manager=<QM>&channel=<CHANNEL>&queue=<QUEUE>
```

`ibmmq://` and `ibm-mq://` are both accepted. The host is given in IBM MQ's
`host(port)` form, and a comma-separated list provides failover
(`host1(1414),host2(1414)`). `queue_manager` and `channel` (the SVRCONN
channel) are always required; supply `queue` **or** `topic`.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `ibmmq:`.

```yaml
input:
  ibmmq:
    url: "localhost(1414)"
    queue_manager: "QM1"
    channel: "DEV.APP.SVRCONN"
    queue: "DEV.QUEUE.1"
```

## Examples

**Drain a queue into a file, one-shot:**

```bash
mqb copy --drain \
  --from 'ibmmq://mq.local(1414)?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=DEV.QUEUE.1&username=app&password=<password>' \
  --to file:///data/mq.jsonl?format=json
```

Credentials on the command line land in shell history and process listings. Keep them out
of the URL by interpolating an environment variable instead — see
[Secrets](../cookbook/secrets.md).

**Bridge a queue into Kafka, continuous:**

```bash
mqb copy \
  --from 'ibmmq://mq.local(1414)?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=DEV.QUEUE.1' \
  --to kafka://kafka.local:9092?topic=mq-events
```

**Subscribe to a topic (pub/sub), continuous:**

```bash
mqb copy \
  --from 'ibmmq://mq.local(1414)?queue_manager=QM1&channel=DEV.APP.SVRCONN&topic=orders/new' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

## Key options

| Option | Purpose |
|---|---|
| `queue_manager` | **Required.** Queue Manager name (e.g. `QM1`). |
| `channel` | **Required.** SVRCONN channel name defined on the QM. |
| `queue` | Queue for point-to-point (defaults to the route name if omitted). |
| `topic` | Topic string for pub/sub; enables subscriber mode on a consumer. |
| `username` / `password` | Authentication (required if the channel enforces it). |
| `max_message_size` | Consumer-only: largest message that can be read, in bytes (default 4 MiB). A larger one stops the route with an error and stays on the queue. |
| `wait_timeout_ms` | Consumer-only: polling timeout (default 1000 ms). |
| `tls` | TLS via the MQ-native key repository (object; set with a JSON literal `?tls={...}`). |

Full field list: [reference/ibmmq.md](../reference/ibmmq.md). Setup and
prerequisites: [IBM MQ setup](../IBM_MQ_SETUP.md).
