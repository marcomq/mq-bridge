# Configuration Guide

`mq-bridge` uses a flexible configuration system supporting YAML, JSON, and environment variables.

## Configuration Reference

The best way to understand the configuration structure is through a comprehensive example. `mq-bridge` uses a YAML map where keys are route names.

```yaml
# mq-bridge.yaml

# Route 1: Kafka to NATS
kafka_to_nats:
  concurrency: 4
  input:
    kafka:
      url: "localhost:9092"
      topic: "orders"
      group_id: "bridge_group"
      # TLS Configuration (Optional)
      tls:
        required: true
        ca_file: "./certs/ca.pem"
  output:
    nats:
      url: "nats://localhost:4222"
      subject: "orders_stream.processed"
      stream: "orders_stream"

# Route 2: HTTP Webhook to MongoDB with Middleware
webhook_to_mongo:
  input:
    http:
      url: "0.0.0.0:8080"
      # Force the normal route pipeline instead of the inline HTTP response fast path.
      inline_response_fast_path: false
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "webhooks"
      format: "json" # a bit slower, but better readability
    middlewares:
      - retry:
          max_attempts: 3
          initial_interval_ms: 500

# Route 3: File to AMQP (RabbitMQ)
file_ingest:
  input:
    file:
      path: "./data/input.jsonl"
  output:
    amqp:
      url: "amqp://localhost:5672"
      exchange: "logs"
      queue: "file_logs"

# Route 4: AWS SQS to SNS
aws_sqs_to_sns:
  input:
    aws:
      # To consume from SNS, subscribe this SQS queue to the SNS topic in AWS Console/Terraform.
      queue_url: "https://sqs.us-east-1.amazonaws.com/000000000000/my-queue"
      region: "us-east-1"
      # Credentials (optional if using env vars or IAM roles)
      access_key: "test"
      secret_key: "test"
  output:
    aws:
      topic_arn: "arn:aws:sns:us-east-1:000000000000:my-topic"
      region: "us-east-1"

# Route 5: IBM MQ Example
ibm_mq_route:
  input:
    ibmmq:
      queue_manager: "QM1"
      url: "localhost(1414)"
      channel: "DEV.APP.SVRCONN"
      queue: "DEV.QUEUE.1"
      username: "app"
      password: "admin"
  output:
    memory:
      topic: "received_from_mq"

# Route 6: MQTT to Switch (Content-based Routing)
iot_router:
  input:
    mqtt:
      url: "mqtt://localhost:1883"
      topic: "sensors/+"
      qos: 1
  output:
    switch:
      metadata_key: "sensor_type"
      cases:
        temp:
          kafka:
            url: "localhost:9092"
            topic: "temperature"
      default:
        memory:
          topic: "dropped_sensors"

# Route 7: ZeroMQ PUSH/PULL
zeromq_pipeline:
  input:
    zeromq:
      url: "tcp://0.0.0.0:5555"
      socket_type: "pull"
      bind: true
  output:
    zeromq:
      url: "tcp://localhost:5556"
      socket_type: "push"
      bind: false
      # format: "raw_framed"  # default: raw payload bytes with a JSON metadata frame in front, so headers still travel
      # format: "raw"         # raw payload bytes per frame, no metadata (e.g. JPEG, Protobuf)
      # format: "json"        # JSON-wrapped CanonicalMessage, whole batch in one frame
      # backend: "try_omq"    # default: use the omq backend when built in, else zmq. Or pin "omq" / "zmq".
      # NOTE: REQ/REP replies ignore `format`. A "rep" consumer always answers with a JSON
      # array of canonical messages and a "req" publisher always decodes one, even under
      # "raw"/"raw_framed" — there `format` still frames the request only. An external REP
      # service answering a mq-bridge "req" endpoint has to reply in that JSON shape.

# Route 8: PostgreSQL via SQLx
sqlx_postgres_route:
  input:
    sqlx:
      url: "postgres://user:pass@localhost:5432/mydb"
      table: "job_queue"
      delete_after_read: true
  output:
    memory:
      topic: "processed_jobs"

# Route 9: Cross-process IPC via the memory endpoint
# The `topic` field (alias `url`) is a transport URL:
#   "name"                  -> memory://name  (in-process, same process only)
#   "memory://name"         -> in-process channel
#   "ipc://name"            -> Unix: /run/mq-bridge/name.sock (falls back to
#                              $XDG_RUNTIME_DIR/mq-bridge, then /tmp/mq-bridge)
#                              Windows: \\.\pipe\mq-bridge-name
#   "ipc:///abs/path.sock"  -> that exact socket path (Unix)
#   "unix:///abs/path.sock" -> Unix only, path must be absolute
#   "pipe://name"           -> Windows only, \\.\pipe\name
# The consumer side binds/listens and must be started before the publisher connects.
# IPC does not support `subscribe_mode` or `request_reply`.
# `enable_nack` defaults to true, but redelivery is consumer-local: a nacked message
# is retried inside the consumer and is lost if the consumer process dies.
ipc_ingest:
  input:
    memory:
      url: "ipc:///run/mq-bridge/orders.sock"
      capacity: 256
  output:
    kafka:
      topic: "orders"
      url: "localhost:9092"
```

## Configuration Details

### Environment Variables
All YAML configuration can be overridden with environment variables. The mapping follows this pattern:
`MQB__{ROUTE_NAME}__{PATH_TO_SETTING}`

For example, to set the Kafka topic for the `kafka_to_nats` route:
```sh
export MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC="my-other-topic"
```
#### Postgres CDC example

```yaml
orders_cdc:
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost:5432/app"
      publication: "orders_pub"   # CREATE PUBLICATION orders_pub FOR TABLE orders;
      slot_name: "mqb_orders"       # created if missing (permanent slot, resumable)
  output:
    nats:
      subject: "orders.changes"
      url: "nats://localhost:4222"
```

Each change arrives as a `CanonicalMessage` whose payload is the flat row and whose `postgres.operation` metadata marks the operation — the same convention as MongoDB CDC, so typed handlers work identically across both. The replication transport uses the published [`pgwire-replication`](https://crates.io/crates/pgwire-replication) crate.

### NATS JetStream Notes

Two gotchas worth knowing before wiring up a `nats` endpoint:

- **Subject must be prefixed with the stream name.** When mq-bridge auto-creates
  a JetStream stream (no existing stream already covers the subject), it scopes
  the stream to `{stream}.>`. So `stream: "orders_stream"` requires a subject
  like `orders_stream.foo` — a subject such as `orders.foo` will fail to publish
  with "no stream found for given subject". This only applies to
  auto-creation; publishing to a stream that already exists with a wider
  subject filter works regardless of naming.
- **`stream` is required even in Core NATS mode.** Consumer validation requires
  a `stream` value even when `no_jetstream: true`. It's unused for the actual
  Core NATS subscribe, but validation still rejects a missing value — pass any
  placeholder string.

### Middleware Configuration

> Every available middleware, with its fields, defaults, supported side (input/output) and a
> working example, is listed in **[REFERENCE.md](REFERENCE.md#middleware)**. Note in
> particular the [ordering rule](REFERENCE.md#ordering--read-this-before-combining-middleware):
> on an **output**, the *last* middleware in the list is the outermost layer, so `dlq` goes
> last.

Middleware is defined as a list under an endpoint. `retry` and `dlq` belong on the output;
on an input the route fails at start.

```yaml
input:
  middlewares:
    - deduplication:
        sled_path: "/var/data/mq-bridge/dedup_db"
        ttl_seconds: 3600 # 1 hour
  kafka:
    # ... kafka config
output:
  middlewares:
    - retry:
        max_attempts: 5
        initial_interval_ms: 200
    - dlq:
        endpoint:
          nats:
            subject: "my-dlq-subject"
            url: "nats://localhost:4222"
  nats:
    # ... nats config
```

### TLS & Security Hardening

Most endpoints accept a `tls` block. The available fields are:

```yaml
tls:
  required: true                   # enable TLS
  ca_file: "./certs/ca.pem"        # CA to verify the server
  cert_file: "./certs/client.pem"  # client cert (mTLS)
  key_file: "./certs/client.key"   # client private key (mTLS)
  cert_password: "secret"          # password for an encrypted key (where supported)
  accept_invalid_certs: false      # NEVER set true in production
```

**Hardening checklist (e.g. for PCI-DSS Req 4.2.1):**

1. **Enable TLS on every endpoint carrying sensitive data** (`required: true`) and supply
   a `ca_file`. Use mTLS (`cert_file` + `key_file`) for mutual authentication where the
   broker supports it.
2. **Never disable certificate validation.** `accept_invalid_certs` defaults to `false`;
   leaving it that way is required — setting it `true` on a sensitive path defeats TLS.
3. **Choose the crypto provider feature.** Build with `rustls-aws-lc` (FIPS-capable, also
   enables post-quantum key exchange) or `rustls-ring`. The rustls-based endpoints — NATS,
   MQTT, HTTP, gRPC, WebSocket, AMQP — only ever negotiate rustls's safe TLS 1.2/1.3 AEAD
   cipher suites; weak/legacy suites (RC4, 3DES, CBC-SHA1, export) cannot be offered, so
   "strong ciphers only" holds without any explicit cipher list.
4. **Kafka** (librdkafka/OpenSSL): certificate verification is on by default
   (`enable.ssl.certificate.verification` follows `accept_invalid_certs`). If an auditor
   requires an explicit allowlist, pin it via `producer_options` / `consumer_options`:
   ```yaml
   kafka:
     producer_options: [["ssl.cipher.suites", "ECDHE-RSA-AES256-GCM-SHA384"]]
     consumer_options: [["ssl.cipher.suites", "ECDHE-RSA-AES256-GCM-SHA384"]]
   ```
5. **IBM MQ** (native stack): set a strong `tls.cipher_spec` (a TLS 1.2/1.3 CipherSpec) — it
   is required for encrypted connections. Note `cipher_spec` lives under `tls`, not at the
   top level of the `ibmmq` config (a breaking rename from earlier releases, where it was
   `ibmmq.cipher_spec`).
6. **Keep sensitive payloads out of logs.** Message payloads are emitted at `trace` level;
   run production above `trace` and confirm no cardholder data (PAN) reaches logs or traces.
7. **Do not commit secrets.** Source passwords and tokens from a secrets manager or env vars
   (`MQB__...`) rather than checked-in config.

**Notes and boundaries:**

- TLS 1.3 alone is sufficient in 2026 (Mozilla "Modern" profile). The rustls endpoints
  currently negotiate **TLS 1.2 and 1.3** (both are PCI-acceptable). A central
  "TLS 1.3-only" toggle is not yet configurable in the library; enforce a minimum protocol
  version on the broker/server side, which is the side that accepts the connection.
- Kafka and IBM MQ use native TLS stacks, so a library-wide version policy cannot be
  applied to them — configure their minimum TLS version on the broker.

### HTTP Consumer Fast Path

Compatible `http -> response` routes may use an inline response fast path for lower latency. This bypasses the normal route consumer/worker/disposition pipeline, but it still keeps the output publisher chain active, including output handlers and allowed output middlewares.

The fast path is only considered when:

- the input has no middlewares
- `receive_streamable` is `false`
- `fire_and_forget` is `false`
- output middlewares are limited to `buffer`, `delay`, `limiter`, and/or `metrics`

To force the normal route pipeline, set this on the HTTP consumer:

```yaml
input:
  http:
    url: "0.0.0.0:8080"
    inline_response_fast_path: false
```

This is useful when you want stable, explicit semantics regardless of future optimizations, or when you want to avoid the inline path's response behavior differences. In particular, the inline path does not automatically echo unchanged request metadata back as HTTP response headers.

For HTTP publishers, `pass_through_status: true` treats non-2xx response statuses as response
data instead of publisher errors. On a non-streaming HTTP request/reply route, it also keeps the
listener running after a transient sink failure and returns HTTP 502 to the request. For composite
outputs such as `fanout`, every leaf sink must opt in; mixed outputs retain the normal
stop-and-reconnect policy. Streamable HTTP inputs retain their protocol-specific error frames;
neither they nor `fire_and_forget` consumers use this 502 behavior.

### Connection Sharing

Publishers that target the same server reuse one underlying transport client by
default, instead of each opening its own. This consolidates TCP connections, background
threads, and batching, and follows each driver's own guidance (one shared producer /
client / pool per application). Sharing applies to **Kafka, NATS, MongoDB, SQLx, HTTP,
and gRPC**; the client is keyed by its connection-level settings (URL, auth, TLS, and
client-level options), never by topic/subject/collection. A shared client is released
once the last publisher using it is dropped.

Set `shared: false` on a publisher to give it a dedicated connection:

```yaml
orders_out:
  output:
    kafka:
      topic: "orders"
      url: "localhost:9092"
      shared: false   # dedicated producer — keeps this latency-sensitive topic off a busy producer's queue
```

*   **Kafka**: a single producer serves every topic and is the recommended setup. Use
    `shared: false` to isolate a latency-sensitive topic from a high-throughput one so
    they don't share one internal send queue (head-of-line blocking).
*   **SQLx**: a shared pool means its `max_connections` is a budget shared across every
    route using that database. Use `shared: false` if a route needs its own pool.
*   **gRPC**: a shared channel multiplexes over one HTTP/2 connection; at very high
    concurrency its max-concurrent-streams cap can bottleneck — `shared: false` gives a
    dedicated channel.

### Dynamic gRPC sources

The stable generated `mqbridge.Bridge` protocol remains the default. To call an
arbitrary unary or server-streaming gRPC method, provide a compiled protobuf descriptor
set plus the service, method, and JSON request:

```yaml
input:
  grpc:
    url: https://grpc.example.com:443
    descriptor_set_path: proto/events.bin
    service_name: events.EventService
    method_name: Tail
    request:
      topic: audit
```

The deprecated `timeout_ms` and `server_streaming` configuration keys are still accepted:
`timeout_ms` is a fallback for connection and request setup, while a dynamic stream's idle and
overall deadlines require their dedicated keys.

Generate the descriptor with imports included:

```bash
protoc --descriptor_set_out=proto/events.bin --include_imports -I proto proto/events.proto
```

Responses use protobuf's canonical JSON representation as the canonical message payload.
Dynamic mode derives unary versus server-streaming behavior from the descriptor; client-streaming
and bidirectional-streaming methods are rejected with explicit capability errors. A descriptor
describes the wire format but does not define broker acknowledgement
semantics, so dynamic sources have no generic ACK operation. Use the built-in
`mqbridge.Bridge` mode when route-level ACK/NACK and at-least-once delivery are required.

The same descriptor keys on a route's `output` call a method instead of reading one: unary
methods send one call per message, client-streaming methods stream a whole batch into one call,
and `request` is rejected because the published messages are the requests.

See the complete [gRPC integration guide](GRPC.md) for reflection, descriptor bytes, metadata and
authentication, separate deadlines, canonical protobuf JSON, TLS/mTLS, external client generation,
delivery guarantees, and the intentional generic-server boundary.

### Specialized Endpoints

> This section covers `switch` in depth. The other structural endpoints — `ref`, `fanout`,
> `request`, `response`, `reader`, `static`, `stream_buffer`, `null` and `custom` — are
> documented in **[REFERENCE.md](REFERENCE.md#structural-endpoints)**.

#### Switch

The `switch` endpoint is a conditional publisher that routes messages to different outputs based on a metadata key.

It checks the specified `metadata_key` in each message. If the key's value matches one of the `cases`, the message is forwarded to that endpoint. If no case matches, it's sent to the `default` endpoint. If there is no default, the message is dropped.

This is useful for content-based routing.

**Example**: Route orders to different systems based on `country_code` metadata.

```yaml
output:
  switch:
    metadata_key: "country_code"
    cases:
      US:
        kafka:
          topic: "us_orders"
          url: "kafka-us:9092"
      EU:
        nats:
          subject: "eu_orders"
          url: "nats-eu:4222"
    default:
      file:
        path: "/var/data/unroutable_orders.log"
```

#### Directory spool (`dir_spool`)

The `dir_spool` endpoint is a crash-safe FIFO queue whose backing store is a directory. Each
message becomes a *chunk*: a payload file holding the raw `CanonicalMessage` payload bytes,
plus an optional JSON sidecar holding its metadata. Chunks are written to a `.tmp` name,
fsynced, and renamed into place, so a reader listing the directory never sees a partial
write; on the reading side a chunk is deleted once its message is acknowledged.

Use it to decouple a fast producer from a slower consumer across a process or language
boundary — a Rust edge engine feeding a Python data-science script, say — with no broker to
run and no shared memory to size. The producer can finish and exit while the consumer is
still draining, and a crash on either side leaves a directory you can inspect with `ls`.

Prefer the [`file`](#configuration-reference) endpoint instead when the data is a stream of
delimited records appended to one file. `dir_spool` is for a queue of arbitrarily large
opaque blobs, where the delimiter framing and the single append point both get in the way.

| Field | Side | Default | Meaning |
|---|---|---|---|
| `path` | both | — | Spool directory. Created if missing. |
| `naming_pattern` | sink | `{seq:09}` | Chunk name without extension. Supports `{seq}`, `{seq:06}` / `{seq:06d}`, `{timestamp}` (unix millis) and `{message_id}`. |
| `shard_depth` | both | `0` | Levels of shard subdirectory to spread chunks over. 0 writes every chunk straight into `path`. |
| `shard_width` | both | `3` | Characters of the sequence number each shard level consumes. Ignored when `shard_depth` is 0. |
| `payload_extension` | both | `bin` | Extension of the payload file, with or without the leading dot. |
| `metadata_extension` | both | `json` | Extension of the metadata sidecar. Empty string writes and expects payload files only. |
| `atomic` | sink | `true` | Write to `.tmp` and rename on completion. |
| `fsync` | both | `chunk` | Durability: `chunk` fsyncs every chunk file and the directory once per batch, `off` fsyncs nothing and lets the OS flush. |
| `done_file` | both | `DONE` | Sentinel that marks production finished. |
| `emit_done` | sink | `never` | When to create `done_file`: `success` (the route reached the end of its input and every chunk was written), `end` (whenever the producer closes, however the pass ended), `never`. Set it on the *last* producer only. |
| `producer_file` | both | `PRODUCER` | File holding the producer lock, which keeps a second producer out. |
| `consumer_file` | both | `CONSUMER` | File holding the consumer lock, which keeps a second draining consumer out. |
| `drain_on_read` | source | `true` | Delete each chunk once its message is acknowledged. |
| `stop_on_done` | source | `false` | End the stream once the queue is empty *and* `done_file` is present. |
| `poll_interval_ms` | source | `100` | Idle poll interval when the directory holds no new chunks. |
| `source_metadata` | source | `false` | Stamp `mqb.src.spool_path` and `mqb.src.spool_chunk`. |
| `claim` | both | `exclusive` | Lock this side of the spool: `exclusive` refuses to start when another instance holds the same role, `warn` logs and runs anyway, `off` takes no lock at all. |

Lexical order is queue order, so `naming_pattern` **must start with the sequence** — a sink
whose pattern begins with anything else is rejected. The front of a chunk name is
load-bearing twice over: the listing is ordered by it, and a publisher opening an existing
spool reads its next sequence number back out of it, resuming past the highest chunk already
there rather than overwriting the head of the queue. `{timestamp}_{seq}` would resume from a
13-digit timestamp, and a literal prefix would resume from zero and overwrite. Padding is
not enforced, but an unpadded `{seq}` warns: chunk 10 sorts before chunk 2.

`payload_extension` and `metadata_extension` must also differ, on both sides — a consumer
whose two extensions match would read every sidecar as a payload.

##### Cardinality, and how production ends
One spool directory takes **one producer and one consumer at a time**, each of which may be
internally concurrent — `concurrency > 1` on either side is fine, since the sequence counter
and the in-flight set are shared within an instance. A *second* instance in the same role
corrupts the queue rather than sharing it: two producers seed from the same highest sequence
number and overwrite each other's chunks, and two draining consumers each deliver every
chunk they win the race to read.

So each side takes a pid lock on its own file in the spool — `producer_file` and
`consumer_file` — created when the endpoint opens and removed when it closes, and `claim:
exclusive` (the default) refuses to start when its role is already held, naming the holder.
The two roles never conflict with each other, so a producer and a consumer sharing a
directory — the point of the endpoint — needs no configuration. Every instance touching one
spool must of course agree on the two names; that agreement is what makes them exclude each
other.

*At a time* is the operative phrase: the locks say "someone is running", not "production is
finished". A spool may be filled by several producers in turn, so the end of the stream is a
separate signal — the `done_file` sentinel:

- `stop_on_done` ends the stream once the queue is empty **and** `done_file` is present, so
  a producer that finished long ago still has its backlog drained first, and the gap between
  two producers does not cut the stream short.
- Only the **last** producer should set `emit_done`, and its value says what "finished"
  has to mean. The sentinel is written as that producer closes, before its lock is
  released, so a hand-off to another producer cannot lose it.
  - `success` is the strict reading: the route reached the natural end of its input (an
    exhausted source, or a `--drain` that emptied it) **and** every chunk the producer
    accepted reached the disk. A route that is shut down, that fails, or that reconnects
    writes nothing, so a `stop_on_done` consumer keeps waiting for the production that did
    not finish. A *continuously running* producer never reaches a natural end, so `success`
    on one of those means "never" in practice — use `end` there.
  - `end` is the loose reading: nothing more is coming from here, whatever the reason. It
    says nothing about whether everything worked, so a consumer will treat a truncated
    stream as the whole of it — which is the right trade when the alternative is waiting
    forever on a producer that may have died.
- A producer opening the spool **deletes** an existing sentinel: it is producing again, and
  a stale marker would tell a `stop_on_done` consumer to exit the moment its queue first ran
  dry.
- A producer that *crashes* never writes the sentinel, so a `stop_on_done` consumer keeps
  waiting — correct, because production did not finish. Its lock, on the other hand, is
  broken automatically, so the restarted producer can pick up where it left off.

Two things to know about the locks themselves:

- **A lock is broken only when its owner is provably gone.** The check is by process id, so
  it works for a crash on the machine that wrote the lock. A spool shared between hosts or
  containers cannot be judged that way: clear such a lock by deleting the file, and be aware
  that a pid check across a pid-namespace boundary can also break a lock whose owner is
  alive but invisible.
- **The three control files must not collide** with each other or with a chunk. A name that
  ends in `payload_extension` would be delivered as a message, one ending in
  `metadata_extension` would be read as a chunk's sidecar, one ending in `.tmp` looks like a
  chunk mid-write, and two roles under one name would have each release deleting the other's
  lock. All of these are rejected at startup (and by route validation), compared
  case-insensitively so that `DONE` and `done` count as one file on Windows and macOS.

Genuinely shared spools are still possible with `claim: warn` or `claim: off`, at a price on
each side. Several *simultaneous* producers need `{message_id}` in `naming_pattern` to keep
names from colliding. Several *draining* consumers get **duplicate delivery**: a chunk is
claimed only in the reading process's memory, and deleted only once its message is
acknowledged, so two drainers can both read one before either deletes it and both will
deliver it. There is no on-disk claim — no rename-to-claim handshake — so competing
consumers do not partition a spool between themselves. The lock is what makes that
arrangement hard to enter by accident; if you switch it off, one drainer per spool is still
the rule.

`drain_on_read: false` is the one exception to the cardinality rule: such a reader deletes
nothing, so several of them over one spool each see every chunk once and none of them takes
a lock — though each will warn if a draining consumer holds the directory, because that one
deletes chunks out from under it. It remembers what it has read by name, in memory, for as
long as the run lasts: a chunk may appear with a name below one already delivered, so a
position alone could not tell the two apart. Budget for that on a spool of millions of
chunks that is never drained, and restart the reader to reset it — a fresh run re-reads
the directory from the beginning.

##### Throughput and durability
The endpoint's dominant cost is fsync, not mq-bridge. The default `fsync: chunk` is **two
fsyncs per message** when a sidecar is written — the payload and its sidecar — plus one
directory fsync per batch. Measured on one machine (btrfs on NVMe, 64 KiB payloads, one
message per `send_batch`), single sink, no route:

| | `fsync: chunk` | `fsync: off` |
|---|---|---|
| with sidecar | ~210 msg/s | ~2100 msg/s |
| `metadata_extension: ""` | ~360 msg/s | ~3900 msg/s |

On tmpfs the same run gives ~3500 and ~9200 msg/s respectively, which is the number to use
if the spool lives in `/dev/shm` — and a caution against sizing from a tmpfs measurement for
a spool that will land on a disk. Treat these as an order of magnitude, not a specification:
a spinning disk is far slower and a battery-backed controller far faster.

Two ways to buy headroom before reaching for `fsync: off`: drop the sidecar when the payload
is self-describing (`metadata_extension: ""`, which nearly doubles the rate and halves the
file count), and batch — the per-batch directory fsync is amortized over the batch, so
`batch_size: 32` pays it once rather than 32 times. `fsync: off` trades the endpoint's
crash-safety claim, not just its tail: a power loss can leave a chunk present but truncated,
which a consumer would deliver as a short message.

##### Sharding
A flat spool is one directory per stream, and most filesystems degrade long before mq-bridge
does — 30fps with sidecars is over 200,000 files an hour. `shard_depth` spreads chunks over
subdirectories named from the leading digits of the sequence number, so with the default
`{seq:09}`, `shard_depth: 2` and `shard_width: 3`, chunk 1 is written as `000/000/001.bin`
(its sidecar beside it) and no directory ever holds more than 1000 entries.

Queue order is unaffected: lexical order over the whole relative path is still sequence
order, and a chunk's recorded identity (`mqb.src.spool_chunk`) becomes that path. Sharding
also makes a large backlog cheaper to read, not just to store — the consumer walks shards in
order and stops as soon as its listing is full, so it reads the leading shards rather than
the whole tree, and a drained shard directory is pruned as it empties.

Three things to get right:

- **`naming_pattern` must start with a zero-padded `{seq:0N}`** wider than
  `shard_depth * shard_width`. The directory names are cut from the front of every rendered
  name, so that front has to be a fixed width — otherwise chunk 999 and chunk 1000 would
  shard to different depths and lexical order would stop being queue order. A pattern that
  cannot be split is rejected at startup, as is one with no room left for the file name.
- **Both ends must agree on `shard_depth` and `shard_width`.** A consumer descends only as
  far as its own depth, into directory names only as wide as its own width, so either one
  set differently from the producer reads the spool as permanently empty (and under
  `stop_on_done`, ends the stream immediately). It warns when a scan finds subdirectories
  it is not configured to enter, which is what that mistake looks like.
- **A control file cannot be shaped like a shard.** `done_file: "000"` with
  `shard_width: 3` is rejected, since one name cannot be both the sentinel and a directory.

A directory scan keeps up to 65,536 chunk names for the batches that follow, so draining a
backlog costs one scan per that many messages rather than one per batch. Two consequences
are visible from outside: chunks that arrive while a listing is still being served are
picked up only once it is exhausted (within `poll_interval_ms` on an idle spool), and the
batch that empties a listing can be shorter than the route's `batch_size`.

**Example**: video frames written by one process and drained by another.

```yaml
# Producer: raw H.264 chunks, no sidecar — an Annex B chunk is self-describing, and a
# sidecar per frame would double the file count and the fsyncs for nothing. Keep the
# default `metadata_extension: "json"` on a spool whose metadata has to travel.
# Holds the PRODUCER lock while it runs; on a clean finish it writes DONE, which is what
# ends the consumer's stream below. Use `emit_done: end` instead if the consumer must not
# wait on a producer that may die.
frame_capture:
  input:
    memory:
      topic: "frames"
  output:
    dir_spool:
      path: "/tmp/video_telemetry_spool"
      naming_pattern: "{seq:09}_{timestamp}"
      # 30fps fills a flat directory fast: two levels of three digits keep every
      # directory under 1000 entries. The consumer needs the same shard_depth.
      shard_depth: 2
      shard_width: 3
      payload_extension: ".h264"
      metadata_extension: ""
      emit_done: success

# Consumer: drain in sequence order, delete as we go, exit when the producer is done.
frame_ingest:
  input:
    dir_spool:
      path: "/tmp/video_telemetry_spool"
      shard_depth: 2
      shard_width: 3
      payload_extension: ".h264"
      metadata_extension: ""
      drain_on_read: true
      stop_on_done: true
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "telemetry"
      collection: "frames"
```

A message nacked by the route is left on disk and redelivered on the next poll. A chunk
whose payload file exists without a sidecar is still delivered, with empty metadata — which
is what lets a foreign producer write into the spool with nothing but `write()` and
`rename()`.

##### Further reading
`dir_spool` is a deliberately old idea. Spooling — buffering plus a queue, so that a fast
producer hands work off and carries on while a slow consumer drains it at its own pace — has
been the standard answer to this problem since the 1960s, and the print spooler is its
canonical form. These are worth reading before designing anything on top of a spool
directory:

- [Spooling](https://en.wikipedia.org/wiki/Spooling) (Wikipedia) — the concept and its
  vocabulary, the name (said to come from "Simultaneous Peripheral Operations On-Line",
  possibly a backronym), and a bibliography that goes back to the primary sources: Lundin &
  Stoneman's *The Spooler User Guide* (1977), Donovan's *Systems Programming* (1972), and the
  spooling chapters of Peterson & Silberschatz and Tanenbaum.
- [Printing and spooling](https://www.cise.ufl.edu/~jnw/SysAdminfa04/Lectures/34.html)
  (University of Florida CISE, system administration lecture, 2004) — Unix print spooling
  from the operator's side: BSD copying job data into the spool directory under a job id
  versus System V writing a request file that points at the original, the `lpd`/`lpsched`
  daemons, and the operational concerns that outlive the software — who owns the spool
  directory and with what permissions, and which hosts may submit into it.
- [Spooling: print spoolers in practice](https://www.onenoughtone.com/learn/spooling/2) — a
  modern worked example, tracing a job through CUPS end to end: the `cupsd` scheduler, the
  queue under `/var/spool/cups/`, filter chains, and device backends.
- *Spooler Programmer's Guide*, Hewlett-Packard, part number 522287-002, August 2012
  ([PDF](http://nonstoptools.com/manuals/Spoolcom-Guide.pdf) — the file is named for
  Spoolcom, one component it documents, but it is the programmer's guide) — the deepest of
  these, and the one closest to this endpoint's problem. It specifies a production spooler
  (HP NonStop, documented since 1995) as a set of cooperating processes with explicit state
  machines: a supervisor, *collectors* that accept data from application programs and write
  it to disk, *print processes* that drain a job and mark it complete, and *perusal
  processes* that read spooled data **without** consuming it. Worth reading for the state
  diagrams — job, collector, print process, device — and for the chapter on spooling from a
  fault-tolerant process pair, which is the classical treatment of the question this
  endpoint answers with locks and redelivery: what a spool owes a producer that may die
  mid-job.

Four things `dir_spool` takes from that tradition. The directory *is* the queue, so the
producer's obligation ends when the file lands and a crash on either side leaves a state you
can read with `ls`. Data is copied into the spool rather than referenced in place — the BSD
choice rather than the System V one — which is what lets the producer finish and exit while
the consumer is still draining. The control information is a separate file from the data,
which is what the payload/sidecar split is, and why the two extensions must differ. And a
reader that consumes is a different thing from a reader that only looks: NonStop's *perusal
process* is `drain_on_read: false`, which is why several of those may share a spool while
only one drainer may.

The vocabulary maps closely enough to be worth keeping in mind. A NonStop *collector* is
this endpoint's publisher, a *print process* its consumer, `PRINTCOMPLETE` the acknowledgement
that deletes a chunk, a *job number* the sequence in a chunk's name, and the supervisor's
*ready list* the cached listing a consumer serves batches from.

What it does differently is have no supervisor. A classic spooler puts one privileged daemon
in front of the queue and lets it serialize access and own the state machine; `dir_spool` has
no daemon at all, which is why exclusion is a pair of pid locks, why the cardinality rules
above have to be stated rather than enforced by a process boundary, and why the states a
chunk moves through are implicit in the filesystem rather than written down as a diagram. The
filter chains a print spooler applies on the way out are, in mq-bridge, the route's middleware
and handlers.

### IDE Support (Schema Validation) 
mq-bridge includes a JSON schema for configuration validation and auto-completion. 
1. Ensure you have a YAML plugin installed (e.g., YAML for VS Code). 
2. Configure your editor to reference the schema. For VS Code, add this to .vscode/settings.json: 
```json 
{ 
  "yaml.schemas": { 
    "https://raw.githubusercontent.com/marcomq/mq-bridge/main/mq-bridge.schema.json": ["mq-bridge.yaml", "config.yaml"]
  } 
} 
```
To regenerate the schema from this repo, run: `cargo test --features schema`
