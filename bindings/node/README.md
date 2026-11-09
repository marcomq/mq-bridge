# mq-bridge Node bindings

Native Node.js bindings for `mq-bridge`, built with `napi-rs`.

This package is server-side only. It loads a native `.node` addon and does not run in browsers.

## Build

```sh
npm install
npm run build:basic
```

For the fastest local smoke test, build only the HTTP + middleware surface used
by the example:

```sh
npm run build:ci
npm run example
```

## Quick start: publish a message with no route/config file

For ad hoc testing (e.g. seeding a topic by hand) you don't need a route, a
handler, or a config file — `Publisher.fromConfig` takes a plain object and
`sendJson` resolves once the broker acks it:

```js
const { Publisher } = require("mq-bridge");

(async () => {
  const endpoint = { kafka: { brokers: "localhost:9092", topic: "orders" } };
  const pub = Publisher.fromConfig(endpoint);
  for (let i = 0; i < 5; i++) {
    await pub.sendJson({ orderId: i, amount: i * 10 });
  }
  console.log("published 5 messages");
})();
```

For a truly file-free one-off, paste the same lines into `node -e "..."`.

Swap the `endpoint` object for any other transport (`nats`, `amqp`, `mqtt`,
`mongodb`, `memory`, `file`, ...). `sendJson(data, metadata?, id?)` takes an
optional metadata object as its second argument (e.g. `{ kind: 'order.created' }`).
`Publisher` has no `close()`, so let the script exit once sends finish rather
than reusing a long-lived instance across many short runs.

## Handler shape

```ts
import { Message, Route } from "mq-bridge";

const route = Route.fromStr(config, "orders");

route.withHandler(async (message) => {
  const data = message.json();
  return Message.fromJson({ ok: true, data });
});

route.addHandler("order.created", async (data) => {
  return Message.fromJson({ seen: data });
});

route.start();
```

Middleware is configured through the normal mq-bridge route config.

## Loading config

The constructors mirror the Python bindings — the same capabilities are exposed
in both, with Node using camelCase names where Python uses snake_case:

- `Route.fromFile(path, name?)` loads from a YAML/JSON file
- `Route.fromStr(text, name?)` loads from an in-memory YAML/JSON string
- `Route.fromConfig(obj, name?)` loads from a JS object
- `Publisher.fromFile` / `Publisher.fromStr` / `Publisher.fromConfig` build a publisher endpoint

The `name` is optional: pass it to pick one entry out of a `routes:`/`publishers:`
document, or omit it to treat the whole config as a single bare route/endpoint body.
`fromYaml` / `fromYamlStr` remain as deprecated aliases for `fromFile` / `fromStr`.

## Calling a route like a function

`Caller` deploys a route whose input is an in-process `memory` endpoint and
resolves each call to its answer. Calls made at the same time reach the route
as one batch.

```ts
import { Caller } from "mq-bridge";

// `users` in routes.yaml: an `input` with only a `lookup` middleware, no `output`.
const users = Caller.fromFile("routes.yaml", "users");
const user = await users.call({ path: "/u/7" }); // the parsed JSON answer
const found = await users.callMany([{ path: "/u/7" }, { path: "/u/8" }]);
await users.stop();
```

`call()` sends the fields as a JSON object and resolves to the parsed answer;
`callMany()` resolves like `Promise.allSettled`, one result per request.
`callMessage()` / `callManyMessages()` take and return `Message`s, e.g. to keep
metadata. `Caller.fromFile` / `fromConfig` take the same shapes as `Route`, but a
route without `input` reads from the caller and one without `output` answers with
the message. `Caller.fromRoute(route)` deploys a route built in code, with its handler.

A message the route fails rejects with the error text at once. `send()`
resolves to `null` when the route accepts a message without an answer; `call()`
rejects then.

## Pull-based consumer

`Route` is push-based. To pull messages on your own schedule, use `Consumer`,
which wraps any input endpoint:

```js
import { Consumer } from "mq-bridge";

const consumer = Consumer.fromConfig({ nats: { subject: "orders", url: "nats://localhost:4222" } });

while (!consumer.exhausted) {
  const batch = await consumer.poll(500, 1000); // [] on timeout
  if (batch.length === 0) continue;
  for (const message of batch) handle(message.json());
  await consumer.commit(); // ack only after handling
}
```

`poll(max?, timeoutMs?)` returns up to `max` messages (default 256) without
acking, waiting up to `timeoutMs` milliseconds (omit it to block until a message
arrives); `commit()` acks every batch returned since the last commit. Committing
only after the downstream write succeeds gives at-least-once delivery. `exhausted`
turns `true` once a bounded source (e.g. a file) is drained; streaming brokers
never set it. `Consumer.fromFile` / `fromStr` accept a named entry under a
`consumers:` section or a single bare endpoint body.

`consumer.status()` resolves to a snapshot (`healthy`, `target`, `pending`,
`capacity`, `error`, `details`). `pending` is the broker backlog/lag where the
transport reports it (Kafka offset lag, AMQP queue depth, NATS JetStream
`numPending`), so `pending === 0` is a precise "caught up" check; it's absent
where the broker exposes no backlog (core NATS, MQTT). `consumer.close()`
releases the connection — idempotent, and `poll()`/`status()` reject afterwards.
Node is garbage-collected, so close explicitly rather than waiting for GC.

> **You must call `commit()` — it is not optional.** It is the only thing that
> tells the broker a batch is done. If you keep polling without committing, the
> offset never advances (every message is **re-delivered** on the next run), most
> brokers **stall** once their unacknowledged/prefetch window fills, and the
> uncommitted batches are held in memory so the process **grows unbounded**.
> Commit after each batch you have durably handled; to retry a failed batch,
> simply don't commit it.

### Per-batch tokens: `pollBatch` / `ack` / `nack`

When you need to ack or release **specific** batches (rather than everything since
the last commit), use the token form. `pollBatch(max, timeoutMs)` resolves to
`{ messages, token }`; `ack(token)` commits just that batch, and `nack(token)`
releases it for redelivery (`nack()` with no argument nacks every outstanding
batch). This is the shape a [`dlt`](https://dlthub.com)-style loader wants — poll →
yield → load package commits → `ack(token)`.

```js
const { messages, token } = await consumer.pollBatch(500, 1000); // token === null on timeout
if (token !== null) {            // nothing returned on an idle timeout
  // ... persist the batch downstream ...
  await consumer.ack(token);     // or consumer.nack(token) to redeliver
}
```

Tokens stay outstanding until acked/nacked; `commit()` still acks every
outstanding batch at once, so don't mix the two styles on one consumer. On
cumulative-ack transports (Kafka), acking a later batch would implicitly ack the
earlier ones, so `ack(token)` must follow receive order — acking out of order
rejects; ack the oldest outstanding batch first, or use `commit()`. Transports
that ack each batch individually (NATS JetStream, AMQP, MQTT) accept any order.

> **At-least-once + idempotent merge.** Redelivery (after a nack, a missed
> `commit()`, or an expired broker ack deadline) means a record can arrive twice; a
> downstream loader must dedup on a stable key. `message.id` is globally unique per
> source position (Kafka `partition:offset`, NATS `stream_sequence`, AMQP delivery
> tag) and makes a natural primary key; source cursor fields are also available in
> `message.metadata` (`mqb.src.kafka_topic`/`mqb.src.kafka_offset`, `mqb.src.nats_subject`/`mqb.src.nats_stream_sequence`,
> `mqb.src.amqp_routing_key`/`mqb.src.amqp_delivery_tag`) when its Kafka, NATS, or
> AMQP source config sets `source_metadata: true` (off by default). Keep `batchSize × per-record cost` under
> the smallest broker ack deadline (JetStream `AckWait`, AMQP prefetch/timeout, MQTT
> inflight) or raise it in config. **Kafka has no per-message nack:** `nack` there
> leaves the offset unadvanced, so redelivery happens on the next run/rebalance.

## Custom endpoints and middleware

Register a JS object as an endpoint, and use its name in any route config —
useful when a system has a good Node SDK but no Rust one.

```js
const mqb = require("mq-bridge");

mqb.registerEndpoint("pulsar", (routeName, config) => ({
  async receiveBatch(maxMessages) {
    // [] = nothing right now; throw mqb.EndOfStream when the source is done
    return await pullFrom(config.url, config.topic, maxMessages);
  },
  async commit(dispositions) {},   // optional: one "ack"/"nack" per message
  async close() {},
}));

const route = mqb.Route.fromConfig({
  input: { pulsar: { url: "pulsar://localhost:6650", topic: "orders" } },
  output: { file: { path: "orders.jsonl" } },
}, "ingest");
route.start();
```

Implement `sendBatch(messages)` instead of `receiveBatch` for an output. A
middleware works the same way via `registerMiddleware`, with `onReceive` /
`onSend` hooks that return one slot per input message (`null` drops it).

Because the endpoint runs in JavaScript, **keep the event loop free** while the
route is running — `route.join()` straight after `stop()` blocks the very
dispatch the endpoint needs. Yield first:

```js
route.stop();
await new Promise((r) => setTimeout(r, 50));
route.join();
```

A registered endpoint also keeps the process alive. Once the routes using it
have stopped, `unregisterEndpoint(name)` (or `unregisterMiddleware(name)`)
releases it so the process can exit on its own — unlike `process.exit()`, which
skips pending flushes and `close()` hooks.

**Full guide, including the Rust path:** [EXTENDING.md](../../docs/EXTENDING.md).

## Logging

By default the Rust core's internal `tracing` events go nowhere. Call
`initLogging` once at startup to route them into your own logger (console,
pino, winston, …):

```js
import { initLogging } from "mq-bridge";

initLogging((record) => {
  // record: { level, target, message }
  console.log(`[${record.level}] ${record.target}: ${record.message}`);
}, "debug"); // level is optional, defaults to "warn"
```

`target` is the emitting Rust module (e.g. `mq_bridge::route`). `level` seeds
the Rust-side filter (default `"warn"`); the `MQ_BRIDGE_LOG` / `RUST_LOG`
environment variables take precedence over it. Filtering happens in Rust, so
suppressed events never cross the FFI boundary. The callback is held weakly
and won't keep the process alive. Call it once per process — a second call
throws.

## Analysis

HTTP comparison benchmark, driven by a native load generator (`wrk`) so the
client is never the bottleneck. It boots each server in its own process — the
mq-bridge Node route plus raw Node `http`, uWebSockets.js, Fastify, and Express
(all peers run only when installed) — and drives each with `wrk`:

```bash
cd bindings/node
npm run build                 # release addon (the bench is meaningless on a debug build)
npm i -D fastify express      # optional Node HTTP peers
npm i -D uNetworking/uWebSockets.js#v20.51.0   # the http-arena Node leader (native, from GitHub)
npm run bench:http -- --connections 1,8,32 --duration 8
```

Requires `wrk` on `PATH` (`brew install wrk`). The `mqb` target needs the addon
built with the `http` feature (`npm run build` or `npm run build:ci`). Each
server echoes a tiny JSON body with `value` incremented; mq-bridge routes on the
`kind` header while the framework peers post plain JSON.

`uws` (uWebSockets.js) is the framework that tops public Node benchmarks like
http-arena and TechEmpower — it bypasses Node's `http` stack with a C++ socket
layer, so it is the most demanding yardstick here.
