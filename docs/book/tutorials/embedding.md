# Embed the library (Rust / Python / Node)

The core engine is a Rust library, and the **same engine** ships as native bindings
for **Python** and **Node.js**. The Tokio runtime, broker I/O, routing, and batching
all stay in Rust; the binding is a thin layer for handlers and configuration. Behaviour
and reliability match the Rust engine regardless of which language calls in.

This page picks up where the [library quick start](../getting-started/library-quick-start.md)
ends (install lines are there). The exact constructor and type names are in the
[Language bindings API](../reference/bindings.md).

## Design the route as config

The natural way to embed the library is to **design the route as config**, then load that
exact config from your code: hand-write the YAML, or build and test the route in the
[desktop UI](../getting-started/desktop-ui.md) and export it. The running route behaves
identically to the `mqb --config` form, and every field in the
[connector pages](../connectors/README.md) and the
[middleware reference](../engine/reference.md) applies unchanged.

```yaml
# routes.yaml
routes:
  orders:
    input:
      kafka: { url: "localhost:9092", topic: "orders", group_id: "orders-service" }
    output:
      middlewares:
        - retry: { max_attempts: 5 }
      mongodb: { url: "mongodb://localhost:27017", database: "shop", collection: "orders" }
```

## Handlers

A handler runs your business logic between input and output. Return a message to publish it,
return nothing to acknowledge and drop the input, or raise/throw to fail it (retryable or not).

**Python** — handlers are synchronous; `run()` blocks, `start()` returns:

```python
from mq_bridge import Route, NonRetryableError

def handle(message):
    order = message.json()
    if order.get("amount", 0) <= 0:
        raise NonRetryableError("invalid amount")
    return message.with_json({**order, "checked": True})

route = Route.from_file("routes.yaml", "orders").with_handler(handle)
route.start()   # or route.run() to block, or `with route:` to scope it
```

**Node.js**

```js
import { Message, Route } from "mq-bridge";

const route = Route.fromFile("routes.yaml", "orders");
route.withHandler(async (message) => {
  const order = message.json();
  return Message.fromJson({ ...order, checked: true });
});
route.start();
```

**Typed dispatch** — `add_handler` / `addHandler` routes on the message's `kind` metadata field
and hands you decoded JSON:

```python
route.add_handler("order.created", lambda order: {"seen": order["order_id"]})
```

## Publishing into a route

A `Publisher` sends to any endpoint — for example the route's input, to feed it from your own
code:

```python
from mq_bridge import Publisher

publisher = Publisher.from_config({"kafka": {"url": "localhost:9092", "topic": "orders"}})
publisher.send_json({"order_id": 42}, {"kind": "order.created"})
```

Node.js has the same shape with `Publisher.fromConfig(...)` and `await publisher.sendJson(...)`.

## Rust

The Rust crate exposes the full engine: `Route`, `Endpoint`, `Publisher`, the handler types, and
the `MessageConsumer` / `MessagePublisher` traits. A config file loads into
`mq_bridge::models::Config` (a map of route name → `Route`), or build routes in code:

```rust
use mq_bridge::{models::Endpoint, CanonicalMessage, Handled, Route};

let handler = |mut msg: CanonicalMessage| async move {
    msg.set_payload_str(format!("handled {}", msg.get_payload_str()));
    Ok(Handled::Publish(msg))
};
let route = Route::new(Endpoint::new_memory("in", 200), Endpoint::new_memory("out", 200))
    .with_handler(handler);
route.deploy("my_route").await?;
```

### Calling a route like a function

A `Caller` deploys a route whose input is an in-process `memory` topic and waits for its
answer, so code that needs the route's result looks like a query:

```rust
use mq_bridge::Caller;

// `enrich` in routes.yaml: input `memory: { topic: enrich }` (with a `lookup`, a
// handler, any middleware), output ending in `response: {}`.
let enrich = Caller::from_file("routes.yaml", "enrich").await?;
let answer = enrich.call(r#"{"customer_id": 7}"#.into()).await?;
```

Calls made at the same time are read as one batch, so a batched `lookup` or an inference
endpoint serves them together. See [Request / reply](request-reply.md#calling-a-route-from-rust).

### Typed handlers and sending commands

`TypeHandler` picks a handler by the message's `kind` metadata field and deserializes the
payload into your type:

```rust
use mq_bridge::type_handler::TypeHandler;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct CreateUser { id: u32, username: String }

let typed_handler = TypeHandler::new()
    .add("create_user", |cmd: CreateUser| async move {
        println!("create_user {} {}", cmd.id, cmd.username);
        // () maps to Handled::Ack
    });
let route = Route::new(input, output).with_handler(typed_handler);

// Publish into the route's input; msg! sets the `kind` field.
let input_publisher = Publisher::new(route.input.clone()).await?;
input_publisher.send(msg!(&CreateUser { id: 1, username: "test".into() }, "create_user")).await?;
```

### CQRS-style flows

Routes and typed handlers can act as a command bus and an event bus without becoming a domain
framework. A command route handles the write side and emits an event; downstream routes
subscribe to those events to update read models:

```rust
// Write side: handle the command, emit an event
let command_bus = TypeHandler::new()
    .add("submit_order", |cmd: SubmitOrder| async move {
        let evt = OrderSubmitted { id: cmd.id };
        Ok(Handled::Publish(msg!(evt, "order_submitted")))
    });

// Read side: project the event
let projection = TypeHandler::new()
    .add("order_submitted", |evt: OrderSubmitted| async move {
        // update read database / cache
        Ok(())
    });
```

## See also

- [Language bindings API](../reference/bindings.md) — the reference for this material.
- [Writing endpoints & middleware](../engine/extending.md) — plugging your own endpoint or
  middleware into the engine from Rust, Python, or Node.
- [Core concepts](../getting-started/concepts.md) and [Learn the architecture](../engine/architecture.md) — the handler and message model.
- [The three ways to run it](../getting-started/run-forms.md) — library vs CLI-server vs desktop.
