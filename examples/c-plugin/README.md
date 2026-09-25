# C plugins

Two mq-bridge plugins written in C against
[`include/mq_bridge_plugin.h`](../../include/mq_bridge_plugin.h):

- **`minimal.c`** (`drop_heartbeats`, ~35 lines) is a middleware that drops
  messages whose payload is `ping` and passes everything else through unchanged.
  Start here.
- **`plugin.c`** (`legacy_payments`) wraps two existing C libraries that know
  nothing about mq-bridge:
  - as a **middleware**, `legacy_parser.c` turns each 25-byte payment record
    into JSON. Invalid records are dropped and logged through the host.
  - as an **output**, `legacy_ledger.c` appends each message to a file. The
    library is not thread-safe, so the plugin locks around it.
  - it registers a **crash handler** that adds a line to the host's crash dump
    (see [docs/PLUGINS.md](../../docs/PLUGINS.md#when-it-crashes)).

  ```
  DE12345678000000012345EUR  ->  {"account":"DE12345678","amount_minor":12345,"currency":"EUR"}
  ```

Both use [`mq_bridge_plugin_helpers.h`](../../include/mq_bridge_plugin_helpers.h)
so their tables list only the entries they implement.

## Build

```sh
cmake -B build && cmake --build build
# or directly (macOS: -dynamiclib instead of -shared)
cc -std=c11 -shared -fPIC -I ../../include minimal.c -o libdrop_heartbeats.so
```

## Use

```yaml
payments_to_ledger:
  input:
    memory: { topic: raw }
    middlewares:
      - custom: { name: drop_heartbeats }
      - custom: { name: legacy_payments }
  output:
    custom:
      name: legacy_payments
      config: { path: "/var/lib/payments/ledger.jsonl" }
```

Load the libraries before the route starts, e.g. from Python:

```python
mq_bridge.load_endpoint_plugin("./libdrop_heartbeats.so")
mq_bridge.load_endpoint_plugin("./liblegacy_payments.so")
```

See [docs/PLUGINS.md](../../docs/PLUGINS.md#writing-one-in-c-or-c) for the rules
a C or C++ plugin must follow.
