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
| Named targets, also from other threads (since 1.7.0) | `Output` and `ThreadedHandler::builder` |
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
- **`emit` has one target.** It fails without an emit target. For more targets, use `Output`.
- **A panic becomes a non-retryable error.** The thread keeps serving later messages.

### Named outputs

An `Output` (since 1.7.0) sends to one endpoint. Create the outputs where you wire up the routes,
so that handlers only know a name:

```rust,no_run
use mq_bridge::blocking_handler::{HandlerContext, Output, ThreadedHandler};
use mq_bridge::models::Endpoint;
use mq_bridge::{CanonicalMessage, Handled, HandlerError};

# async fn example() -> anyhow::Result<()> {
let tokens = Output::new(Endpoint::new_memory("tokens", 1024)).await?;
let reports = Output::new(Endpoint::new_memory("reports", 1024)).await?;

let background = reports.clone();
std::thread::spawn(move || background.send(CanonicalMessage::from("ready")));

let handler = ThreadedHandler::builder(
    |msg: CanonicalMessage, ctx: &HandlerContext| -> Result<Handled, HandlerError> {
        ctx.emit(msg)?;
        ctx.output("reports")?.send(CanonicalMessage::from("sent"))?;
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

- **`Output` works without a handler.** It is `Clone + Send + Sync` and holds its runtime handle, so
  a plain OS thread can use it.
- **`send` returns once the target accepted the message**, so sends from one thread stay in order.
  It blocks and panics in an async context. Use `send_async` there.
- **An `Output` adds no `correlation_id`**, unlike `ctx.emit`, so a cancel does not skip control
  messages or reports sent through it.
- **`ctx.output` fails for an undeclared name**, and the error lists the declared ones. A failed
  send is an error too. If a report must never fail the message, log the error instead of using `?`.
- **The handler's thread starts at `build()`**, so `cancel_handle()` is available before any route
  is deployed. Dropping the `ThreadedHandler` ends the thread and drops the handler there.

### Cancelling

`CancelHandle` is cheap to clone and works from any thread. A queued message with a cancelled id
is acked without running the handler. The running handler sees `ctx.is_cancelled()` turn `true`
and has to stop by itself. The last 1024 cancelled ids are remembered, so use a new id per request.

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
