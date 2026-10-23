# Apache Pulsar

Pulsar is an input and an output. The endpoint lives in its own repository,
[mq-bridge-pulsar](https://github.com/marcomq/mq-bridge-pulsar), so the core library carries no
Pulsar dependency. `mqb` has it built in; the library and the bindings load it as a plugin.

## Configure

```yaml
pulsar_to_file:
  input:
    pulsar:
      url: "pulsar://localhost:6650"
      topic: "persistent://public/default/orders"
      subscription: "order-workers"
      initial_position: earliest
  output:
    file: { path: "orders.jsonl" }
```

| Field | Applies to | Default | Meaning |
|---|---|---|---|
| `url` | both | required | Broker URL, `pulsar://host:6650`. |
| `topic` | both | route name | Topic to read or write. |
| `subscription` | input | `mq-bridge-<route>` | Subscription name. Consumers use a shared subscription. |
| `initial_position` | input | `latest` | Where a **new** subscription starts: `latest` or `earliest`. |

From a URL:

```bash
mqb copy 'pulsar://localhost:6650?topic=orders&subscription=export&initial_position=earliest' \
  'file:///tmp/orders.jsonl'
```

## Behaviour to know

- **`initial_position` applies only when the subscription is created.** With `latest`, a topic's
  existing backlog is never delivered. An existing subscription resumes from its own cursor, so
  use a new `subscription` name to read a topic again from the start.
- **At-least-once.** A message is acknowledged, or negatively acknowledged, only after the route
  has processed its batch.
- **Publishing** enqueues the whole batch, flushes the producer once and waits for every receipt.

## Outside `mqb`

```bash
brew install marcomq/tap/mq-bridge-pulsar
conda install -c marcomq mq-bridge-pulsar
pip install mq-bridge mq-bridge-pulsar     # then: mq_bridge_pulsar.register()
npm install mq-bridge mq-bridge-pulsar     # then: import { register } from "mq-bridge-pulsar"
```

From Rust, add the `mq-bridge-pulsar` crate and call `mq_bridge_pulsar::register()?` before any
route starts; building it needs `protoc` on `PATH`. Do not pass the library to `mqb --plugin`:
the name is already registered there, see [Native plugins](../extending/plugins.md).
