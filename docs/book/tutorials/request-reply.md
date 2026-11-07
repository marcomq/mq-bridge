# Request / reply

`mq-bridge` supports request-response patterns for interactive services such as web
APIs. A client sends a request and waits for the matching response, while the bridge
keeps the correlation details away from your handler.

The `response` output is the most direct option and the safest one under concurrency:
the response stays in the same execution context as the request, so concurrent
requests do not share a reply queue or race on correlation IDs.

## How it works

1. An input endpoint that supports request-reply (like `http`) receives a request.
2. The message passes through the route's processing chain — this is where you attach
   a **handler** to process the request and generate a response payload.
3. The final message is sent to the `output`.
4. If the output is `response: {}`, the bridge sends the message back to the original
   input source, which delivers it as the reply (e.g. the HTTP response body).

> Not every backend can do request-reply. It is only supported/tested for endpoints
> that natively support or emulate it. SQLx, files, AWS, IBM MQ, and Sled do **not**
> support request-reply. Check the Request-Reply column in
> [Consumer vs. subscriber, and nack support](../reference/endpoints.md#consumer-vs-subscriber-and-nack-support).

## Example: MongoDB request/response

A sender writes a request document to MongoDB and waits for a reply. The bridge reads
the document, runs the handler, and writes the result back to the reply collection:

```yaml
mongo_responder:
  input:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "requests"
  output:
    # 'response' sends the processed message back to the reply collection
    # (whatever reply_to the sender set).
    response: {}
```

## Attaching the handler (Rust)

The `response` output only echoes what the pipeline produced — you supply the logic by
attaching a handler to the route:

```rust
use mq_bridge::models::Handled;
use mq_bridge::CanonicalMessage;

let handler = |mut msg: CanonicalMessage| async move {
    let request_body = String::from_utf8_lossy(&msg.payload);
    let response_body = format!("Handled response for: {}", request_body);
    msg.payload = response_body.into();
    Ok(Handled::Publish(msg))
};
// load the route from YAML, then attach `handler` to its output and run it.
```

See [Embed the library](embedding.md) for the full load-and-run scaffolding, and
[Core concepts](../getting-started/concepts.md) for the handler model
(`CommandHandler` / `EventHandler` / `TypeHandler`).

## Calling a route from Rust

`Caller` turns a route into an async function. It deploys the route, puts each request
on the route's `memory` input and waits for the answer:

```yaml
enrich:
  concurrency: 4
  input:
    memory: { topic: "enrich" }
    middlewares:
      - lookup:
          from:
            sqlx:
              url: "postgres://localhost/crm"
              table: "customers"
              lookup_query: "SELECT id, tier FROM customers WHERE id IN (${payload:customer_id}::bigint)"
          into: customer
  output:
    response: {}
```

```rust
use mq_bridge::Caller;

let enrich = Caller::from_file("routes.yaml", "enrich").await?;
let answer = enrich.call(r#"{"customer_id": 7}"#.into()).await?;

// Elsewhere in the program:
let enrich = Caller::get("enrich").expect("deployed at startup");
```

- `call` returns the answer; `send` returns `Sent::Ack` when the output only accepts the
  message (a plain sink), and `call` fails then.
- Calls made at the same time are read as one batch: a batched `lookup` queries once for
  all of them, an inference endpoint gets them together.
- `call_many(messages)` sends several messages as one batch and returns one result per
  message, in order; one failing does not fail the others. `send_many` is its `send`.
- A message that fails comes back as an error at once, with the error text, after the
  route's `retry` middleware if it has one. The route keeps running.
- The wait is bounded by the input's `request_timeout_ms` (default 30 s).
- `parallel` on the input pays off only with many concurrent calls (about 256 and more); at
  16-64 it makes the route slower.
- `Caller::deploy(name, route)` takes a route built in code (with a handler);
  `Caller::new(name, output)` adds the memory input itself. `stop()` ends the route.

The same works without `Caller`: a memory input with `request_reply: true` answers each
failure to its requester, and a `memory` publisher with `request_reply: true` sends and waits.

## Caveats

- If the input does not support responses (e.g. File, SQLx), a message sent to `response` is
  dropped.
- Configure timeouts on the requester side: the bridge's processing time adds latency.
- Middleware that drops metadata (like `correlation_id`) can break the response chain.
- MongoDB request/reply is emulated by polling for the reply; set its timeouts deliberately.

## See also

- [`request` / `response` structural endpoints](../engine/reference.md#structural-endpoints) — the reference definitions.
