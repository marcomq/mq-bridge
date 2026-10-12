# Extending mq-bridge

How to add an endpoint or a middleware that lives **outside** this repository — in
your own Rust crate, or written directly in Python or JavaScript.

Everything here plugs into the same two extension points:

| You want | Implement | Register with |
|---|---|---|
| A new source/sink (Pulsar, an internal broker, a SaaS API) | `CustomEndpointFactory` | `register_endpoint_factory` |
| A step that inspects/rewrites/drops messages in flight | `CustomMiddlewareFactory` | `register_middleware_factory` |

Registration is process-global and keyed by name. **Register before starting any
route that names it**; registering the same name twice is an error, so each
factory needs its own name.

> Looking for the built-in endpoints and middleware instead? See
> [REFERENCE.md](REFERENCE.md). To ship a Rust endpoint as a loadable library
> that Rust, Python and Node.js hosts can all load — an endpoint, a middleware
> or both — see [PLUGINS.md](PLUGINS.md).

---

## How a custom name reaches your code

Once `pulsar` is registered, both of these configs route to your factory:

```yaml
# Shorthand: any endpoint key mq-bridge does not recognise is looked up
# in the custom-endpoint registry.
input:
  pulsar:
    url: "pulsar://localhost:6650"
    topic: "orders"
```

```yaml
# Explicit form. Prefer this if you validate configs against
# mq-bridge.schema.json, which cannot know your custom key.
input:
  custom:
    name: "pulsar"
    config:
      url: "pulsar://localhost:6650"
      topic: "orders"
```

Middleware is always the explicit form, in any endpoint's `middlewares` list:

```yaml
output:
  file: { path: "out.jsonl" }
  middlewares:
    - custom:
        name: "redact"
        config: { fields: ["ssn"] }
```

Whatever sits under the endpoint key (or under `config:`) is handed to your
factory verbatim as JSON. mq-bridge does not interpret it.

---

## Rust: an endpoint in your own crate

This is the path for a real transport — it has no FFI overhead and full access
to the async ecosystem. Keeping it in a separate crate means its dependency tree
never lands in mq-bridge's build or CI.

```toml
# Cargo.toml
[dependencies]
mq-bridge = { version = "1", default-features = false }
pulsar = "6"
async-trait = "0.1"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt", "macros"] }
```

```rust
use std::sync::Arc;

use async_trait::async_trait;
use mq_bridge::errors::ConsumerError;
use mq_bridge::traits::{
    BatchCommitFunc, CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher,
};
use mq_bridge::{CanonicalMessage, ReceivedBatch, SentBatch};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PulsarConfig {
    url: String,
    topic: Option<String>,
    #[serde(default)]
    subscription: Option<String>,
}

#[derive(Debug)]
struct PulsarFactory;

#[async_trait]
impl CustomEndpointFactory for PulsarFactory {
    async fn create_consumer(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let mut config: PulsarConfig = serde_json::from_value(config.clone())?;
        // Convention: default the topic to the route name, like kafka/nats do.
        let topic = config.topic.take().unwrap_or_else(|| route_name.to_string());
        Ok(Box::new(
            PulsarConsumer::connect(&config.url, &topic, config.subscription.as_deref()).await?,
        ))
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let mut config: PulsarConfig = serde_json::from_value(config.clone())?;
        let topic = config.topic.take().unwrap_or_else(|| route_name.to_string());
        Ok(Box::new(PulsarPublisher::connect(&config.url, &topic).await?))
    }
}

/// Call once, before starting any route that uses `pulsar`.
pub fn register() -> anyhow::Result<()> {
    mq_bridge::extensions::register_endpoint_factory("pulsar", Arc::new(PulsarFactory))
}
```

Both methods default to "unsupported", so a source-only endpoint just omits
`create_publisher` and gets a clear error if someone configures it as an output.

### The consumer contract

```rust
#[async_trait]
impl MessageConsumer for PulsarConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        let messages = self.pull(max_messages).await?;      // your client
        if messages.is_empty() {
            // "Nothing right now" — NOT end of stream. The route backs off and
            // retries, and treats this as the drain signal under exit_on_empty.
            return Ok(ReceivedBatch::empty());
        }

        let acker = self.acker.clone();
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                for disposition in dispositions {
                    match disposition {
                        MessageDisposition::Nack => acker.nack().await?,
                        _ => acker.ack().await?,
                    }
                }
                Ok(())
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
```

Three rules that decide whether your endpoint behaves well:

1. **Never block forever on an empty source.** Return `ReceivedBatch::empty()`
   instead. A consumer that parks indefinitely makes `exit_on_empty` / `--drain`
   hang — the single most common bug in a new endpoint.
2. **`commit` gets one disposition per message, in order.** It is what advances
   the broker offset. Do not ack at read time unless the transport gives you no
   choice (and say so in your docs — it downgrades the route to at-most-once).
3. **Classify your errors.** `ConsumerError::Connection` makes the route
   reconnect; `ConsumerError::Permanent` shuts it down (use it for poison data,
   not for a dropped socket); `ConsumerError::EndOfStream` ends it cleanly.

### The publisher contract

```rust
#[async_trait]
impl MessagePublisher for PulsarPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, mq_bridge::errors::PublisherError> {
        // Hand the client the whole batch, then flush once. Do NOT `await` a
        // single-message send per message — that is the difference between
        // ~100k and ~1M messages/s.
        let payloads = messages.iter().map(|m| m.payload.clone());
        self.producer.send_all(payloads).await?;
        self.producer.flush().await?;
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
```

If your client has no batch API, start every send before awaiting any of them
(collect the futures, then `join_all`) rather than awaiting each in turn — the
point is to keep one in-flight request from gating the next.

`PublisherError::Retryable` is retried by a `retry` middleware;
`PublisherError::NonRetryable` goes straight to a `dlq` if one is configured.

### Optional lifecycle hooks

`on_connect_hook` runs once after the endpoint is created and before the route
reports itself ready — use it to warm a pool or create tables.
`on_disconnect_hook` runs during shutdown. Both are optional.

### Using it

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mq_bridge_pulsar::register()?;
    mq_bridge::Route::from_file("routes.yaml", Some("pulsar_to_file"))?
        .run("pulsar_to_file")
        .await
}
```

---

## Python

```python
import mq_bridge

class PulsarSource:
    def __init__(self, config):
        import pulsar
        self.pulsar = pulsar
        client = pulsar.Client(config["url"])
        self.consumer = client.subscribe(config["topic"], config.get("subscription", "mq-bridge"))
        self.pending = []

    def receive_batch(self, max_messages):
        # Return [] / None for "nothing right now"; raise StopIteration to end.
        batch = []
        self.pending = []
        for _ in range(max_messages):
            try:
                message = self.consumer.receive(timeout_millis=100)
            # pulsar-client 3.4.x exposes an expired receive as pulsar.Timeout.
            except self.pulsar.Timeout:
                break
            self.pending.append(message)
            batch.append(message.data())
        return batch

    def commit(self, dispositions):
        for message, disposition in zip(self.pending, dispositions):
            if disposition == "nack":
                self.consumer.negative_acknowledge(message)
            else:
                self.consumer.acknowledge(message)

    def close(self):
        self.consumer.close()

mq_bridge.register_endpoint("pulsar", lambda route_name, config: PulsarSource(config))

route = mq_bridge.Route.from_config({
    "exit_on_empty": True,
    "input": {"pulsar": {"url": "pulsar://localhost:6650", "topic": "orders"}},
    "output": {"file": {"path": "orders.jsonl"}},
}, "pulsar_to_file")
route.run()
```

**The object.** `factory(route_name, config)` returns it; which methods it has
decides what it can be:

| Method | Makes it usable as | Notes |
|---|---|---|
| `receive_batch(max_messages)` | an `input` | Returns `Message`/`bytes`/`str`/JSON values, or `None`/`[]` for idle. Raise `StopIteration` for end of stream — only here; from any other method it is an ordinary error. |
| `commit(dispositions)` | — | Optional. One `"ack"`/`"nack"` string per message in the batch. |
| `send_batch(messages)` | an `output` | Receives `Message` objects. |
| `close()` | — | Optional. Called when the route releases the endpoint. |

Configuring a sink-only object as an `input` fails at route startup with a
message naming the missing method.

**Errors.** Raise `mq_bridge.RetryableError` to have a failed `send_batch`
retried; anything else is non-retryable and reaches a `dlq`. On the read side any
exception triggers a reconnect, except `mq_bridge.NonRetryableError`, which shuts
the route down instead of re-reading data that cannot heal.

**Threading.** Each endpoint instance gets its own thread, and every call into it
— construction included — happens there. Your object never sees concurrent calls,
even with `concurrency > 1`, so it does not need to be thread-safe. It also means
one endpoint is one Python thread's worth of throughput.

**Releasing a name.** The registry is process-global and rejects a duplicate
name. `mq_bridge.unregister_endpoint(name)` (and `unregister_middleware(name)`)
drops the factory once the routes using it have stopped, freeing the name and the
reference held on your factory object. Both return `True` when a registration was
removed.

### Python middleware

```python
class Redact:
    def __init__(self, config):
        self.fields = config.get("fields", [])

    def on_send(self, messages):
        out = []
        for message in messages:
            data = message.json()
            if data.get("internal"):
                out.append(None)          # drop this one
                continue
            for field in self.fields:
                data.pop(field, None)
            out.append(mq_bridge.Message.from_json(data, message.metadata))
        return out                        # one slot per input message

mq_bridge.register_middleware("redact", lambda route_name, config: Redact(config))
```

- `on_receive(messages)` applies when the middleware sits on an **input**
  endpoint; `on_send(messages)` when it sits on an **output**. Implement either
  or both — a side you leave out passes through untouched.
- Both **must return exactly one item per input message**: a `Message` to keep it
  (rewritten or not), or `None` to drop it. That fixed length is what keeps
  acknowledgements aligned with the source batch — a dropped message is acked at
  the source, so it is not redelivered forever.

---

## Node

```js
const mqb = require("mq-bridge");

mqb.registerEndpoint("pulsar", (routeName, config) => {
  const client = new Pulsar.Client({ serviceUrl: config.url });
  let consumer;
  let pending = [];
  return {
    async receiveBatch(maxMessages) {
      consumer ??= await client.subscribe({ topic: config.topic, subscription: "mq-bridge" });
      const batch = [];
      pending = [];
      for (let i = 0; i < maxMessages; i += 1) {
        let message;
        try {
          message = await consumer.receive(100);
        } catch (error) {
          // pulsar-client 1.18.x reports an expired receive as `TimeOut`.
          if (error instanceof Error && error.message.endsWith(": TimeOut")) break;
          throw error;
        }
        pending.push(message);
        batch.push(message.getData());
      }
      return batch;                       // [] means "nothing right now"
    },
    async commit(dispositions) {
      for (let i = 0; i < dispositions.length; i += 1) {
        if (dispositions[i] === "nack") consumer.negativeAcknowledge(pending[i]);
        else await consumer.acknowledge(pending[i]);
      }
      pending = [];
    },
    async close() {
      await client.close();
    },
  };
});

const route = mqb.Route.fromConfig({
  input: { pulsar: { url: "pulsar://localhost:6650", topic: "orders" } },
  output: { file: { path: "orders.jsonl" } },
}, "pulsar_to_file");
route.start();
```

The shape matches Python, with JS names and promises: `receiveBatch`,
`commit`, `sendBatch`, `close`; `registerMiddleware` with `onReceive` / `onSend`.
Throw `mqb.EndOfStream` (instead of `StopIteration`) to end a source, and set
`err.retryable = true` on a thrown error to have it retried.

### Keep the event loop free

Your endpoint runs **in JavaScript**, so mq-bridge has to hand work back to the
Node event loop to call it. Anything that blocks the JS thread starves those
calls:

```js
route.start();
// ...your app runs, the event loop turns, the endpoint gets called...
route.stop();
await new Promise((r) => setTimeout(r, 50));   // let the loop drain the teardown
route.join();
```

Calling `route.join()` immediately after `stop()` blocks the loop while the route
is still finishing, which costs a 5s shutdown timeout — and if the route still
needs the endpoint, it deadlocks outright. This does not affect normal
event-loop-driven apps; it only bites when you block the thread on purpose.

For the same reason the host object is built lazily, on first use rather than at
`start()`: a factory dispatched from inside `start()` could never be serviced.

A registered endpoint also keeps the Node process alive (it is a live resource,
like an open server). Once the routes using it have stopped, release it so the
process can exit on its own:

```js
mqb.unregisterEndpoint(name);     // mqb.unregisterMiddleware(name) for middleware
```

Both return `true` when a registration was removed, `false` when the name was
not registered. `process.exit()` also works, but it skips pending flushes and
`close()` hooks.

---

## Choosing a language

| | Rust | Python / Node |
|---|---|---|
| Throughput | Full — no FFI hop | One host thread per endpoint; fine for I/O-bound sources |
| Reuse | Rust crates | The host ecosystem (an official SDK that has no Rust equivalent) |
| Distribution | A crate users add and `register()` | A few lines in the app that already exists |

Reach for a host-language endpoint when the vendor ships a good Python/Node SDK
and no Rust one, or when the endpoint is glue specific to your deployment. Write
it in Rust when it is a real transport other people will want, when it must keep
up with a high-throughput route, or when you want to publish it — as a crate, or
as a native plugin every language can load ([PLUGINS.md](PLUGINS.md)).

---

## See also

- [PLUGINS.md](PLUGINS.md) — ship a Rust endpoint or middleware as a loadable native plugin
- [REFERENCE.md](REFERENCE.md) — every built-in endpoint and middleware
- [ARCHITECTURE.md](ARCHITECTURE.md) — how routes, batching and commits fit together
- [CONFIGURATION.md](CONFIGURATION.md) — config loading, env vars, schema validation
