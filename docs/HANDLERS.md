# Handlers and streaming in Rust

This page covers the Rust API around a route: blocking handlers, emitting several messages per
input, cancelling by correlation id, and reading a streamed answer. For YAML fields, see the
[reference](https://marcomq.github.io/mq-bridge/engine/reference.html). The code blocks on this
page are compiled as doctests.

| You need | Use |
|---|---|
| An async function from message to message | `Route::with_handler` with a closure |
| `&mut` state or blocking work, such as local inference | `ThreadedHandler` |
| Several outputs per input while the handler runs | `ThreadedHandler::with_emitter` and `ctx.emit` |
| Named targets (since 1.7.0) | `ThreadedHandler::builder` and `ctx.output` |
| Send from a plain OS thread (since 1.7.0) | `Publisher::blocking_send` |
| All routes wired up in one place (since 1.7.0) | `Topology` |
| Stop work for one request | `CancelHandle::cancel` with its `correlation_id` |
| One answer to a request, without a route | `Caller` |
| An answer read token by token, without a route (since 1.7.0) | `StreamCaller` |
| Your own source that produces the same tokens (since 1.7.0) | `StreamToken` |

## Blocking handlers

A `ThreadedHandler` runs a `BlockingHandler` on one dedicated OS thread. It needs no `Mutex` and
does not block a Tokio worker.

```rust,no_run
use mq_bridge::blocking_handler::{HandlerContext, ThreadedHandler};
use mq_bridge::models::Endpoint;
use mq_bridge::{CanonicalMessage, Handled, HandlerError, Publisher, Route};

# async fn example() -> anyhow::Result<()> {
let memory = |topic: &str| -> serde_json::Result<Endpoint> {
    serde_json::from_value(serde_json::json!({"memory": {"topic": topic}}))
};

let tokens = Publisher::new(memory("tokens")?).await?;
let handler = ThreadedHandler::with_emitter(
    |msg: CanonicalMessage, ctx: &HandlerContext| -> Result<Handled, HandlerError> {
        for word in msg.get_payload_str().split_whitespace() {
            if ctx.is_cancelled() {
                return Ok(Handled::Ack);
            }
            ctx.emit(CanonicalMessage::from(word))?;
        }
        Ok(Handled::Publish(CanonicalMessage::from("done")))
    },
    tokens.inner(),
);
let cancel = handler.cancel_handle();

Route::new(memory("requests")?, memory("results")?)
    .with_handler(handler)
    .deploy("words")
    .await?;

cancel.cancel("turn-7");
# Ok(())
# }
```

- **One message at a time.** Messages run in arrival order on the one thread, so route
  `concurrency` does not run this handler in parallel.
- **The return value goes to the route's output.** `Handled::Publish` sends a message there and
  `Handled::Ack` sends nothing.
- **`ctx.emit` sends right away and waits for the publisher.** It is not tied to the input's
  commit, so an input that is redelivered emits its messages again.
- **An emitted message inherits the input's `correlation_id`** unless it already has one.
- **`emit` has one target.** It fails without an emit target. For more targets, use named outputs.
- **A panic becomes a non-retryable error.** The thread keeps serving later messages.

### Named outputs

Since 1.7.0, a handler can get named `Publisher`s. Create them where you wire up the routes, so
that handlers only know a name:

```rust,no_run
use mq_bridge::blocking_handler::{HandlerContext, ThreadedHandler};
use mq_bridge::models::Endpoint;
use mq_bridge::{CanonicalMessage, Handled, HandlerError, Publisher};

# async fn example() -> anyhow::Result<()> {
let tokens = Publisher::new(Endpoint::new_memory("tokens", 1024)).await?;
let reports = Publisher::new(Endpoint::new_memory("reports", 1024)).await?;

let background = reports.clone();
std::thread::spawn(move || background.blocking_send(CanonicalMessage::from("ready")));

let handler = ThreadedHandler::builder(
    |msg: CanonicalMessage, ctx: &HandlerContext| -> Result<Handled, HandlerError> {
        ctx.emit(msg)?;
        ctx.output("reports")?.blocking_send(CanonicalMessage::from("sent"))?;
        Ok(Handled::Ack)
    },
)
.emit_to("tokens", tokens)
.output("reports", reports)
.build();
let cancel = handler.cancel_handle();
# drop(cancel);
# Ok(())
# }
```

- **`blocking_send` works without a handler**, from a plain OS thread. The `Publisher` holds only a
  handle to the runtime it was created on and does not keep it alive. That runtime must stay alive
  and be driven until the sending threads finish: use a multi-thread runtime, or keep a
  current-thread runtime inside `block_on`. Synchronous code keeps the runtime in a variable:

  ```rust,no_run
  # use mq_bridge::{models::Endpoint, CanonicalMessage, Publisher};
  # fn main() -> anyhow::Result<()> {
  let runtime = tokio::runtime::Runtime::new()?;
  let publisher = runtime.block_on(Publisher::new(Endpoint::new_memory("reports", 1024)))?;
  let sender = std::thread::spawn(move || publisher.blocking_send(CanonicalMessage::from("ready")));
  sender.join().expect("the sender panicked")?;
  drop(runtime);
  # Ok(())
  # }
  ```

- **`blocking_send` returns once the target accepted the message**, so sends from one thread stay in
  order. It panics in an async context. Use `send` there.
- **`blocking_send` adds no `correlation_id`**, unlike `ctx.emit`, so a cancel does not skip control
  messages or reports sent through it.
- **`ctx.output` fails for an undeclared name**, and the error lists the declared ones. A failed
  send is an error too. If a report must never fail the message, log the error instead of using `?`.
- **The handler's thread starts at `build()`**, so `cancel_handle()` is available before any route
  is deployed. Dropping the `ThreadedHandler` ends the thread and drops the handler there.

### Cancelling

`CancelHandle` is cheap to clone and works from any thread. A queued message with a cancelled id
is acked without running the handler. The running handler sees `ctx.is_cancelled()` turn `true`
and has to stop by itself. The last 1024 cancelled ids are remembered, so use a new id per request.

## One place for the wiring

A `Topology` (since 1.7.0) declares routes that belong together. Build the handlers first, so that
their cancel handles exist, and hand them over finished:

```rust,no_run
use mq_bridge::blocking_handler::{HandlerContext, ThreadedHandler};
use mq_bridge::models::Endpoint;
use mq_bridge::{CanonicalMessage, Handled, HandlerError, Publisher, Route, Topology};

# async fn example() -> anyhow::Result<()> {
let topic = |name| Endpoint::new_memory(name, 1024);
let reports = Publisher::new(topic("reports")).await?;
let worker = ThreadedHandler::builder(
    |msg: CanonicalMessage, ctx: &HandlerContext| -> Result<Handled, HandlerError> {
        ctx.output("reports")?.blocking_send(msg)?;
        Ok(Handled::Ack)
    },
)
.output("reports", reports)
.build();

let deployed = Topology::new("app")
    .route("app_worker", Route::handled_by(topic("work"), worker))
    .route(
        "app_ingest",
        Route::new(topic("incoming"), topic("work")).with_batch_size(1),
    )
    .deploy()
    .await?;

println!("{}", deployed.diagram());
deployed.stop().await;
# Ok(())
# }
```

- **`Route::handled_by` is a route that ends in its handler.** It has no output, so a message
  returned as `Handled::Publish` is discarded. Use `Route::new(input, output).with_handler(..)` when
  the return value should go somewhere.
- **Routes start in declaration order and stop in reverse.** Declare consumers first, so that a
  source stops before what reads from it. `stop_order` lists routes to stop first.
- **A declaration is checked before anything starts.** A repeated name and an unknown name in
  `stop_order` are errors.
- **A failed start stops what this deploy started**, in stop order. `TopologyError` names the route.
  A route of an earlier deployment under the same name is replaced when its turn comes, so a failed
  redeploy can leave the earlier deployment's later routes running.
- **`stop()` only stops this deployment.** It skips a route deployed again under the same name.
- **`stop()` does not drain.** Messages waiting in a topic between two routes stay there.
- **`diagram()` lists each route with its declared outputs.** It shows what was declared. A handler
  can still send elsewhere, so pass it the same `Publisher` clones you declare.
- **Dropping a `DeployedTopology` leaves the routes running.** `Route::stop` by name keeps working.

## The token format

An `openai_chat` or `http_bulk` endpoint with `stream_to` sends each piece of text as one message.
Each message's payload is the text, and its metadata says where it belongs:

| Metadata key | Value |
|---|---|
| `correlation_id` | The request |
| `mqb.stream.attempt` | A new id for each attempt at the request |
| `mqb.stream.index` | 0, 1, 2, … within one attempt |
| `mqb.stream.end` | `true` on the attempt's last token, else `false` |
| `mqb.stream.status` | Last token only: `complete`, `cancelled` or `error`. With `error`, the payload is the error text |
| `mqb.stream.stop_reason` | Last token only, if the server sent one, such as `stop` or `length` |

- With a `retry` middleware, a failed attempt is followed by tokens from index 0 under a new
  `mqb.stream.attempt`. Discard the earlier attempt's text when the attempt changes.
- A failing `stream_to` does not fail the request.
- Do not put a collecting middleware such as `buffer` on `stream_to`.

The [connector page](https://marcomq.github.io/mq-bridge/connectors/openai.html#streaming-the-answer)
has the endpoint's fields and the remaining edge cases.

## Reading a streamed answer

`StreamCaller` (since 1.7.0) sends a request and returns its tokens. A reply always ends with one
last token, also when the request dies before the server sent one.

```rust,no_run
use mq_bridge::models::Endpoint;
use mq_bridge::{CanonicalMessage, StreamCaller, StreamEnd};

# async fn example() -> anyhow::Result<()> {
let endpoint: Endpoint = serde_json::from_value(serde_json::json!({"openai_chat": {
    "url": "http://localhost:11434",
    "model": "llama3.2",
    "prompt": "${message:payload}",
    "stream_to": {"memory": {"topic": "chat.tokens"}},
    "cancel_from": {"memory": {"topic": "chat.cancel"}},
}}))?;
let chat = StreamCaller::new("chat", endpoint).await?;

let mut reply = chat.start("turn-7", CanonicalMessage::from("Hello")).await?;
while let Some(token) = reply.next().await {
    match token.end() {
        None => print!("{}", token.text()),
        Some(StreamEnd::Failed) => eprintln!("failed: {}", token.text()),
        Some(_) => println!(),
    }
}
# Ok(())
# }
```

- One caller serves parallel requests under different correlation ids. Clone it for other tasks.
- Two callers must not share a `stream_to` or `cancel_from` topic, because they would take each
  other's messages.
- `start` fails while a reply with the same id is still open.
- `cancel_handle()` stops a request from another task. It is `None` without `cancel_from`.
- Dropping an unfinished reply cancels its request if the endpoint has `cancel_from`.

A local source can write the same format with `StreamToken::piece` and `StreamToken::last`, so
readers need no special case for it.
