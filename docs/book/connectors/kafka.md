# Kafka

Reads from or writes to a Kafka topic, through `librdkafka`. It works with anything that
speaks the Kafka protocol: Apache Kafka, Redpanda, Confluent Platform.

## URL format

```text
kafka://broker[:port]?topic=<topic>[&group_id=<group>]
```

| What you want | URL |
| :--- | :--- |
| Write to a topic | `kafka://localhost:9092?topic=orders` |
| Read with a consumer group (resumes after a restart) | `kafka://localhost:9092?topic=orders&group_id=orders-sync` |
| Read only what arrives from now on | `kafka://localhost:9092?topic=orders` |
| Several bootstrap brokers | `kafka://_/?url=broker1:9092,broker2:9092&topic=orders` |
| Write to a topic that must already exist | `kafka://localhost:9092?topic=orders&create_topic=false` |
| Key records by a metadata field | `kafka://localhost:9092?topic=orders&partition_key=customer_id` |
| Pass a librdkafka option | `kafka://localhost:9092?topic=orders&producer_options=[["compression.type","gzip"]]` |
| Read a group from the newest record | `kafka://localhost:9092?topic=orders&group_id=g&consumer_options=[["auto.offset.reset","latest"]]` |
| TLS with a private CA | `kafka://broker:9093?topic=orders&tls={"required":true,"ca_file":"/etc/ssl/kafka-ca.pem"}` |
| User and password (SASL PLAIN over TLS) | `kafka://broker:9093?topic=orders&username=svc&password=secret` |

Rules that the table follows:

- **One broker is enough.** The client asks it for the rest of the cluster. A list of brokers
  is not a valid URL, so it goes in `url=`, with `_` as a placeholder host.
- **`tls`, `producer_options` and `consumer_options` take JSON.** Put the whole URL in single
  quotes so the shell leaves the brackets and quotes alone.
- **An unknown parameter is an error.** `kafka://host?topic=orders&foo=bar` does not start;
  librdkafka options go in `producer_options` or `consumer_options`.

## Config (YAML / library)

Every URL parameter is a field of the same name under `kafka:`. In a config file `url` is the
bare `host:port` list, without `kafka://`.

Read a topic with a consumer group and write it to PostgreSQL:

```yaml
orders_to_postgres:
  input:
    kafka:
      url: "broker1:9092,broker2:9092"
      topic: "orders"
      group_id: "orders-sync"
  output:
    sqlx:
      url: "postgres://app:secret@localhost/shop"
      table: "orders"
      columns: auto
      key: "id"
```

Write to a topic, keyed by customer, with eight partitions if the topic is new:

```yaml
webhooks_to_kafka:
  input:
    http: { url: "0.0.0.0:8080", path: "/orders", method: "POST" }
  output:
    kafka:
      url: "localhost:9092"
      topic: "orders"
      partition_key: "customer_id"   # a metadata field; see Writing
      partitions: 8
```

Copy one topic to another cluster, with retries and a dead-letter topic for what fails:

```yaml
mirror_orders:
  input:
    kafka: { url: "old-kafka:9092", topic: "orders", group_id: "mirror" }
  output:
    middlewares:
      - retry: { max_attempts: 5 }
      - dlq:
          endpoint:
            kafka: { url: "new-kafka:9092", topic: "orders.failed" }
    kafka: { url: "new-kafka:9092", topic: "orders" }
```

## Reading

Whether a restart continues where it stopped depends on `group_id`:

| | With `group_id` | Without `group_id` |
| :--- | :--- | :--- |
| Consumer group | the one you name | a new one on every start (`event-sub-<id>`) |
| First start | the oldest record still in the topic | only records that arrive from now on |
| After a restart | continues after the last acknowledged batch | starts at "now" again |
| Several instances | share the partitions between them | each gets every record |

- **Offsets are committed after the output accepted the batch**, never before. A crash in
  between delivers that batch again (at least once). Make the output idempotent or add
  [deduplication](../cookbook/deduplication.md) where a duplicate matters.
- **A record's headers become the message's metadata**, and the record value is the payload,
  byte for byte. Kafka knows no format; a JSON value stays JSON.
- **`--drain` ends at the end of the topic.** With `mqb copy --drain` and a `group_id`, the copy
  stops when every partition has been read to its end. A topic that does not exist counts as
  empty.
- **`source_metadata: true`** adds the topic, partition and offset of each record as
  `mqb.src.kafka_*` metadata.
- **`consumer_options` override the defaults.** mq-bridge sets `auto.offset.reset` (`earliest`
  with a group, `latest` without) and `enable.auto.commit=false`; leave the second one alone.

## Writing

- **The topic is created if it is missing**, with `partitions` partitions (default 6) and the
  cluster's default replication factor. Set `create_topic: false` where topics are managed
  elsewhere or the user may not create them. An existing topic is never changed.
- **The record key decides the partition.** By default the key is the message id, which
  spreads records evenly. With `partition_key: customer_id` the key is the value of the
  *metadata* field `customer_id`, so all records of one customer keep their order. To key by
  a payload field, copy it into metadata first with a [`transform`](../cookbook/transform.md).
- **Metadata is written as record headers**, together with the message id in
  `mq_bridge.message_id`.
- **Sent means acknowledged by all in-sync replicas.** The producer is idempotent (`acks=all`),
  compresses with lz4 and waits 1 ms to fill a batch. `delayed_ack: true` stops waiting for the
  broker: faster, and a crash can lose what was not yet confirmed.
- **`producer_options` override all of that.** With `acks` set to `0` or `1`, idempotence is
  switched off too, as librdkafka requires.

```yaml
output:
  kafka:
    url: "localhost:9092"
    topic: "clicks"
    producer_options:
      - ["compression.type", "gzip"]
      - ["linger.ms", "20"]
      - ["message.max.bytes", "5000000"]
```

## Authentication and TLS

```yaml
input:
  kafka:
    url: "broker.example.com:9093"
    topic: "orders"
    group_id: "orders-sync"
    username: "svc-orders"
    password: "${KAFKA_PASSWORD}"
    tls:
      required: true
      ca_file: "/etc/ssl/kafka-ca.pem"       # leave out for a public CA
      cert_file: "/etc/ssl/client.pem"       # client certificate (mutual TLS)
      key_file: "/etc/ssl/client.key"
```

| Setting | Effect |
| :--- | :--- |
| `tls.required: true` | Connect with TLS (`security.protocol=ssl`) |
| `tls.ca_file` | The CA that signed the broker certificate |
| `tls.cert_file`, `tls.key_file`, `tls.cert_password` | Client certificate for mutual TLS |
| `tls.accept_invalid_certs: true` | Do not verify the broker certificate. For tests only |
| `username` and `password` | SASL `PLAIN` over TLS (`sasl_ssl`). Setting both switches TLS on, also without `tls.required` |

This is what Confluent Cloud expects, with the API key as `username` and the API secret as
`password`. Another mechanism or protocol is set through the options, which are applied last:

```yaml
consumer_options:
  - ["sasl.mechanism", "SCRAM-SHA-512"]
```

Without `ca_file` the broker certificate is checked against the CA certificates of the
system.

> **Building it yourself?** TLS and SASL are in the prebuilt `mqb`, the Python wheels and the
> Node packages. In your own Rust build they need the Cargo feature `kafka-ssl` (part of
> `full`) in addition to `kafka`. Without it such a connection fails at start with
> `OpenSSL not available at build time`.

## Troubleshooting

| Message or symptom | Cause |
| :--- | :--- |
| `not a valid URI … invalid port number` | A broker list in the URL. Use `kafka://_/?url=b1:9092,b2:9092&topic=…` |
| `unrecognised query param 'x'` | Not a field of this connector. Use `producer_options` / `consumer_options` |
| `Kafka broker not reachable` | Wrong host or port, or a TLS port addressed without TLS |
| `OpenSSL not available at build time` | Built with `kafka` but without `kafka-ssl`. See [Authentication and TLS](#authentication-and-tls) |
| `libzstd not available at build time` | `compression.type=zstd` is not in this build; use `lz4` or `gzip` |
| The source reads nothing from an existing topic | No `group_id`: it only sees new records |
| Records arrive twice after a restart | The batch was written but not yet committed. See [Delivery guarantees](../engine/delivery.md) |

## Key options

| Option | Purpose |
|---|---|
| `topic` | Topic to produce to or consume from. |
| `group_id` | Consumer group ID; omit for ephemeral subscriber mode. |
| `username` / `password` | SASL authentication. |
| `tls` | TLS configuration (URL: a JSON literal, `tls={"required":true}`). |
| `partition_key` | Publisher-only: metadata field used as the record key. |
| `delayed_ack` | Publisher-only: don't wait for broker acknowledgement. |
| `create_topic` | Publisher-only: create the topic at start if it is missing (default `true`). The topic gets `partitions` partitions (default 6) and the cluster's `default.replication.factor`. Set `false` (URI: `?create_topic=false`) on a cluster where topics are managed elsewhere. |
| `producer_options` / `consumer_options` | librdkafka options as `[name, value]` pairs. |

Full field list: [reference/kafka.md](../reference/kafka.md).
