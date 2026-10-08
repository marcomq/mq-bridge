# mq-bridge for C

A C library over the mq-bridge engine: publish, consume and run routes against
any supported endpoint (Kafka, NATS, AMQP, MQTT, HTTP, files, databases, ...),
configured with the same YAML or JSON as the CLI and the other bindings. C++,
and any language that can call C, can use it too.

- Header: [`include/mq_bridge.h`](../../include/mq_bridge.h) (generated, checked in)
- Library: `libmq_bridge.so` / `.dylib` / `mq_bridge.dll`, and `libmq_bridge.a`
- Examples: [`examples/c-library`](../../examples/c-library)

## Build

There are no prebuilt archives yet; build it with Cargo from the repository root.

```sh
cargo build --release -p mq-bridge-c                 # every endpoint
cargo build --release -p mq-bridge-c \
    --no-default-features --features kafka,nats      # only what you need
```

The libraries land in `target/release/`. In-memory endpoints, the structural
endpoints and middleware need no feature; the endpoint features are the ones of
the [Node.js binding](../../node/mq-bridge-node/Cargo.toml).

```sh
cc -std=c11 -I include app.c -L target/release -lmq_bridge -o app
```

Add `-Wl,-rpath,<dir>` or set `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` so the
program finds the shared library at run time. Linking the static library also
needs the system libraries Rust uses (`cargo rustc -p mq-bridge-c -- --print native-static-libs`).

## Use

```c
#include <stdio.h>
#include <string.h>
#include "mq_bridge.h"

int main(void) {
    mqb_publisher_t *publisher = mqb_publisher_from_str(
        "nats: { url: \"nats://localhost:4222\", subject: orders }", NULL);
    if (publisher == NULL) {
        fprintf(stderr, "%s\n", mqb_last_error());
        return 1;
    }
    const char *body = "{\"id\": 1}";
    mqb_message_t *message = mqb_message_new((const uint8_t *)body, strlen(body));
    if (mqb_publisher_send(publisher, message) != MQB_OK) {
        fprintf(stderr, "%s\n", mqb_last_error());
    }
    mqb_message_free(message);
    mqb_publisher_free(publisher);
    return 0;
}
```

Consuming is a poll loop. A batch stays outstanding until it is acknowledged:

```c
mqb_consumer_t *consumer = mqb_consumer_from_file("consumer.yaml", NULL);
mqb_batch_t *batch = NULL;
while (mqb_consumer_poll(consumer, 100, 1000, &batch) == MQB_OK) {
    for (size_t i = 0; i < mqb_batch_count(batch); i++) {
        MqbSlice payload = mqb_message_payload(mqb_batch_at(batch, i));
        fwrite(payload.ptr, 1, payload.len, stdout);
    }
    mqb_batch_free(batch);                 /* NULL after a timeout: a no-op */
    mqb_consumer_commit(consumer);
    if (mqb_consumer_exhausted(consumer)) break;
}
mqb_consumer_free(consumer);
```

A route moves messages in the background; a handler sees each one on the way:

```c
static MqbStatus handle(const mqb_message_t *message, mqb_message_t **out, void *user_data) {
    MqbSlice payload = mqb_message_payload(message);
    *out = mqb_message_new(payload.ptr, payload.len);   /* publish this; leave NULL to ack */
    return MQB_OK;                                      /* MQB_ERR_RETRYABLE redelivers */
}

mqb_route_t *route = mqb_route_from_file("routes.yaml", "orders");
mqb_route_set_handler(route, handle, NULL);
mqb_route_start(route);
/* ... */
mqb_route_stop(route);
mqb_route_join(route);
mqb_route_free(route);
```

## Rules

- **Every call blocks** the calling thread. There are no callbacks to complete
  and no event loop to integrate; use your own threads for concurrency.
- **Errors:** a call returns an `MqbStatus` (`MQB_OK` is 0) or a null handle. The
  text is in `mqb_last_error()`, per thread, until the next failing call.
  `MQB_ERR_RETRYABLE` and `MQB_ERR_CONNECTION` mean the same call may succeed
  later; anything else is permanent.
- **Ownership:** whatever a `*_new`, `*_from_*` or out-parameter gives you, you
  free with the matching `*_free`. Sending does not consume a message. A slice
  (`MqbSlice`) borrowed from a message is not NUL-terminated and is valid until
  that message is changed or freed.
- **Threads:** handles may be shared between threads. Handlers and the log
  callback are called from library threads, several at once. A poll in progress
  holds its consumer: closing it or reading its status from another thread waits
  for the poll, so give a poll a timeout if you do that.
- **No ABI stability.** The header is regenerated with every release and ships
  with its library. `mqb_api_version() != MQB_API_VERSION` means the two are from
  different releases.
- **64-bit targets only.**

## Custom endpoints and middleware

`mqb_register_plugin(&table)` registers an `MqbPluginVTable` defined in your own
program, the same table a [plugin library](../../docs/PLUGINS.md#writing-one-in-c-or-c)
exports. Its name then works as `custom: { name: ..., config: {} }` in a route,
as an endpoint, a middleware or both, depending on its capability flags.
[`mq_bridge_plugin_helpers.h`](../../include/mq_bridge_plugin_helpers.h) fills
the entries you do not implement; [`smoke.c`](../../examples/c-library/smoke.c)
has a complete one. `mqb_load_plugin(path)` loads a plugin library instead.

## Developing

```sh
cargo test -p mq-bridge-c --no-default-features                              # header + C smoke test
MQB_C_ASAN=1 cargo test -p mq-bridge-c --no-default-features --test c_api    # same, with AddressSanitizer
MQB_BLESS=1 cargo test -p mq-bridge-c --no-default-features --test c_header  # regenerate the header
```

Any header change fails the `c_header` test until `REVIEWED_HEADER_HASH` in it is
updated by hand. That is the moment to decide whether `MQB_API_VERSION` in
`src/lib.rs` needs a bump: it does when a program built against the old header
could break.
