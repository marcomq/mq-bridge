# gRPC

Sends or receives messages over gRPC. As a **client** (default) it connects to
a remote server and issues `Publish` / `PublishBatch` RPCs; as a **server**
(`server_mode=true`) it starts an embedded tonic server that accepts those RPCs.
A client consumer can also call **any** service described by a compiled protobuf
descriptor, without generated Rust code — see [Calling an arbitrary
service](#calling-an-arbitrary-service).

## URL format

```text
grpc://host[:port]?topic=<topic>
```

`grpc://` (plain) and `grpcs://` (TLS) are both accepted. In client mode the URL
is the remote server (e.g. `grpc://localhost:50051`); in server mode it is the
bind address (e.g. `grpc://0.0.0.0:50051`).

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `grpc:`.

```yaml
input:
  grpc: { url: "0.0.0.0:50051", server_mode: true }
output:
  grpc: { url: "http://localhost:50051", topic: "orders" }
```

`grpc://` / `grpcs://` are CLI-only; in config the client `url` is `http://` / `https://`.

## Examples

**Forward a Kafka topic to a remote gRPC service, continuous:**

```bash
mqb copy \
  --from kafka://kafka.local:9092?topic=orders \
  --to grpc://orders-svc.local:50051?topic=orders
```

**Run an embedded gRPC server that ingests into Postgres, continuous:**

```bash
mqb copy \
  --from 'grpc://0.0.0.0:50051?server_mode=true&topic=orders' \
  --to 'postgres://user:pass@localhost/app?table=orders&auto_create_table=true'
```

## Calling an arbitrary service

Point the endpoint at a compiled `FileDescriptorSet` and name the service, the method and the
request as JSON. Responses are decoded dynamically and emitted using protobuf's canonical JSON
representation:

```yaml
input:
  grpc:
    url: https://grpc.example.com:443
    descriptor_set_path: proto/events.bin
    service_name: events.EventService
    method_name: Tail
    server_streaming: true
    request:
      topic: audit
```

Generate the descriptor with imports included:

```bash
protoc --descriptor_set_out=proto/events.bin --include_imports -I proto proto/events.proto
```

Unary and server-streaming methods are supported; client-streaming is rejected. A descriptor
describes a wire format but not an acknowledgement protocol, so a dynamic source has no ACK
operation and its delivery semantics are the remote API's own. Where route-level ACK/NACK and
at-least-once matter, use the built-in `mqbridge.Bridge` protocol (the default) — its ACK/NACK
are real RPCs, and unacknowledged messages are retained and redelivered to the same
`consumer_id` while the server process lives. The server keeps at most 1024 of them per
subscriber and closes a subscription that falls further behind; the client subscribes
again and is sent what was retained. Messages published while a subscriber is away are
not kept for it.

## Serving a unary method

A server-mode input with a descriptor, a service and a method also serves that one unary method.
Each call arrives as its request in canonical JSON, and a `response` output replies:

```yaml
input:
  grpc:
    url: 0.0.0.0:50051
    server_mode: true
    descriptor_set_path: proto/benchmark.bin
    service_name: benchmark.BenchmarkService
    method_name: GetSum
output:
  response: {}
```

The reply payload must be JSON matching the method's output message. Streaming methods are
rejected.

## Key options

| Option | Purpose |
|---|---|
| `server_mode` | Start an embedded gRPC server (receive) instead of connecting as a client. |
| `topic` | Topic / subject used for both subscribe and publish paths. |
| `timeout_ms` | Client: connection timeout and per-request deadline. Server: per-request deadline. |
| `consumer_id` | For the built-in `mqbridge.Bridge` protocol, the subscription identity for ACK tracking and redelivery. Defaults to a fresh id per consumer; set it to have unacknowledged messages redelivered after a reconnect. Dynamic services use the remote API's own semantics. |
| `descriptor_set_path` / `service_name` / `method_name` / `request` / `server_streaming` | Dynamic client mode, or the served unary method in server mode (above). |
| `max_decoding_message_size` / `max_encoding_message_size` | Max decoded / encoded message size (decode default 4 MiB, encode unlimited). |
| `http2_keepalive_interval_ms` / `http2_keepalive_timeout_ms` | HTTP/2 keepalive tuning, both modes. |
| `tls` | TLS configuration (object; set with a JSON literal `?tls={...}`). |

Full field list: [reference/grpc.md](../reference/grpc.md).
