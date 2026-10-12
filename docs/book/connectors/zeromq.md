# ZeroMQ

Sends or receives messages over ZeroMQ sockets (PUSH/PULL, PUB/SUB, REQ/REP).
The `socket_type` selects the pattern and `bind` decides whether the endpoint
binds or connects.

## URL format

```text
zeromq://<transport>?socket_type=<type>[&bind=true]
```

`zeromq://` and `zmq://` are both accepted. The URL is a ZeroMQ transport
address such as `tcp://127.0.0.1:5555`. Choose `bind=true` on exactly one side
of a socket pair; the other connects.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `zeromq:`.

```yaml
input:
  zeromq: { url: "tcp://0.0.0.0:5555", socket_type: "pull", bind: true }
output:
  zeromq: { url: "tcp://localhost:5556", socket_type: "push" }
```

`zeromq://` / `zmq://` are CLI-only; in config `url` is the ZeroMQ transport address.

## Examples

**Pull from a PUSH producer and write to a file, continuous:**

```bash
mqb copy \
  --from 'zeromq://tcp://127.0.0.1:5555?socket_type=pull&bind=true' \
  --to file:///data/events.jsonl?format=json
```

**Publish a Kafka topic to a PUB socket, continuous:**

```bash
mqb copy \
  --from kafka://kafka.local:9092?topic=events \
  --to 'zeromq://tcp://0.0.0.0:5556?socket_type=pub&bind=true'
```

**Subscribe to a topic on a remote PUB socket, continuous:**

```bash
mqb copy \
  --from 'zeromq://tcp://feed.local:5556?socket_type=sub&topic=orders' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

## Key options

| Option | Purpose |
|---|---|
| `socket_type` | `push`, `pull`, `pub`, `sub`, `req`, or `rep`. |
| `bind` | Bind to the address instead of connecting (default `false`). |
| `topic` | `sub`: only messages whose topic starts with it are received. `pub`: the topic sent in front of each message. |
| `format` | Wire format: `raw_framed` (default — payload bytes with a JSON metadata frame in front), `raw` (payload bytes only, no metadata), `json` (wraps the whole message). |
| `backend` | `try_omq` (default — use `omq` when the `zeromq-omq` feature is compiled in, else `zmq`), or `zmq` / `omq` to require that backend. |

> **Wire format changed in 0.4.0.** `format` used to default to `json`; it is now
> `raw_framed`, which is binary-safe and still carries headers. A 0.4 peer and a 0.3 peer no
> longer understand each other on the same socket unless one of them uses `format=json` in the URL query.
> REQ/REP replies are the exception to `format` entirely: a REP peer always answers with a JSON
> array of canonical messages, and a REQ publisher always decodes one.

Both backends cover the whole socket set, REQ/REP included. `backend: omq` is the faster path
when the `zeromq-omq` feature is compiled in; naming `omq` or `zmq` explicitly makes that
backend a hard requirement rather than a preference. On `omq`, backpressure is applied by the
socket's high-water mark and `internal_buffer_size` is ignored.

A `pub` socket sends its `topic` as the first frame (an empty one with `raw_framed` when
no topic is set), and a `sub` socket drops that frame on receive. A foreign subscriber sees
`[topic, metadata, payload]` for `raw_framed`.

**Reconnecting.** A socket that binds is found again by its peers. A socket that connects
reconnects after its peer restarted, with one exception: on the `zmq` backend a `pull` or
`rep` socket that connects does not, and receives nothing more until the route is restarted.
Bind on that side, or use the `omq` backend. The route logs a warning at start when it is
configured that way.

Full field list: [reference/zeromq.md](../reference/zeromq.md).
