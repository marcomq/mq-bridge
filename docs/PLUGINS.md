# Plugins

An endpoint or middleware written in Rust can be compiled to a shared library
and loaded into **any** mq-bridge process at runtime — Rust, Python or
Node.js — without being compiled into mq-bridge itself.

That solves a specific problem: an endpoint like Pulsar or a proprietary broker
drags in a dependency tree (and a `protoc`, or a vendor C client) that nobody
who does not use it should have to build. As a plugin it lives in its own
repository, on its own release cycle, and every language runs the *same*
implementation with the same delivery semantics.

> Writing the endpoint in Python or JavaScript instead — no compilation, no
> packaging — is often the better trade. See [EXTENDING.md](EXTENDING.md).

---

## Using a plugin

Loading is always explicit; installing a package never registers anything.

```rust
// Rust
mq_bridge::plugin::load_endpoint_plugin("./libmq_bridge_pulsar.so")?;
```

```python
# Python
import mq_bridge
mq_bridge.load_endpoint_plugin("./libmq_bridge_pulsar.so")
```

```javascript
// Node.js
import { loadEndpointPlugin } from "mq-bridge";
loadEndpointPlugin("./libmq_bridge_pulsar.so");
```

Published endpoint packages wrap that call so you never touch a path:

```python
import mq_bridge_pulsar
mq_bridge_pulsar.register()
```

After loading, the endpoint is usable by name, exactly like a factory registered
in-process:

```yaml
input:
  custom:
    name: pulsar
    config:
      url: "pulsar://localhost:6650"
      topic: "persistent://public/default/orders"
```

Load once, before starting routes that use it. Loading the same file twice is a
no-op, and the **plugin loader** rejects a second library claiming a name that is
already registered, rather than silently replacing it. That check belongs to the
loader alone: registering the same name twice in-process (via
`register_endpoint_factory` / `register_middleware_factory`, see
[EXTENDING.md](EXTENDING.md)) returns an error and preserves the first factory. Rust users who link
the endpoint crate directly can skip loading entirely and call its `register()`.

The `plugin` feature (in `full` and `portable`) provides the loader.

**A plugin is native code in your process.** It can crash it or do anything the
process may do. Treat plugin packages like other native dependencies, not like
sandboxed scripts. Nothing is ever unloaded: a library stays mapped for the life
of the process, because unloading while endpoint handles or in-flight batches
exist cannot be made safe.

---

## Writing a plugin

Implement the ordinary mq-bridge contracts — `CustomEndpointFactory`,
`MessageConsumer`, `MessagePublisher` (see [EXTENDING.md](EXTENDING.md)) — then
export the factory:

```toml
[dependencies]
mq-bridge = { version = "0.4", default-features = false, features = ["plugin-sdk"] }

[lib]
crate-type = ["rlib", "cdylib"]
```

```rust
#[derive(Debug, Default)]
pub struct PulsarFactory;

#[async_trait]
impl CustomEndpointFactory for PulsarFactory { /* ... */ }

mq_bridge::export_endpoint_plugin! {
    name: "pulsar",
    factory: PulsarFactory,
}
```

That is the whole FFI surface. The `rlib` keeps the endpoint usable as plain
Rust — link it, test it, `register()` it — while the `cdylib` is what other
processes load. Your factory type must implement `Default` (the ABI constructs
it with no arguments); configure endpoints through the route's `config`, not
through factory state.

The SDK handles what the boundary requires: panic containment, buffer and handle
lifetimes, error translation, and the plugin's own async runtime. Acknowledgement
timing is passed through untouched — the host's batch commit arrives at your
`ReceivedBatch` commit function, so nothing is acked before the route says so,
and a batch dropped mid-shutdown acks nothing at all.

Declare an output-only (or input-only) endpoint when it is one:

```rust
mq_bridge::export_endpoint_plugin! {
    name: "metrics-sink",
    factory: SinkFactory,
    capabilities: mq_bridge::plugin::sdk::CAPABILITIES_OUTPUT_ONLY,
}
```

### Middleware

A plugin can also provide a middleware. It never touches the endpoint it wraps —
the host keeps that wrapper — so all that crosses the ABI is the batch:

```rust
#[derive(Debug, Default)]
struct RedactFactory;

#[async_trait]
impl mq_bridge::plugin::sdk::MiddlewareFactory for RedactFactory {
    async fn create(
        &self,
        _route: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn mq_bridge::plugin::sdk::BatchFilter>> {
        Ok(Box::new(Redact::new(config)?))
    }
}

#[async_trait]
impl mq_bridge::plugin::sdk::BatchFilter for Redact {
    async fn on_receive(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Option<CanonicalMessage>>> {
        // Exactly one entry per input message, in order: `None` drops it.
        Ok(messages.into_iter().map(|m| Some(self.redact(m))).collect())
    }
}

mq_bridge::export_middleware_plugin! {
    name: "redact",
    middleware: RedactFactory,
}
```

Routes name it like any custom middleware; loading the library registers it:

```yaml
input:
  kafka: { topic: orders }
  middlewares:
    - custom:
        name: redact
        config: { fields: ["ssn"] }
```

Return `None` to drop a message. The host acknowledges dropped messages on the
source for you, so they are not redelivered — and a batch that is filtered away
entirely never reaches the route, which keeps `exit_on_empty` from mistaking it
for a drained source.

A plugin that provides both uses one name for both, which is usually what a
transport-specific middleware wants:

```rust
mq_bridge::export_endpoint_plugin! {
    name: "pulsar",
    factory: PulsarFactory,
    middleware: PulsarMiddleware,
}
```

### Limits of ABI v1

- A batch is published all-or-nothing: no per-message publish responses, so no
  request/reply through a plugin.
- `MessageDisposition::Reply` acknowledges the source message.
- One plugin per shared library (the export macro defines the discovery symbol),
  so two plugin crates cannot be statically linked into one binary. Gate the
  macro behind a feature if that matters for your crate.

### Testing it

Run the same semantic suite twice — linked directly, and loaded as a plugin. If
both agree, the ABI round trip changed nothing:

```rust
use mq_bridge::plugin::conformance::{self, ConformanceOptions};
use mq_bridge::plugin::{load_endpoint_plugin, test_support::build_plugin_cdylib};

let config = serde_json::json!({ "url": "pulsar://localhost:6650" });
let direct = conformance::run(&PulsarFactory, ConformanceOptions::new("direct", config.clone())).await?;

let library = build_plugin_cdylib(".", "mq-bridge-pulsar")?;
let info = load_endpoint_plugin(&library)?;
let factory = mq_bridge::extensions::get_endpoint_factory(&info.name).unwrap();
let loaded = conformance::run(factory.as_ref(), ConformanceOptions::new("plugin", config)).await?;

assert_eq!(direct, loaded);
```

The suite checks round-tripping, metadata preservation, nack redelivery, and
that an uncommitted batch is redelivered. Turn the redelivery checks off
(`expect_redelivery = false`) for endpoints that legitimately have none, or whose
broker delays redelivery beyond a test's patience, and the metadata check off
(`expect_metadata = false`) for transports that carry payloads only.

`build_plugin_cdylib` builds the package and reads the artifact path back out of
cargo, so tests do not hard-code target-directory layout or file extensions.

---

## Shipping it to Python and Node.js

Both bindings understand the same package manifest:

```json
{ "name": "pulsar", "library": "mq_bridge_pulsar" }
```

Store it as `mq-bridge-plugin.json`. A platform wheel may put its native library
beside that manifest. A cross-platform npm package puts each library under
`prebuilds/<platform>-<arch>/` (with `-gnu` or `-msvc` where applicable).

Python packages call `plugin_library_path()` and `load_plugin_package()`;
Node.js packages can export `definePluginPackage(__dirname)` directly. The
bindings own platform detection, filenames, errors, and loading, so endpoint
packages contain no custom loader logic.

`mq-bridge-pulsar` is the worked example. It publishes platform wheels under one
Python distribution name and one npm package containing all supported prebuilds.
The builders live in mq-bridge itself, so plugins do not copy packaging scripts:

```console
pip install "mq-bridge-py[plugin-packaging]"
python -m mq_bridge.plugin_packaging --package python/my_plugin --out dist
mq-bridge-package-plugin --package node --pack --out npm
```

---

## Loading without writing code

[mq-bridge-app](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) loads plugins for you,
so a YAML-only deployment can use an endpoint the binary never compiled:

```yaml
plugins:
  - "${MQB_PLUGIN_DIR}/libmq_bridge_pulsar.so"

routes:
  orders:
    input:
      custom:
        name: pulsar
        config: { url: "pulsar://localhost:6650" }
```

or per run:

```console
mq-bridge-app --plugin ./libmq_bridge_pulsar.so --config mq-bridge.yaml
```

Paths go through the app's usual `${VAR}` expansion, which is what keeps a
config portable across machines that install libraries in different places.
Plugins load before any route is built; a path that fails to load stops startup
rather than leaving a route to fail later with "unknown endpoint".

---

## Versioning and compatibility

The ABI has its own major/minor version
(`mq_bridge::support::plugin_abi::MQB_PLUGIN_ABI_MAJOR` / `_MINOR`), independent
of the mq-bridge release it ships in:

- A different **major** is rejected at load with an actionable error.
- Within a major, fields are only ever appended to the function table, and both
  sides use its recorded size to decide what exists — so an older plugin keeps
  working with a newer host.

Publish the supported ABI range in your package metadata, and test each packaged
plugin against the oldest and newest mq-bridge you claim to support.

---

## See also

- [EXTENDING.md](EXTENDING.md) — custom endpoints and middleware, including
  Python and JavaScript ones
- [REFERENCE.md](REFERENCE.md) — every built-in endpoint and middleware
- [ARCHITECTURE.md](ARCHITECTURE.md) — how routes, batching and commits fit together
