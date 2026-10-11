# WebSocket

Acts as a WebSocket **server** (consumer — listens for connections) or
**client** (publisher — connects to a target URL). As a consumer it receives
frames; as a publisher it sends them.

## URL format

```text
# Consumer (listen):   ws://<bind-address>   or  wss://<bind-address>
# Publisher (connect):  ws://host[:port]/path  or  wss://host[:port]/path
```

A publisher takes `ws://` (plain) and `wss://` (TLS); its URL is the target server. For a
consumer the URL is the listen address (e.g. `ws://0.0.0.0:9000`).

## TLS and authentication

| Side | Setting | Effect |
|---|---|---|
| Consumer | `tls.required: true` with `tls.cert_file` and `tls.key_file` | Listens with TLS. On the command line a `wss://` source sets `tls.required`. |
| Consumer | `tls.ca_file` in addition | Only clients with a certificate signed by that CA are accepted (mutual TLS). |
| Publisher | a `wss://` URL | Connects with TLS and checks the server certificate and host name against the public roots. |
| Publisher | `tls.ca_file` | Trusts that CA instead of the public roots. |
| Publisher | `tls.cert_file` and `tls.key_file` | Presents a client certificate. |
| Consumer | `basic_auth: [user, password]` | An upgrade request without these credentials gets `401`. |
| Publisher | `basic_auth: [user, password]` | Sends them as the `Authorization` header of the upgrade request. |
| Consumer | `max_connections: N` | At most N connections at once; one more gets `503`. Unset means no limit. |

A publisher with `tls.required: true` and a `ws://` URL is refused when the route starts.

A listener without `basic_auth` and without `tls.ca_file` accepts every client, so bind it
to loopback or put a proxy in front. `basic_auth` without TLS sends the password in the clear.
A client has 10 seconds to complete the TLS handshake and send its upgrade request. Behind
TLS a connection over `max_connections` is closed before the handshake instead of getting a
`503`. On the command line: `?basic_auth=["user","password"]&max_connections=1000`.

With TLS a request for a path other than `path` is closed after the upgrade; without TLS it
is answered with `404`.

## Delivery

WebSocket has no acknowledgement. A publisher answers pings, drops frames the server sends
and connects again after the server closed the connection; a batch that is being written
at the moment the server goes away is lost. Connect, TLS and upgrade together may take
10 seconds. A send has no limit of its own: add the `timeout` middleware for a server that
may stop reading.

A listener closes its connections when its route stops. A client that does not read its
replies for 10 seconds is disconnected. A message the route rejects is not signalled to
the client; only a reply is. A route with a `websocket` input is therefore at-most-once, and
`required_delivery: at_least_once` refuses it.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `websocket:`.

```yaml
input:
  websocket: { url: "0.0.0.0:9000" }                # listen address
output:
  websocket: { url: "ws://localhost:9000/events" }  # target server
```

A TLS listener, and a publisher that trusts a private CA:

```yaml
input:
  websocket:
    url: "0.0.0.0:9443"
    tls: { required: true, cert_file: /etc/mqb/server.pem, key_file: /etc/mqb/server.key }
output:
  websocket:
    url: "wss://feed.internal:9443/events"
    tls: { ca_file: /etc/mqb/ca.pem }
```

## Examples

**Listen for WebSocket frames and forward them to Kafka, continuous:**

```bash
mqb copy \
  --from ws://0.0.0.0:9000 \
  --to kafka://kafka.local:9092?topic=ws-events
```

**Listen with TLS:**

```bash
mqb copy \
  --from 'wss://0.0.0.0:9443?tls={"cert_file":"/etc/mqb/server.pem","key_file":"/etc/mqb/server.key"}' \
  --to kafka://kafka.local:9092?topic=ws-events
```

**Only accept a specific path:**

```bash
mqb copy \
  --from ws://0.0.0.0:9000?path=/ingest \
  --to file:///data/ws.jsonl?format=json
```

**Push a stream to a remote WebSocket server, continuous:**

```bash
mqb copy \
  --from redis://localhost:6379?stream=events \
  --to wss://feed.example.com/socket
```

## Key options

| Option | Purpose |
|---|---|
| `path` | Consumer-only: only upgrade requests whose URI path matches exactly are delivered. |
| `message_id_header` | Consumer-only: handshake header to read the message ID from (default `message-id`). |
| `execution_mode` | Consumer-only: `auto` (default), `direct_only`, or `routed`. |
| `backlog` | Consumer-only: TCP listen backlog for the accept socket (default 4096). |
| `tls` | TLS configuration (object; set with a JSON literal `?tls={...}`). See [TLS and authentication](#tls-and-authentication). |
| `basic_auth` | `[user, password]`: required by a consumer, sent by a publisher. |
| `max_connections` | Consumer-only: most connections open at once (default: no limit). |
| `routed_queue_capacity` | Consumer-only: queue capacity for the routed adapter (default 100). |

Full field list: [reference/websocket.md](../reference/websocket.md).
