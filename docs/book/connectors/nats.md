# NATS

Publishes to or consumes from NATS subjects. By default it uses **JetStream**: messages are
stored in a stream and acknowledged. With `no_jetstream=true` it uses Core NATS, which keeps
nothing: a message goes to whoever is subscribed at that moment.

## URL format

```text
nats://[user:password@]host[:port]?subject=<subject>&stream=<stream>
```

| What you want | URL |
| :--- | :--- |
| Publish to a JetStream stream | `nats://localhost:4222?subject=orders.new&stream=orders` |
| Consume a stream, resuming after a restart | `nats://localhost:4222?subject=orders.new&stream=orders` |
| Consume every subject of the stream | `nats://localhost:4222?subject=orders.>&stream=orders` |
| Only messages that arrive from now on | `nats://localhost:4222?subject=orders.new&stream=orders&deliver_policy=new` |
| Every instance gets every message | `nats://localhost:4222?subject=orders.new&stream=orders&subscriber_mode=true` |
| Core NATS, no JetStream | `nats://localhost:4222?subject=orders.new&no_jetstream=true` |
| Request / reply | `nats://localhost:4222?subject=rpc.price&no_jetstream=true&request_reply=true` |
| Several servers of a cluster | `nats://_/?url=nats://n1:4222,nats://n2:4222&subject=orders.new&stream=orders` |
| User and password | `nats://app:secret@localhost:4222?subject=orders.new&stream=orders` |
| Token | `nats://localhost:4222?subject=orders.new&stream=orders&token=s3cr3t` |
| TLS with a private CA | `nats://nats.example.com:4222?subject=orders.new&stream=orders&tls={"required":true,"ca_file":"/etc/ssl/nats-ca.pem"}` |

Rules that the table follows:

- **`stream` is required with JetStream**, for an input and an output. Without it the route
  stops with `stream must be provided when JetStream is enabled`.
- **Name the subject after the stream.** A stream that mq-bridge creates for an output takes
  the subjects `<stream>.>`, so `subject=orders.new` fits `stream=orders`, and `subject=new`
  does not. See [Streams](#streams).
- **One server is enough.** The client learns the other members of a cluster from the server
  it connects to. To name several, so that the first connection does not depend on one of
  them, put a comma-separated list in `url=`, with `_` as a placeholder host.
- **`tls` takes JSON**, so put the URL in single quotes. An unknown parameter is an error.

## Config (YAML / library)

Every URL parameter is a field of the same name under `nats:`.

Take orders in over HTTP and publish them to JetStream:

```yaml
orders_in:
  input:
    http: { url: "0.0.0.0:8080", path: "/orders" }
  output:
    nats:
      url: "nats://localhost:4222"
      subject: "orders.new"
      stream: "orders"
      deduplicate: true
```

Consume them into PostgreSQL:

```yaml
orders_to_postgres:
  input:
    nats:
      url: "nats://localhost:4222"
      subject: "orders.new"
      stream: "orders"
  output:
    sqlx:
      url: "postgres://app:secret@localhost/shop"
      table: "orders"
      columns: auto
```

With credentials and TLS:

```yaml
input:
  nats:
    url: "nats://nats.example.com:4222"
    subject: "orders.>"
    stream: "orders"
    username: "app"
    password: "${NATS_PASSWORD}"
    tls: { required: true, ca_file: "/etc/ssl/nats-ca.pem" }
```

For a cluster, `url` takes a comma-separated list: `"nats://n1:4222,nats://n2:4222"`. Use
`username` and `password`, or `token`. With `tls.required: true` the connection is TLS or
fails; without `ca_file` the public CAs are trusted. `cert_file` and `key_file` add a client
certificate.

## Streams

JetStream stores messages in a *stream*, and a stream lists the subjects it stores.

- **An existing stream is used as it is.** mq-bridge does not change its subjects or limits.
  Then any subject the stream stores can be used.
- **An output creates a missing stream** with the subjects `<stream>.>`: everything below the
  stream's name. With a wildcard in `subject` it takes that subject instead. The new stream
  keeps at most `stream_max_messages` messages (default 1,000,000) and `stream_max_bytes`
  bytes (default 1 GiB).
- **An input creates a missing stream** with exactly its `subject`.

For streams with other settings (replicas, retention, a work-queue policy), create them with
the `nats` command line first:

```sh
nats stream add orders --subjects 'orders.>' --storage file --replicas 3 --defaults
```

## Reading

| | Default | `subscriber_mode: true` |
| :--- | :--- | :--- |
| JetStream consumer | durable, named after stream and subject | not durable |
| After a restart | continues after the last acknowledged message | starts again by `deliver_policy` |
| Several instances | share the messages | each gets every message |

- **`deliver_policy`** decides where a new consumer starts: `all` (default, the oldest stored
  message), `new` (only what arrives from now on), `last`, or `last_per_subject`. A durable
  consumer that already exists keeps its position.
- **A message is acknowledged when the output has accepted it.** One that failed is handed
  back to JetStream and delivered again.
- **`prefetch_count`** is the number of messages that may be unacknowledged at a time (default
  10000). Lower it when messages are large.
- **Headers are metadata.** `source_metadata: true` adds the stream sequence as
  `mqb.src.nats_*`.
- **The durable consumer's name** is built from stream and subject, with `*` written `_any_`
  and `>` written `_all_`. Two routes with the same stream and subject therefore share one
  consumer and split the messages.
- **A `--drain` right after a crash waits for redelivery.** What the crashed run held comes
  back after `ack_wait` (default 30 s), so the drain ends only once those messages are copied.

## Writing

- **JetStream waits for the server's acknowledgement** of every message. `delayed_ack: true`
  does not wait: faster, and a crash can lose what was not yet stored.
- **`deduplicate: true`** sends the message id as `Nats-Msg-Id`. JetStream then drops a
  message it has seen within the stream's duplicate window, so a retry does not store it
  twice.
- **Metadata is sent as headers.**
- **Core NATS confirms nothing.** With `no_jetstream: true` a message published while nobody
  is subscribed is gone.

## Request / reply

With `request_reply: true` the output sends each message as a NATS request and waits up to
`request_timeout_ms` (default 30000) for the reply. This route is an HTTP front for a NATS
service:

```yaml
price_api:
  input:
    http: { url: "0.0.0.0:8080", path: "/price" }
  output:
    request:
      to:
        nats:
          url: "nats://localhost:4222"
          subject: "rpc.price"
          no_jetstream: true
          request_reply: true
          request_timeout_ms: 2000
      forward_to:
        response: {}
```

And this one is such a service: it answers every request on `rpc.echo` with its payload.

```yaml
echo_service:
  input:
    nats: { url: "nats://localhost:4222", subject: "rpc.echo", stream: "rpc", no_jetstream: true }
  output:
    response: {}
```

An input needs `stream` even with `no_jetstream: true`; the value is checked and not used.
A JetStream consumer sends a reply to the subject in the message's `reply_to` header; a
Core NATS consumer to the native reply subject.

## Examples

**Load a JSONL file into a JetStream subject, one-shot:**

```bash
mqb copy --drain \
  --from 'file:///data/orders.jsonl?format=raw' \
  --to 'nats://localhost:4222?subject=orders.new&stream=orders'
```

**Consume a durable JetStream stream into Postgres, continuous:**

```bash
mqb copy \
  --from 'nats://localhost:4222?subject=orders.new&stream=orders' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

**HTTP in front of a Core NATS service, continuous:**

```bash
mqb copy \
  --from 'http://0.0.0.0:8080?path=/rpc' \
  --to 'nats://localhost:4222?subject=rpc.echo&no_jetstream=true&request_reply=true'
```

## Key options

| Option | Purpose |
|---|---|
| `subject` | Subject to publish to or subscribe to. |
| `stream` | JetStream stream name. **Required** unless a publisher sets `no_jetstream`. |
| `no_jetstream` | Use Core NATS (fire-and-forget) instead of JetStream. |
| `deliver_policy` | Consumer-only: `all` (default), `last`, `new`, `last_per_subject`. |
| `subscriber_mode` | Consumer-only: ephemeral subscriber instead of a durable consumer. |
| `request_reply` + `request_timeout_ms` | Publisher-only request/reply pattern. |
| `deduplicate` | Publisher-only (JetStream): send `Nats-Msg-Id` so JetStream dedupes redeliveries. |
| `username` / `password` / `token` | Authentication. |
| `tls` | TLS configuration (object; set with a JSON literal `?tls={...}`). With `required: true` the connection is TLS or fails; without `ca_file` the public CAs are trusted. |

Full field list: [reference/nats.md](../reference/nats.md).
