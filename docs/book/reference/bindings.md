# Language bindings API

The core engine is a Rust library, but the **same engine** ships as native bindings for
**Python** and **Node.js**. The Tokio runtime, broker I/O, routing, and batching all stay in
Rust; the binding is a thin layer for handlers and configuration.

| Language | Package | Install |
| :--- | :--- | :--- |
| Rust | [`mq-bridge`](https://crates.io/crates/mq-bridge) | `cargo add mq-bridge` |
| Python | [`mq-bridge`](https://pypi.org/project/mq-bridge/) | `pip install mq-bridge` |
| Node.js | [`mq-bridge`](https://www.npmjs.com/package/mq-bridge) | `npm install mq-bridge` |

The core of the library are the `MessageConsumer` and `MessagePublisher` traits, found in
`mq_bridge::traits`.

## Config loaders

Constructor names are kept aligned across languages, so a config loader reads the same in
either binding (Python uses `snake_case`, Node uses `camelCase`):

| Purpose | Python | Node.js |
|---|---|---|
| Load a route from a YAML/JSON **file** | `Route.from_file` | `Route.fromFile` |
| Load from an in-memory YAML/JSON **string** | `Route.from_str` | `Route.fromStr` |
| Load from a parsed **dict / JS object** | `Route.from_config` | `Route.fromConfig` |
| Build a publisher endpoint | the matching `Publisher.*` | the matching `Publisher.*` |

The `name` argument is optional in both: pass it to select one entry from a
`routes:`/`publishers:` document, or omit it to treat the config as a single bare
route/endpoint body.

This is the natural companion to the [configuration-first workflow](../getting-started/run-forms.md):
design and test a route in the UI, export the JSON/YAML, then load that exact config from your
code. See the [Embed the library](../tutorials/embedding.md) tutorial for full examples in each
language.

## Extending from a binding

Custom endpoints and middleware are not Rust-only. Both bindings expose
`register_endpoint` / `register_middleware` (`registerEndpoint` / `registerMiddleware` in
Node), with the same batch, ack and request-reply semantics as a Rust `CustomEndpointFactory`,
and both can load a compiled plugin — `mq_bridge.load_endpoint_plugin(path)` in Python,
`loadEndpointPlugin(path)` in Node — or an installed one by name with
`load_installed_plugin("connect")` / `loadInstalledPlugin("connect")`, so one Rust
implementation serves every language. Nothing is loaded unasked: automatic plugin discovery
is off until `enable_plugin_discovery()` or `MQB_PLUGIN_DISCOVERY=1`.
Registration is process-global, keyed by name, and must happen before a route that names it
starts; a duplicate name is an error rather than a silent replacement.

Full examples in [Custom endpoints](../extending/custom-endpoints.md) and
[Native plugins](../extending/plugins.md).

## Rust API surface

The Rust crate exposes the full engine. The main types:

- `Route::new(input, output)` — a pipeline from one endpoint to one endpoint, with
  `.with_batch_size(n)`, `.with_handler(h)`, `.add_handler(kind, f)`, `.deploy(name)` /
  `.run()`.
- `Endpoint` — protocol adapters (`Endpoint::new_memory`, `Endpoint::null`, …) plus the
  serde-configured variants.
- `Publisher::new(endpoint)` — publish into a route's input or any endpoint.
- Handlers: `CommandHandler` (1-to-1/1-to-0), `EventHandler` (terminal 1-to-N), and
  `TypeHandler` (dispatches on the `kind` metadata field, deserializing payloads).
- `CanonicalMessage` — the unified message type all handlers work with; `msg!(&value, "kind")`
  builds one with a `kind`.

See [Core concepts](../getting-started/concepts.md) and
[Learn the architecture](../engine/architecture.md) for the handler model, and the
[Rust docs on docs.rs](https://docs.rs/mq-bridge) for the full API.

## Notes

- The Python binding holds up under load on the third-party
  [http-arena.com](https://www.http-arena.com/#sort=rps:-1&q=python) requests-per-second HTTP
  benchmark (a live leaderboard — rankings shift over time).
- A binding is a thin layer: routing, batching, and broker I/O stay in Rust regardless of
  which language calls in, so behaviour and reliability match the Rust engine.
