# gRPC integration

mq-bridge exposes two deliberately separate gRPC capabilities: the `mqbridge.Bridge` protocol and
descriptor-driven dynamic calls. A server-mode endpoint can additionally serve one
descriptor-defined **unary** method per route; streaming server methods are not implemented.

## Bridge protocol

[`src/endpoints/grpc/proto/mqbridge/bridge.proto`](../src/endpoints/grpc/proto/mqbridge/bridge.proto) is the
stable public API for external applications. Its package is `mqbridge`; generated Rust types remain
available from `mq_bridge::endpoints::grpc::proto`. The source `.proto` and this documentation are
included in the published crate.

Compatibility rules for `mqbridge`:

- Never reuse a field number or enum value.
- Reserve fields and enum values when removing them.
- Make only backward-compatible additions to this package.
- Put breaking changes in a new package, `mqbridge.v2`, leaving `mqbridge` in place so existing
  clients keep working and can migrate on their own schedule.

The package deliberately carries no version suffix: `mqbridge` is the contract mq-bridge has always
shipped, and renaming it would mean serving both names forever for no gain.

Generate a client from the checked-in contract (the exact plugin installation mechanism is owned
by each language ecosystem):

```bash
# Rust (build.rs)
tonic_prost_build::compile_protos("src/endpoints/grpc/proto/mqbridge/bridge.proto")?;

# Python
python -m grpc_tools.protoc -I src/endpoints/grpc/proto --python_out=. --grpc_python_out=. src/endpoints/grpc/proto/mqbridge/bridge.proto

# Go
protoc -I src/endpoints/grpc/proto --go_out=. --go-grpc_out=. src/endpoints/grpc/proto/mqbridge/bridge.proto

# TypeScript (example using grpc-tools + ts-proto)
protoc -I src/endpoints/grpc/proto --plugin=protoc-gen-ts_proto --ts_proto_out=. src/endpoints/grpc/proto/mqbridge/bridge.proto

# Java (use protoc-gen-grpc-java from the grpc-java release)
protoc -I src/endpoints/grpc/proto --java_out=. --grpc-java_out=. src/endpoints/grpc/proto/mqbridge/bridge.proto
```

Generated SDKs may be published as separate language packages; consumers should not copy private
mq-bridge implementation types.

### External producer to mq-bridge

Configure a Bridge-only inbound server:

```yaml
input:
  grpc:
    url: 0.0.0.0:50051
    server_mode: true
    topic: orders
```

An external generated client calls `mqbridge.Bridge/Publish` or `PublishBatch`. A successful
publish response means mq-bridge's downstream route returned its disposition; it does not by
itself prove a durable commit in a later external system.

### mq-bridge to an external subscriber

Configure the external process as a Bridge server and mq-bridge as a client:

```yaml
output:
  grpc:
    url: http://worker:50051
    topic: orders
```

For the reverse flow, an external generated client calls `Subscribe` with a stable `consumer_id`,
then calls `Acknowledge` for each delivered message. ACK, NACK, and REQUEUE belong only to this
versioned Bridge protocol. The embedded server's replay state is bounded and in memory; process
restart loses it, and retention limits can evict old entries. It is not durable broker storage.

## Dynamic client source

A dynamic source invokes an existing arbitrary unary or server-streaming service. RPC shape comes
from the descriptor; `server_streaming` is a deprecated compatibility hint and is ignored.
Client-streaming and bidirectional-streaming methods fail construction with a capability error that
names the unsupported shape and the supported alternatives.

Descriptors can be supplied by `descriptor_set_path`, by `descriptor_set_bytes` through the Rust
API, or discovered with reflection v1. This server-streaming source calls `Tail`:

```yaml
input:
  grpc:
    url: https://events.example.com:443
    reflection: true
    service_name: events.v1.EventService
    method_name: Tail
    request:
      topic: audit
    connect_timeout_ms: 3000
    request_timeout_ms: 5000
    idle_stream_timeout_ms: 30000
    overall_timeout_ms: 3600000
    bearer_token: ${EVENTS_TOKEN}
    metadata:
      x-tenant: accounting
```

All deadlines default to disabled. The deprecated `timeout_ms` key is still accepted as a fallback
for `connect_timeout_ms` and `request_timeout_ms`. It no longer imposes an idle or overall limit on
a dynamic response stream; configure `idle_stream_timeout_ms` or `overall_timeout_ms` explicitly
when those limits are wanted. A Bridge publisher's overall batch timeout also requires
`overall_timeout_ms` explicitly.

`idle_stream_timeout_ms` is retryable: exceeding it drops the stream and the route reconnects.
`overall_timeout_ms` caps the lifetime of the RPC and is terminal — exceeding it stops the route
rather than restarting the call, which would reset the cap on every reconnect.

## Dynamic client sink

The same descriptor configuration on a route's **output** calls a method instead of reading
one. Shape again comes from the descriptor:

- **unary** (`A -> B`) — one call per message; the reply is returned as that message's response,
  so it feeds request/reply and the structural `request` endpoint.
- **client-streaming** (`stream A -> B`) — one call per batch: every message in the batch is
  streamed into a single RPC, which returns one reply.
- **server-streaming** and **bidirectional** methods are rejected: they produce a stream of
  responses, which is a source. Use them as the route's input.

`request` is rejected on an output — the published messages *are* the requests. Each message
payload must be canonical protobuf JSON for the method's input type; a payload that does not
match the descriptor fails permanently (it is a poison message, not a transient fault) and is
reported per message without affecting the rest of the batch.

```yaml
output:
  grpc:
    url: https://catalog.example.com:443
    reflection: true
    service_name: catalog.v1.Catalog
    method_name: PutItem
    request_timeout_ms: 5000
    bearer_token: ${CATALOG_TOKEN}
```

gRPC status codes are classified: `INVALID_ARGUMENT`, `NOT_FOUND`, `ALREADY_EXISTS`,
`PERMISSION_DENIED`, `UNAUTHENTICATED`, `FAILED_PRECONDITION`, `OUT_OF_RANGE`, and
`UNIMPLEMENTED` are non-retryable, so the route dead-letters instead of replaying a request the
server has already refused. Everything else is retryable.

### Acknowledgement granularity on a client-streaming sink

A unary sink acknowledges **per message**: one message, one call, one outcome.

A client-streaming sink acknowledges **per batch**. One reply covers the whole stream, and a
failure part-way through cannot say which messages the server already consumed, so every message
in the batch is failed and a retry redelivers all of them. That is at-least-once with a
batch-sized blast radius: size the route's `batch_size` accordingly, and prefer a unary method
when the target offers one.

## Unary source

An arbitrary unary source uses the same shape; select its unary method and request:

```yaml
input:
  grpc:
    url: https://catalog.example.com:443
    descriptor_set_path: proto/catalog.bin
    service_name: catalog.v1.Catalog
    method_name: GetItem
    request:
      item_id: sku-123
    request_timeout_ms: 5000
```

## Metadata and credentials

Dynamic descriptor-driven calls support four metadata and credential settings:

- `metadata`: static ASCII metadata values.
- `binary_metadata`: raw byte values for embedded Rust callers; keys must use the `-bin` suffix.
- `bearer_token`: a bearer credential sent in the `authorization` metadata entry.
- `api_key` and optional `api_key_name`: an API key and its metadata name (default `x-api-key`).

`bearer_token` and `api_key` require an `https://` URL; a credential offered over plaintext h2c is
rejected rather than sent in the clear. Authentication values are validated without being included
in errors, endpoint status, logs, or connection-cache identities. All four settings are also sent on the reflection call, so a server
that guards reflection sees the same credentials.

These four settings apply **only** to dynamic descriptor-driven calls. Setting any of them on a
Bridge client, Bridge publisher, or server-mode endpoint is rejected at construction rather than
silently connecting unauthenticated; authenticate the Bridge protocol with TLS client
certificates.

## Additional dynamic source details

For a local descriptor, replace `reflection: true` with:

```yaml
descriptor_set_path: proto/events.bin
```

and generate it with:

```bash
protoc --descriptor_set_out=proto/events.bin --include_imports -I proto proto/events.proto
```

An arbitrary unary method produces one canonical message; a server-streaming method produces one
per response. Every response has a deterministic ID derived from service, method, response index,
and protobuf bytes, plus `grpc.service`, `grpc.method`, `grpc.response_index`, and
`grpc.ack_guarantee=none` metadata.

Dynamic JSON uses protobuf's canonical JSON mapping: bytes are base64 strings; enum names are
strings; timestamps use RFC 3339; maps are JSON objects; a oneof sets at most one named field; and
64-bit integers are JSON strings. Unknown fields and values that do not match the input descriptor
are construction errors.

Dynamic calls have **no acknowledgement guarantee**. Descriptor availability says only how to
encode an RPC. gRPC transport success is not evidence that a target durably committed work. Any
future acknowledgement integration must explicitly configure a second RPC or request template;
mq-bridge will not infer one from descriptors.

gRPC failures preserve code, message, and trailing metadata in `GrpcStatusError`. Its ordinary
`Display`/`Debug` output omits trailer values; callers must explicitly inspect
`trailing_metadata()` when those values are required.

### TLS and mutual TLS

```yaml
input:
  grpc:
    url: https://events.example.com:443
    reflection: true
    service_name: events.v1.EventService
    method_name: Tail
    bearer_token: ${EVENTS_TOKEN}
    tls:
      required: true
      ca_file: certs/ca.pem
      cert_file: certs/client.pem
      key_file: certs/client-key.pem
```

The same TLS fields apply to Bridge clients and servers. On an embedded server, `ca_file` enables
client-certificate verification for mTLS.

A client uses TLS when its URL is `https://` (or has no scheme and `tls.required` is set).
`tls.required` with an `http://` URL is an error. `ca_file` is trusted in addition to the
bundled web roots; `cert_file` and `key_file` go together, and an encrypted key
(`cert_password`) is not supported.

## Dynamic server method

A server-mode input with a descriptor, `service_name`, and `method_name` serves that unary method
alongside `mqbridge.Bridge` on the same listener:

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

Each call becomes one message whose payload is the request in protobuf's canonical JSON mapping
and whose `mq_bridge.topic` metadata is the RPC path (`/benchmark.BenchmarkService/GetSum`). The
route's reply payload must be JSON matching the output message; it is encoded back to protobuf.
An acknowledgement without a reply returns the default output message, a failed message returns
`INTERNAL`, and a request that does not decode returns `INVALID_ARGUMENT`. Streaming methods, and a
method name without a service name or descriptor, are rejected at construction.

Any caller the listener admits can invoke the method; there is no per-call authorization. Incoming
call metadata is not passed to the route, so it cannot carry a caller identity downstream. Restrict
who may call with TLS client certificates on the listener.

## Generic server boundary

Beyond one unary method per route, server mode does not register arbitrary RPC paths from
descriptor sets. Streaming server methods would first need a public contract defining all of the
following:

- conversion of every incoming streamed protobuf request into `CanonicalMessage`;
- correlation and production of unary, client-streaming, server-streaming, and bidi responses;
- registration and conflict handling for descriptor-defined RPC paths;
- how downstream code returns gRPC headers, status codes, and trailers;
- bounded queues and per-stream backpressure;
- behavior when downstream processing fails before or after a partial streamed response.

Until those semantics exist, accepting arbitrary inbound services would make delivery and failure
behavior ambiguous, so streaming stays out of server mode.
