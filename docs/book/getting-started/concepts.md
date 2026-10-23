# Core concepts

Four terms recur throughout this book.

## Route

A **route** is a named data pipeline that defines a flow from one `input` to one `output`. It
carries optional `middlewares` on either side, a handler, and the tuning knobs `batch_size`,
`concurrency`, and `commit_concurrency_limit`. In config, the top-level keys *are* route names:

```yaml
orders_bridge:                 # <- route name
  input:  { kafka: { topic: "orders", url: "localhost:9092" } }
  output: { nats:  { subject: "orders.processed", url: "nats://localhost:4222" } }
```

## Endpoint

An **endpoint** is a source or sink for messages, written as a single-key object naming the
connector. Transport endpoints talk to a broker or store (`kafka`, `nats`, `sqlx`, `mongodb`,
`file`, `memory`, …); [structural endpoints](../engine/reference.md) (`fanout`,
`switch`, `response`, `ref`, `null`, …) compose other endpoints or shape routing. The same
endpoint type can act as consumer or publisher depending on which side of the route it is on.
See the full catalog in [Endpoints (connectors)](../reference/endpoints.md).

## Middleware

**Middleware** intercepts and processes messages around an endpoint — retries, dead-letter
queues, transformation, deduplication, rate limiting, encryption, metrics. It attaches as a
`middlewares:` list on the input, output, or both. Ordering matters: **a message passes the
list from the bottom up**, on both sides, so on an output `dlq` goes last to catch what the
entries above it failed. See [Middleware](../engine/reference.md) for the full ordering rules,
and [Endpoint, middleware or structural endpoint?](../engine/architecture.md#endpoint-middleware-or-structural-endpoint)
for when to reach for which.

## Handler

A **handler** is a programmatic component for business logic, used when you embed the library
(config-only routes forward messages without one). There are three kinds:

- **`CommandHandler`** — a 1-to-1 or 1-to-0 transformation. It takes a message and can return a
  new message to publish or return as a response.
- **`EventHandler`** — a terminal 1-to-N handler for event consumption. It reads messages
  without removing them for other event handlers, and should not return a response.
- **`TypeHandler`** — a strongly-typed handler. It dispatches on the `kind` metadata field and
  deserializes payloads into concrete Rust types, so handlers don't repeat parsing code.

```rust
let typed_handler = TypeHandler::new()
    .add("create_user", |cmd: CreateUser| async move {
        // handle create_user; Ok(()) maps to Handled::Ack
    })
    .add("delete_user", |cmd: DeleteUser| async move {
        // handle delete_user
    });
let route = Route::new(input, output).with_handler(typed_handler);
```

Handlers return a `Handled` value: `Handled::Ack` (consume), `Handled::Publish(msg)` (forward
`msg` down the publisher chain), and so on. Message selection for `TypeHandler` is driven by the
`kind` metadata field, which `msg!(&value, "kind")` sets. See the
[Embed the library](../tutorials/embedding.md) tutorial and
[Language bindings API](../reference/bindings.md).

## CanonicalMessage

Every handler works with a **`CanonicalMessage`** — the unified message type that decouples
your code from any specific transport. It carries a `payload` (bytes), string `metadata`
headers, and a `message_id` (a UUIDv7 when omitted). Conventional metadata keys: `kind`
(message type / typed routing), `correlation_id` and `reply_to` (request/reply). Keys starting
with `mqb.src.` are reserved for provenance and stripped on input.

## Delivery model

Endpoints default to a **consumer** (queue) pattern; **subscriber** (pub/sub) behaviour is
opt-in per backend. Delivery is **at-least-once** where a durable position or lease exists — so
for ETL, pair it with an [idempotent write](../cookbook/upserts.md) at the sink for effective
exactly-once. Ack/nack and retry/DLQ handling are designed to work with batching; see
[Learn the architecture](../engine/architecture.md#batching-and-concurrency).
