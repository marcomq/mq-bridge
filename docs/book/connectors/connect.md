# Connect plugin (Redpanda Connect components)

The [Connect plugin](https://github.com/marcomq/mq-bridge-connect) runs Redpanda Connect
components inside an mq-bridge route. Its inputs and outputs become the `connect` endpoint, and
its processors become middleware that also works on native endpoints. mq-bridge keeps routing,
batching, retries, DLQ and deduplication.

Every component the plugin links is listed with its fields:

- [Inputs](connect-inputs.md)
- [Outputs](connect-outputs.md)
- [Processors](connect-processors.md)

Prefer a native [connector](README.md) where one exists: it needs no plugin and avoids the hop
into Go. Reach for Connect for systems mq-bridge has no endpoint for, such as Elasticsearch,
OpenSearch, Cassandra, Google Pub/Sub, BigQuery or Azure storage, and for Bloblang mappings.

## Install

The plugin is a separate install and is not part of `mqb`.

```bash
brew install marcomq/tap/mq-bridge-connect
conda install -c marcomq mq-bridge-connect
```

Either puts the libraries where mq-bridge discovers them. For the bindings, install the
language package and call its `register()` before starting routes:

```bash
pip install mq-bridge mq-bridge-connect
npm install mq-bridge mq-bridge-connect
```

From Rust, `cargo add mq-bridge mq-bridge-connect` and use `ConnectFactory`. The
[plugin README](https://github.com/marcomq/mq-bridge-connect#install) covers Docker, manual
installs and offline builds.

## Configure an endpoint

Name the component with `connector`; every other field is that component's own configuration.

```yaml
mqtt_to_elasticsearch:
  input:
    connect:
      connector: mqtt
      urls: ["tcp://localhost:1883"]
      topics: ["orders"]
  output:
    middlewares:
      - retry: { max_attempts: 3 }
    connect:
      connector: elasticsearch_v8
      urls: ["http://localhost:9200"]
      index: "orders"
      action: "index"
      id: '${! json("id") }'
```

The long form `custom: { name: connect, config: {...} }` is the same endpoint. Use it if you
validate configs against `mq-bridge.schema.json`, which does not know the `connect` key.

A component often names the same thing differently per direction. Put such fields in an `input`
or `output` block inside the `connect` block; the block matching the endpoint is merged in and the other is
dropped, so one configuration can describe both ends.

### From a URL

`mqb copy` takes the component name after `connect+`. A `_` in the name is written `-`:

```bash
mqb copy 'connect+mqtt://localhost:1883/orders' 'file:///tmp/out.jsonl'
mqb copy 'connect+amqp-0-9://guest:guest@localhost:5672/orders' 'file:///tmp/out.jsonl'
```

For `mqtt`, `amqp_0_9`, `amqp_1`, `nats`, `nats_jetstream`, `pulsar`, `redis_streams`,
`redis_pubsub` and `redis_list`, the authority is the broker address and the path is the topic,
queue or subject. Every other field is a query parameter under its own name.

### A full Redpanda Connect document

If you already have a Redpanda Connect config, pass it as `yaml`, minus the end mq-bridge owns.
This form also takes a `pipeline` of processors and resource sections:

```yaml
output:
  connect:
    yaml: |
      pipeline:
        processors:
          - mapping: 'root = this.without("internal")'
      output:
        file:
          path: /tmp/orders.jsonl
```

## Processors as middleware

Some processors are exported as their own middleware, `connect_<processor>`, and run on any
endpoint. The [processor list](connect-processors.md) marks which.
They need plugin 0.1.1 or newer; 0.1.0 provides the endpoint only.

```yaml
input:
  kafka: { url: "localhost:9092", topic: "orders" }
  middlewares:
    - connect_mapping: 'root = this.merge({"received_at": now()})'
```

To run several processors in one call, or to share a cache between them, use the `connect`
middleware:

```yaml
middlewares:
  - connect:
      processors:
        - mapping: 'root = this.payload'
        - dedupe: { cache: seen, key: '${! json("id") }' }
      cache_resources:
        - { label: seen, memory: { default_ttl: 5m } }
```

It accepts any processor that keeps, rewrites or drops each message. One that splits or merges
messages (`unarchive`, `split`, `archive`, `group_by`) belongs in an endpoint's `pipeline`.

The middlewares are registered when the plugin loads. A route that uses them without a `connect`
endpoint needs the plugin loaded up front, as `mqb`'s `plugins:` list does.

## Behaviour to know

- **At-least-once.** A nacked message is redelivered by the source; an uncommitted batch is
  nacked when the stream closes.
- **Ordering.** Up to `max_in_flight` (default 64) source batches are in flight at once and have
  no order between them. Set `max_in_flight: 1` where order matters.
- **`publish_timeout`** (default `30s`, outputs) bounds a send. When it elapses the send fails as
  retryable, so `retry` and `dlq` take over; the timed-out batch may still arrive later.
- **No replies.** Request/reply is not supported through a `connect` endpoint.
- **Secrets.** The app's `balanced` config security mode moves credentials out of a `connect`
  config as it does for native endpoints; see
  [Secrets](../cookbook/secrets.md#custom-endpoints-and-plugins). It cannot see inside a `yaml`
  document, so reference secrets there from the environment or a file.

Details are in the plugin's [Semantics](https://github.com/marcomq/mq-bridge-connect#semantics).

## What is not included

Only Redpanda Connect packages that reach no Redpanda Community License code are linked, so the
bundled code is Apache-2.0 and MIT. That excludes, among others, Redpanda Connect's `kafka`,
`aws`, `snowflake` and `redpanda` packages. Use mq-bridge's native [Kafka](kafka.md),
[AWS](aws.md) and [object storage](object-store.md) connectors for those, and
[Parquet on object storage](../cookbook/snowflake.md) for Snowflake. See the plugin's
[licensing notes](https://github.com/marcomq/mq-bridge-connect/blob/main/docs/LICENSING.md).
