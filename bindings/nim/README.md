# mq-bridge for Nim

A Nim binding of mq-bridge: publish, consume and run routes against any supported
endpoint (Kafka, NATS, AMQP, MQTT, HTTP, files, databases, ...), configured with the
same YAML or JSON as the CLI and the other bindings. Custom endpoints and middleware
can be written in Nim too.

It runs on the [C library](../c/README.md) (`libmq_bridge`), which it loads when the
program starts. Needs Nim 2.0 or later with ORC (the default) and threads on (the
default), on a 64-bit platform.

## Install

The nimble package is `mq_bridge` (nimble does not allow `-` in names). Until it is in
the nimble registry, install it from a checkout:

```sh
git clone https://github.com/marcomq/mq-bridge
cd mq-bridge/bindings/nim && nimble install
```

`nimble install` also downloads the matching C library (every endpoint) from the
[GitHub release](https://github.com/marcomq/mq-bridge/releases) into the package, so
`nim c -r app.nim` works with no further step. Prebuilt libraries exist for Linux
x86_64 and arm64, macOS arm64 and Windows x86_64; it needs `curl` and `tar`. In a
checkout without installing, `nimble getlib` does the same download. To skip it, set
`MQB_SKIP_LIB_DOWNLOAD=1`.

To build the library yourself instead, e.g. with fewer endpoints, run from the
repository root:

```sh
cargo build --release -p mq-bridge-c                 # every endpoint
cargo build --release -p mq-bridge-c \
    --no-default-features --features kafka,nats      # only what you need
```

Use the library and the binding of the same release; the binding refuses a library
with another API version at start-up.

## Finding the library

The program loads the library at start, from the first of:

1. the package's `src/lib/`, filled by `nimble install` or `nimble getlib`, as it was
   when the program was compiled;
2. next to the executable (macOS and Windows);
3. the system loader's search path: a system library directory, or
   `LD_LIBRARY_PATH` (Linux) / `DYLD_LIBRARY_PATH` (macOS).

To ship a program, put the library next to the executable; on Linux use
`LD_LIBRARY_PATH` or a system directory. To load one fixed file instead, compile with:

```sh
nim c -d:mqbLib=/opt/mq-bridge/lib/libmq_bridge.so app.nim
```

## Use

```nim
import std/options
import mq_bridge

let publisher = Publisher.fromStr("nats: { url: \"nats://localhost:4222\", subject: orders }")
publisher.send("""{"id": 1}""")
publisher.sendBatch([newMessage("a"), newMessage("b", metadata = [("kind", "order")])])

let consumer = Consumer.fromStr("memory: { topic: orders }")
let batch = consumer.poll(max = 100, timeoutMs = 1000)
for message in batch:
  echo message.payload, " ", message.metadata("kind")
consumer.ack(batch)
```

A route moves messages from its input to its output, optionally through a handler:

```nim
let route = Route.fromStr("""
input: { kafka: { url: "localhost:9092", topic: orders, group_id: bridge } }
output: { nats: { url: "nats://localhost:4222", subject: orders } }
""")
route.withHandler(proc (m: MessageView): Option[Message] =
  if m.payload.len == 0:
    raise newException(RetryableError, "empty, try again later")
  none(Message))          # none: acknowledge; some(msg): publish msg instead
route.run()               # start, then wait until it stops
```

`withBatchHandler` gets a whole batch and returns one result per message;
`addHandler(kind, handler)` handles only messages whose `kind` metadata matches. A
handler that raises `RetryableError` has the message redelivered (on a memory input
only with `enable_nack: true`); any other exception drops it as failed.

Errors raise `MqbError` or a subtype (`RetryableError`, `ConnectionError`,
`ConfigError`, `EndOfStreamError`, `UnsupportedError`) carrying the library's text.

## Threads

Handlers run on one Nim thread, one call at a time:

- After `route.start()` or `route.run()` that is the calling thread. A handler runs
  while that thread is inside an mq_bridge call (`join`, `send`, `poll`, `request`,
  ...), so a handler can fire while its own thread publishes or waits. A thread busy
  elsewhere calls `dispatch()` to run the calls waiting for it.
- `route.start(worker = true)` runs the route's handlers on a thread of its own
  instead; the starting thread is free. Data the handler shares with other threads
  needs a lock.

Handlers never run on the library's own threads, so they can use the GC'd heap,
closures and exceptions freely.

## Custom endpoints and middleware

Subclass `PluginConsumer`, `PluginPublisher` or `PluginMiddleware`, override its
methods and register constructors; a route then names it as
`custom: { name: <name>, config: {...} }`. Each plugin runs on a Nim thread of its own.

```nim
type DropPing = ref object of PluginMiddleware

method apply(d: DropPing, messages: seq[PluginMessage]): seq[Option[PluginMessage]] =
  for m in messages:
    result.add(if m.payload == "ping": none(PluginMessage) else: some(m))

registerPlugin Plugin(name: "drop_ping",
  newMiddleware: proc (route, config: string, side: MiddlewareSide): PluginMiddleware =
    DropPing())
```

```yaml
output:
  nats: { url: "nats://localhost:4222", subject: orders }
  middlewares:
    - custom: { name: drop_ping, config: {} }
```

A middleware that needs the endpoint itself (to end the stream, hold messages back, or
keep state across commits) sets `wraps: true` and overrides `wrapConsumer` or
`wrapPublisher`. The host lends it the endpoint as an `InnerConsumer` / `InnerPublisher`:

```nim
type
  Take = ref object of PluginMiddleware
    limit: int
  TakeConsumer = ref object of PluginConsumer
    inner: InnerConsumer
    left: int

method wrapConsumer(t: Take, inner: InnerConsumer): PluginConsumer =
  TakeConsumer(inner: inner, left: t.limit)

method receive(c: TakeConsumer, max: int): PluginBatch =
  if c.left == 0: raise newException(EndOfStreamError, "limit reached")
  dec c.left
  c.inner.receive(max)

method commit(c: TakeConsumer, batch: PluginBatch, ds: seq[Disposition]) =
  c.inner.commit(batch, ds)

registerPlugin Plugin(name: "take", wraps: true,
  newMiddleware: proc (route, config: string, side: MiddlewareSide): PluginMiddleware =
    Take(limit: parseJson(config)["limit"].getInt))
```

At most 16 Nim plugins per process. `loadPlugin(path)` loads a native plugin library
built for any language; `loadInstalledPlugin("connect")` loads the installed
`libmq_bridge_connect` and nothing else. `enablePluginDiscovery()` lets routes
load installed plugins by endpoint name; it is off by default.

## Examples

[examples/](examples/README.md) has three small programs: parallel lookups against
SQL and HTTP, LLM classification and streaming, and CSV to JSON and Parquet.

## Tests

```sh
cargo build -p mq-bridge-c --no-default-features    # from the repository root
cd bindings/nim && nimble test
```

The tests find the library in `target/debug` or `target/release`, or take its path
from `MQB_LIB`. `nim c -r -d:addressSanitizer tests/<test>.nim` runs one under
AddressSanitizer. `tests/tdrift.nim` fails when the Nim declarations and the C headers
disagree.
