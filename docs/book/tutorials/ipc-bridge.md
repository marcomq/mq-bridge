# Cross-process IPC bridge

The `memory` endpoint is more than an in-process channel: its `url` (alias `topic`)
is a transport URL, so the same endpoint can bridge **separate processes** on one
machine over a Unix socket or Windows named pipe — no broker required.

## Transport URLs

The `url` field selects the transport:

| URL | Meaning |
|---|---|
| `"name"` | `memory://name` — in-process, same process only |
| `"memory://name"` | in-process channel |
| `"ipc://name"` | Unix: `/run/mq-bridge/name.sock` (falls back to `$XDG_RUNTIME_DIR/mq-bridge`, then `/tmp/mq-bridge`). Windows: `\\.\pipe\mq-bridge-name` |
| `"ipc:///abs/path.sock"` | that exact socket path (Unix) |
| `"unix:///abs/path.sock"` | Unix only, path must be absolute |
| `"pipe://name"` | Windows only, `\\.\pipe\name` |

**The consumer side binds/listens and must be started before the publisher
connects.** IPC does not support `subscribe_mode` or `request_reply`. `enable_nack`
defaults to true, but redelivery is consumer-local: a nacked message is retried
inside the consumer and is lost if the consumer process dies.

## Example: ingest process → Kafka

One process reads from a Unix socket and forwards to Kafka. Start this consumer
**first** so the socket exists before any producer connects:

```yaml
ipc_ingest:
  input:
    memory:
      url: "ipc://orders"
      capacity: 256
  output:
    kafka:
      topic: "orders"
      url: "localhost:9092"
```

```bash
mqb --config ipc_ingest.yaml
```

The named form is used rather than an explicit path so no privileged directory has to exist
up front: `/run/mq-bridge/` needs root, and `ipc://orders` falls back to
`$XDG_RUNTIME_DIR/mq-bridge` or `/tmp/mq-bridge`. Give an absolute
`ipc:///abs/path.sock` only once you have created and permissioned that directory.

Any other process on the same host can now publish into
`ipc://orders` — via its own `mq-bridge` route, or programmatically
through a `Publisher` built on a `memory` endpoint (see the
[embedding tutorial](embedding.md)) — and the messages flow through to Kafka.

## When to use it

- **Decouple a producer and a sink into separate processes** without standing up a
  broker — e.g. an app writes to the socket, a long-lived bridge process owns the
  Kafka/DB connections and batching.
- **Language boundary.** A Python or Node process publishes into the socket; a Rust
  bridge process does the heavy I/O.

`capacity` bounds the in-flight channel; tune it alongside the guidance in
[Performance tuning](../operations/tuning.md).

## When not to use it

IPC is **at-most-once across a consumer crash**. A send is acknowledged once its bytes are
in the kernel socket buffer, so messages the consumer had not yet processed are lost when it
dies, and the publisher is never told. It also serves **one publisher at a time**: a second
one connects without an error and then waits until the first disconnects.

If the hand-over must survive a crash of either process, use a
[directory spool](../connectors/dir-spool.md) between the two processes, or a broker.

## See also

- [Configuration grammar](../engine/configuration.md) — this is Route 9 of the annotated example, with the full URL grammar in comments.
- [Endpoints (concepts)](../reference/endpoints.md#memory-endpoint-and-ipc).
