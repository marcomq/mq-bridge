# NATS

Publishes to or consumes from NATS subjects. Uses **JetStream** by default
(durable, acked); set `no_jetstream=true` for fire-and-forget Core NATS.

## URL format

```text
nats://host[:port]?subject=<subject>[&stream=<name>]
```

For a multi-server cluster, pass a comma-separated list via the `?url=` escape
hatch: `nats://_/?url=nats://n1:4222,nats://n2:4222&subject=orders`. Consumers
require `stream` (the JetStream stream name), even with `no_jetstream=true`
(validated but unused there). If the bridge auto-creates a stream it is scoped
to `{stream}.>`, so prefix your subject accordingly.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `nats:`.

```yaml
input:
  nats: { url: "nats://localhost:4222", subject: "orders.new", stream: "orders" }
```

For a cluster, `url` takes a comma-separated list of servers.

## Examples

**Load a JSONL file into a JetStream subject, one-shot:**

```bash
mqb copy --drain \
  --from file:///data/orders.jsonl?format=json \
  --to 'nats://localhost:4222?subject=orders&stream=ORDERS'
```

**Consume a durable JetStream stream into Postgres, continuous:**

```bash
mqb copy \
  --from 'nats://localhost:4222?subject=orders&stream=ORDERS' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

**Core NATS request/reply (no JetStream), continuous:**

```bash
mqb copy \
  --from http://0.0.0.0:8080/rpc \
  --to 'nats://localhost:4222?subject=rpc.echo&no_jetstream=true&request_reply=true'
```

## Key options

| Option | Purpose |
|---|---|
| `subject` | Subject to publish to or subscribe to. |
| `stream` | JetStream stream name. **Required for consumers.** |
| `no_jetstream` | Use Core NATS (fire-and-forget) instead of JetStream. |
| `deliver_policy` | Consumer-only: `all` (default), `last`, `new`, `last_per_subject`. |
| `subscriber_mode` | Consumer-only: ephemeral subscriber instead of a durable consumer. |
| `request_reply` + `request_timeout_ms` | Publisher-only request/reply pattern. |
| `deduplicate` | Publisher-only (JetStream): send `Nats-Msg-Id` so JetStream dedupes redeliveries. |
| `username` / `password` / `token` | Authentication. |
| `tls` | TLS configuration (object; set with a JSON literal `?tls={...}`). With `required: true` the connection is TLS or fails; without `ca_file` the public CAs are trusted. |

A JetStream consumer sends a reply to the subject in the message's `reply_to` header; a
Core NATS consumer to the native reply subject. A durable consumer is named after stream and
subject, with `*` written `_any_` and `>` `_all_`.

Full field list: [reference/nats.md](../reference/nats.md).
