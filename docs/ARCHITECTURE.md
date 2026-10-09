# Architecture Overview

`mq-bridge` is designed as a highly extensible, protocol-agnostic message integration layer for Rust. Its architecture enables seamless bridging between diverse messaging systems, databases, and protocols, while allowing users to inject custom business logic and reliability patterns.

## Core Principles
- **Protocol Abstraction:** All business logic operates on a unified `CanonicalMessage` type, decoupling your code from specific broker or database APIs.
- **Extensibility:** New endpoints and middleware can be added with minimal effort via trait-based factories.
- **Async-First:** Built on Tokio, all I/O and processing is asynchronous and concurrency-aware.
- **Unopinionated:** The library does not enforce a specific domain or concurrency model, focusing instead on reliable, programmable data movement.

## Main Components

### 1. Route
A `Route` defines a data pipeline from one input endpoint to one output endpoint. Each route can:
- Specify concurrency and batch size
- Attach middleware for reliability, deduplication, metrics, etc.
- Attach a handler for business logic (transform, filter, respond)

### 2. Endpoint
Endpoints are protocol adapters for sources (consumers) and sinks (publishers). Supported types include Kafka, NATS, AMQP, MQTT, MongoDB, HTTP, SQLx, ZeroMQ, Files, AWS, IBM MQ, and the `memory` endpoint (in-process channels and cross-process IPC). Endpoints are created via factory functions and configured via serde (json/yml).

Beyond these protocol adapters there are **structural endpoints** that compose other endpoints
or shape routing rather than talking to a broker: `ref`, `fanout`, `switch`, `request`,
`response`, `reader`, `static`, `stream_buffer`, `null` and `custom`. All of them are
documented in [REFERENCE.md](REFERENCE.md#structural-endpoints).

### 3. Middleware
Middleware wraps consumers and publishers to add cross-cutting features. There is no
`Middleware` trait: a middleware *is* a decorator implementing `MessageConsumer` and/or
`MessagePublisher`, which is why `CustomMiddlewareFactory` is defined as `apply_consumer` /
`apply_publisher`. Available middleware includes:
- Retries (exponential backoff) and dead-letter queues (DLQ)
- JSON transformation (`transform`): mapping, type coercion, schema validation
- Deduplication (sled-based), weak joins, buffering, rate limiting
- Metrics, delays, fault injection
- Custom user middleware

The complete list, with fields, defaults and the layer-ordering rules, is in
[REFERENCE.md](REFERENCE.md#middleware).

### 4. Handler
Handlers are user-defined async functions that process messages. There are two main handler types:
- **CommandHandler:** 1-to-1 or 1-to-0 transformation, can return a new message for publishing or as response.
- **EventHandler:** 1-to-N handler for event consumption. Compatible to CommandHandler, but should not return a response.
- **TypeHandler:** Strongly-typed handler, dispatches based on the `kind` metadata field and deserializes payloads.

## Memory Endpoint and IPC Transport

The `memory` endpoint is not only an in-process channel. Its `topic` field (serde alias: `url`) doubles as a **transport URL**, so the same endpoint type covers both in-process queues and cross-process IPC over Unix domain sockets / Windows named pipes.

### Transport URL schemes

| URL | Resolves to | Platform |
| --- | --- | --- |
| `my-topic` | `memory://my-topic` (no scheme = in-process, for backward compatibility) | all |
| `memory://my-topic` | In-process channel, shared by namespace within the same process | all |
| `ipc://my-queue` | Unix: `/run/mq-bridge/my-queue.sock`<br>Windows: `\\.\pipe\mq-bridge-my-queue` | Unix + Windows |
| `ipc:///var/run/my.sock` | That exact socket path (leading `/` = absolute path, note the three slashes) | Unix |
| `unix:///var/run/my.sock` | That exact socket path; the path **must** be absolute | Unix only |
| `pipe://my-pipe` | `\\.\pipe\my-pipe` — used verbatim, **no** `mq-bridge-` prefix | Windows only |

Anything else (`http://…`, an empty URL, a relative `unix://path`) is rejected at parse time.

**Named `ipc://` resolution on Unix** falls back in order, using the first writable location:
1. `/run/mq-bridge/<name>.sock` (systemd standard)
2. `$XDG_RUNTIME_DIR/mq-bridge/<name>.sock`
3. `/tmp/mq-bridge/<name>.sock` (less secure)

Because of this fallback, `ipc://name` can land in different places for different users or services. When both sides must agree deterministically, use an explicit path (`ipc:///run/myapp/queue.sock`) instead of a bare name.

### Roles: consumer is the server, publisher is the client

IPC is **unidirectional, publisher → consumer**, and the roles are fixed:

- The **consumer** binds and listens on the socket/pipe (server). It removes a stale socket file before binding, creates the parent directory with mode `0700`, and sets the socket to `0600`. It unlinks the socket on drop.
- The **publisher** connects to it (client).

So **the consumer process must be running before the publisher connects** — otherwise the publisher fails with a connection error. The consumer serves one connection at a time; if the peer disconnects, it logs a warning and waits for a new connection rather than erroring out.

### IPC requires async construction

`MemoryConsumer::new`, `MemoryPublisher::new`, and `new_local` are synchronous and only support `memory://`. Given an IPC URL they return an error ("requires async endpoint construction"). Use the async constructors:

```rust
use mq_bridge::endpoints::memory::{MemoryConsumer, MemoryPublisher};
use mq_bridge::models::MemoryConfig;

let config = MemoryConfig::new_with_url("ipc:///run/mq-bridge/orders.sock", Some(100));

// Server side (start first)
let mut consumer = MemoryConsumer::new_async(&config).await?;
// Client side, in another process
let publisher = MemoryPublisher::new_async(&config).await?;
```

Routes always build endpoints through the async factories, so an `ipc://` URL works in YAML/JSON config with no extra code:

```yaml
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

### Wire format

Batches are serialized with **MessagePack** (`Vec<CanonicalMessage>`) and written as length-prefixed frames: a 4-byte big-endian length followed by the payload. Frames larger than 100 MB are rejected.

Framing is handled by `tokio_util::codec::LengthDelimitedCodec` in `endpoints/memory/framed.rs`, shared by both platforms. This matters for more than deduplication: the codec owns the partial-frame buffer, which makes reads **cancel safe**. Routes cancel `receive_batch` on shutdown via `select!`, and a hand-rolled `read_exact` pair would consume part of a frame on cancellation and desync the connection permanently.

### Backpressure

A batch is written as one frame. Socket buffers are small — **8 KiB by default on macOS** (`net.local.stream.sendspace`) — so a batch that outgrows the buffer only completes once the consumer drains it. `send_batch` blocking is therefore normal backpressure, not a fault.

Two consequences worth knowing:

- The consumer must actually be reading, not merely connected. A consumer that has accepted but stopped draining will stall the publisher indefinitely. After 5 seconds blocked, the publisher logs a warning naming the socket; it keeps waiting rather than dropping data.
- `capacity` does not create a queue here. There is no buffering between the two processes beyond the kernel socket buffer.

### Acknowledgements and redelivery over IPC

`enable_nack` defaults to **`true`** for IPC transports (`ipc://`, `unix://`, `pipe://`) and `false` for `memory://`; an explicit `enable_nack` in config always wins.

Redelivery over IPC is **consumer-local**. The socket carries publisher → consumer traffic only, so a nack cannot travel back to the producer — the publisher never reads, and writing to it would strand the messages and eventually block the commit on a full socket buffer. A nacked message is therefore requeued inside the consumer and redelivered ahead of new traffic. It does **not** survive a consumer crash, and the publisher is never told.

The publisher's side is **at-most-once across a consumer crash**. A send is acknowledged once its bytes are in the kernel socket buffer, not when the consumer has read or processed them. If the consumer process dies, whatever sat in that buffer or in the consumer's memory is lost, and the publisher's source has already been committed.

A second publisher on the same socket connects without an error, because the kernel queues the connection, but the consumer only reads it after the first publisher disconnects. Until then its sends fill the socket buffer and then block; the 5-second stalled-send warning is the only sign.

If the hand-over must survive a crash of either process, use `dir_spool` between the two processes, or a broker endpoint. mq-bridge deliberately does not implement a bidirectional ack protocol over IPC.

### Behavioural differences vs `memory://`

- **`subscribe_mode` is not supported** over IPC, on either side — the EventStore/broadcast backend is in-process only.
- **`request_reply` is not supported** for IPC publishers.
- **Only the publisher side may send.** Calling `send_batch` on a consumer-side IPC transport returns an error rather than writing into a peer that never reads.
- **`capacity` bounds the consumer-side buffer**, not an internal queue — a socket has no backlog of its own. `len()` reports whole frames already buffered by the codec (readable without touching the socket), and endpoint `status().pending` adds the consumer's own buffered and awaiting-redelivery messages.

## Batching and Concurrency


Batch processing is a core concept in mq-bridge and is required for all endpoint implementations. Every consumer and publisher must implement batch receive and batch send methods (`receive_batch`, `send_batch`).

### Why batch mode?
- Batching improves throughput and efficiency, especially for high-volume or high-latency backends.
- It enables the bridge to process messages concurrently and in parallel, reducing per-message overhead.

### Concurrency
- Each route can be configured with a `concurrency` parameter, which determines how many worker tasks will process batches in parallel.
- Batch size is also configurable per route.

#### Ordering
Two independent guarantees, each decided once per route by the endpoint itself:

- **Commits** follow `MessageConsumer::commit_requires_order()` (default `true`). Cumulative-ack sources such as Kafka funnel their commits through one sequencer; individually-acking sources commit concurrently, bounded by `commit_concurrency_limit`.
- **Publishing** follows `MessagePublisher::requires_ordered_publish()` (default `false`). Above `concurrency: 1` workers call `send_batch` in parallel, so whole batches can reach the sink out of source order — rows keep their order *within* a batch. The `file` sink declares `true` and the route sequences the `send_batch` calls; batch prep and commits stay parallel, so the ordered path measures within noise of an unordered one.

Only `file` declares it, for two reasons that any other candidate has to clear:

- **The cost must be low.** The file sink already holds a write lock across the whole batch, so sequencing changes who writes, not how many write at once. Sequencing a sink that ends in a network round trip instead costs the whole concurrency factor — one batch in flight instead of N.
- **The guarantee must be reachable.** `object_store` does *not* declare it: read order there is object-key order, and the uuidv7 key randomises everything below the millisecond, so ordering the writes would buy nothing. Ordered cloud export needs a source-sequenced key first.

Broker sinks keep it off. NATS (both Core and JetStream) publishes a batch through `send_batch_helper`, which keeps up to `SEND_BATCH_CONCURRENCY` publishes in flight, so wire order inside one batch is already unordered — only the results are re-sorted. Kafka and AMQP preserve order within a batch (they submit the whole batch, then await confirms), but for them the order-critical part is the cheap submit loop, not the round trip, so a useful fix would have to release the sequence after submit rather than after `send_batch`. For per-key Kafka ordering today, use `concurrency: 1`.

### Helper Utilities
mq-bridge provides several helper functions to make implementing batching easier, especially if your endpoint only supports single-message operations:

- **`send_batch_helper`**: Calls `send` for each message in a batch and aggregates the results. Used to implement `send_batch` when only single-message sending is available.
- **`receive_batch_helper`**: Calls `receive` once and wraps the result as a batch. Used to implement `receive_batch` when only single-message receive is available.
- **`into_commit_func`**: Converts a batch commit function (`BatchCommitFunc`) into a single-message commit function (`CommitFunc`).
- **`into_batch_commit_func`**: Converts a single-message commit function into a batch commit function.

**Sample: Using `send_batch_helper`**
```rust
use mq_bridge::traits::send_batch_helper;
// Inside your MessagePublisher implementation:
async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
    send_batch_helper(self, messages, |pub_ref, msg| {
        Box::pin(pub_ref.send(msg))
    }).await
}
```

**Sample: Using `receive_batch_helper`**
```rust
// receive_batch_helper is a default method on MessageConsumer — no import needed.
// Inside your MessageConsumer implementation:
async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
    self.receive_batch_helper(max_messages).await
}
```

**Sample: Commit function conversion**
```rust
use mq_bridge::traits::{into_commit_func, into_batch_commit_func};
let commit: CommitFunc = into_commit_func(batch_commit_func);
let batch_commit: BatchCommitFunc = into_batch_commit_func(commit_func);
```

### How batch receive works internally

The `receive_batch` method is designed to efficiently collect a batch of messages from the underlying transport. The typical pattern is:

1. **Wait for the first message:** The consumer awaits a message from the backend (e.g., Kafka, NATS, etc.).
2. **Drain additional messages if available:** After the first message is received, the consumer immediately checks if more messages are already available (without waiting). It continues to drain messages up to the batch size or until no more are available.
3. **Return the batch:** The batch is returned as soon as either the batch size is reached or no more messages are immediately available.

This approach minimizes latency for the first message while maximizing throughput for bursts of messages.

**Pseudocode:**
```rust
async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
	let mut messages = Vec::with_capacity(max_messages);
	// Wait for the first message
	let first = self.inner_receive().await?;
	messages.push(first);
	// Try to drain more messages without waiting
	while messages.len() < max_messages {
		match self.try_receive_now()? {
			Some(msg) => messages.push(msg),
			None => break,
		}
	}
	Ok(ReceivedBatch { messages, commit: ... })
}
```

**Example: Receiving and committing a batch**
```rust
let batch = consumer.receive_batch(100).await?;
// Process each message in the batch...
batch.commit(vec![MessageDisposition::Ack; batch.messages.len()]).await?;
```

**Example: Sending a batch**
```rust
let messages = vec![msg1, msg2, msg3];
publisher.send_batch(messages).await?;
```

See the README and tests for more advanced batching and concurrency patterns.

## Getting Started

Below are minimal examples for the three main usage patterns in mq-bridge. For more, see the README and tests.

### 1. Typed Handler (Event-driven, Type-safe)

Use for strongly-typed, event-driven communication. Register Rust types per message `kind`; the bridge deserializes payloads automatically. Supports request-response where the protocol allows. Multiple types can be handled by a single route.

```rust
use mq_bridge::{msg, Handled, Route, publisher::Publisher, models::Endpoint};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OrderPlaced {
    order_id: u64,
    amount: f64,
}

let input = Endpoint::new_memory("in", 10);
let output = Endpoint::null();
let route = Route::new(input, output)
    .add_handler("order_placed", |msg: OrderPlaced| async move {
        println!("Order #{}: ${}", msg.order_id, msg.amount);
        Ok(Handled::Ack)
    });
route.deploy("typed_handler_example").await.unwrap(); // or use route.run()

let input_publisher = Publisher::new(route.input.clone()).await.unwrap();
let event = OrderPlaced { order_id: 42, amount: 19.99 };
input_publisher.send(msg!(&event, "order_placed")).await.unwrap();
// ...
```

### 2. Compute Handler (Generic)

Use a generic handler to process, transform, or filter messages against the raw `CanonicalMessage`. One handler per-route. Suitable for pipelines, ETL, or side-effect processing.

```rust
use mq_bridge::{CanonicalMessage, Handled, Route, models::Endpoint};

let input = Endpoint::new_memory("in", 10);
let output = Endpoint::new_memory("out", 10);
let handler = |mut msg: CanonicalMessage| {
    msg.set_payload_str(format!("processed: {}", msg.get_payload_str()));
    async move { Ok(Handled::Publish(msg)) }
};
let route = Route::new(input, output).with_handler(handler);
route.deploy("compute_handler_example").await.unwrap();
// ...
```


### 3. Direct Endpoint Usage (Manual Control)

Use `send` / `send_batch` and `receive` / `receive_batch` directly on endpoints. Gives full manual control over batching, commit, concurrency, and sequencing. Useful for advanced scenarios or integration with external async runtimes.

```rust
use mq_bridge::endpoints::memory::{MemoryConsumer, MemoryPublisher};
use mq_bridge::{CanonicalMessage, traits::MessageDisposition};

let publisher = MemoryPublisher::new_local("my_topic", 100);
let mut consumer = MemoryConsumer::new_local("my_topic", 100);
let msg = CanonicalMessage::new(b"hello world".to_vec(), None);
publisher.send(msg).await.unwrap();
let received = consumer.receive().await.unwrap();
// Process the message...
// Acknowledge (required for most endpoints):
(received.commit)(MessageDisposition::Ack).await.unwrap();

// Batch variant:
let batch = consumer.receive_batch(10).await.unwrap();
batch.commit(vec![MessageDisposition::Ack; batch.messages.len()]).await.unwrap();
```

## Extending mq-bridge

See **[EXTENDING.md](EXTENDING.md)** for the full guide, with worked examples in
all three languages.

- **Custom Endpoints:** Implement the `CustomEndpointFactory` trait and register it with
  `extensions::register_endpoint_factory`. Any endpoint key mq-bridge does not recognise is
  looked up in that registry, so a registered `pulsar` factory makes `input: { pulsar: {...} }`
  work with no core change. Endpoints for transports we do not want in this repository's
  dependency tree live in their own crates.
- **Custom Middleware:** Implement the `CustomMiddlewareFactory` trait and register it with
  `extensions::register_middleware_factory`; use it as `custom: { name, config }` in an
  endpoint's `middlewares` list.
- **From Python / Node:** `register_endpoint` / `register_middleware` (Python) and
  `registerEndpoint` / `registerMiddleware` (Node) take a host-language object instead of a
  Rust type, for endpoints that only have a Python/JS SDK.
- **Typed Handlers:** Use `TypeHandler` to add new message types and logic.

## Configuration
- All routes, endpoints, and middleware are defined via YAML, JSON, or environment variables.
- See [CONFIGURATION.md](CONFIGURATION.md) for a full reference and examples.

## Example: Route Lifecycle
1. Define a route either as json or code
2. Create endpoints and apply middleware
3. Attach a handler (optional)
4. Deploy or run the route (spawns async workers)
5. Inject or receive messages
6. Route processes, transforms, and delivers messages according to config and handler logic

## More Information
- See the README for usage patterns and code examples.
- See [CONFIGURATION.md](CONFIGURATION.md) for configuration details.
- See the source code for trait definitions and extension points.
