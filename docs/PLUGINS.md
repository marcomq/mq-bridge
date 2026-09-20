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

A plugin is loaded either by path, as below, or by the endpoint name a route
asks for — see [Installing a plugin by name](#installing-a-plugin-by-name).

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

## Installing a plugin by name

A plugin does not have to be listed anywhere. When a route asks for a `custom`
endpoint no factory is registered under, mq-bridge looks for one file named
after it:

| Platform | File |
| :--- | :--- |
| Linux | `libmq_bridge_<name>.so` |
| macOS | `libmq_bridge_<name>.dylib` |
| Windows | `mq_bridge_<name>.dll` |

`-` in an endpoint name becomes `_` in the file name, so `ibm-mq` is
`libmq_bridge_ibm_mq.so`. Installing that file is the whole installation step —
no `plugins:` entry, no `--plugin`, no `load_endpoint_plugin` call:

```console
mqb copy 'redpanda://…' 'postgres://…'
```

Directories are searched in this order, nearest first:

1. every directory in `MQB_PLUGIN_DIR` (`:`-separated, `;` on Windows) — a
   custom location, and the one to set for an unpackaged build
2. the directory holding the running binary
3. `lib/mq-bridge` and `lib` under that binary's prefix — `/opt/homebrew/lib`
   for a brewed `mqb`, `$CONDA_PREFIX/lib` for a conda-installed one
4. the same two under `$CONDA_PREFIX` and `$HOMEBREW_PREFIX`, which reach a
   plugin the *running* binary is not installed beside — `python` in a venv,
   `cargo run`, a plugin brewed for a binary from somewhere else
5. `/opt/homebrew` and `/usr/local` (`/home/linuxbrew/.linuxbrew` and
   `/usr/local` on Linux), because `HOMEBREW_PREFIX` comes from
   `brew shellenv` in a shell profile and a service or container has none
6. `$XDG_DATA_HOME/mq-bridge/plugins`, else `~/.local/share/mq-bridge/plugins`
   (`%APPDATA%\mq-bridge\plugins` on Windows)

On Windows each prefix also contributes `Library\bin`, where a conda environment
keeps its DLLs. Both `lib/mq-bridge` and plain `lib` are searched under every
prefix, so a package manager needs no special layout: `lib.install` in a brew
formula or a conda package's default `lib` is enough, and `lib/mq-bridge` keeps
a hand-managed install tidy.

**Only the requested name is ever looked up.** Directories are never listed, so
a library no route names is never opened, and installing one has no effect on a
process that does not ask for it. That is also why the lookup costs nothing when
every endpoint is built in: it runs only after the registry has already missed.

Set `MQB_PLUGIN_DISCOVERY=0` (or `false`, `off`, `no`) to switch the search off
and resolve endpoints only from factories the host registered or a config listed
by path.

A file whose name does not match the endpoint the library actually provides is an
error naming both — and the library stays loaded, because nothing is ever
unloaded. The same applies to a library that exists but fails to load: that is
reported as a load failure, not as an unknown endpoint.

### Installing one with a package manager

Homebrew and conda install into a prefix the search above already covers, so
either one is the whole installation — no `plugins:` entry, no `--plugin`, no
`load_endpoint_plugin` call:

```console
brew install marcomq/tap/mq-bridge-pulsar     # macOS arm64, Linux x86_64/arm64
conda install -c marcomq mq-bridge-pulsar     # the same, plus Windows x86_64
```

Both cover `pulsar`, `meilisearch` and `redpanda`. Neither depends on
`mq-bridge-app`: one installed library serves whatever host asks for the
endpoint — the brewed CLI, the desktop app, a Python or Node process in a
virtualenv — which is why the search covers `HOMEBREW_PREFIX` and
`CONDA_PREFIX` rather than only the running binary's own prefix.

### Replacing an endpoint `mqb` already has

Some endpoints are compiled into `mqb` itself — Pulsar and Meilisearch are
separate crates linked into the `full` build — and a registered factory is found
before the search above ever runs. The built-in copy therefore wins by default,
so one binary behaves the same wherever it runs.

`MQB_PLUGIN_OVERRIDE` reverses that, which is how such an endpoint is updated
without waiting for a new `mqb` release:

```console
# prefer the installed Meilisearch plugin over the linked-in copy
MQB_PLUGIN_OVERRIDE=meilisearch mqb copy 'postgres://…/orders' 'meilisearch://localhost:7700?index=orders'

# prefer an installed plugin for every built-in extension
MQB_PLUGIN_OVERRIDE=1 mqb run -c config.yaml
```

The value is `1`, `true`, `yes`, `on` or `all` for every extension, or a
comma-separated list of endpoint names. It is an environment variable rather
than a flag or a config key because endpoints are registered once per process,
before any config is read — a `mqb copy` between two URIs never loads a config
file at all, and two routes in one process cannot use different versions of the
same endpoint.

When a plugin is installed but *not* preferred, startup says so rather than
ignoring it silently, naming the file and the variable that would use it. And
because a plugin's version cannot be read without loading it, preference is by
name, not by version: an **older** installed plugin will replace a newer
built-in. `MQB_PLUGIN_DISCOVERY=0` switches off the search, the notice and the
override together.

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

### Describing its configuration

An endpoint's `config` is an opaque JSON object: the plugin defines it, and the
host has no idea what is in there. Since **ABI 1.1** a plugin can describe it as
a JSON Schema, which buys two things — a host can render a form for the endpoint
instead of a blank text box, and it can turn a URI into configuration with the
right *types*.

The second one matters more than it sounds. A URI carries only text, so without
a schema `?batch_size=100` reaches the plugin as the string `"100"` and
`?tls=true` as `"true"`. Any plugin with a non-string config field is unusable
from a URI until it describes one.

```rust
impl CustomEndpointFactory for RedpandaFactory {
    fn config_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url":        { "type": "string",  "x-mqb-uri": "origin" },
                "topic":      { "type": "string",  "x-mqb-uri": "path" },
                "group":      { "type": "string",  "default": "mq-bridge" },
                "batch_size": { "type": "integer", "default": 100 }
            },
            "required": ["url", "topic"]
        }))
    }
    // ...
}
```

Deriving it beats writing it by hand if your config is a Rust struct — add
[`schemars`](https://docs.rs/schemars) to your own crate and return
`serde_json::to_value(schemars::schema_for!(RedpandaConfig)).ok()`. Nothing but
JSON crosses the ABI, so your schemars version and the host's need not agree,
and a plugin written in another language just emits the document. `title`,
`description` and `default` are what a host shows, so they are worth filling in;
`models.rs` doc comments serve the same purpose for built-in endpoints.

`MiddlewareFactory::config_schema` does the same for the middleware half. A
middleware is never addressed by a URI, so only the form applies.

#### Where each field sits in a URI

`x-mqb-uri` names a field's place in a URI. JSON Schema reserves the `x-` prefix
for annotations, so a validator ignores it and the document stays a plain schema.

| `x-mqb-uri` | Gets | From `rp://user@host:9092/orders?group=g` |
| --- | --- | --- |
| `subscheme` | the scheme's part after `+` | nothing; see below |
| `origin` | scheme, userinfo, host and port | `rp://user@host:9092` |
| `url` | everything before `?` | `rp://user@host:9092/orders` |
| `path` | the path, without its leading `/` | `orders` |
| `query` | the query parameter of the same name (the default) | `g` |

Each position but `query` may be claimed by at most one field.
Everything unannotated is a query parameter, read as its declared `type` —
`integer`, `number`, `boolean`, `array` (comma-separated, with `items` honoured)
or string. A value that is not what the field declares is refused by name,
rather than handed to the plugin's deserializer to complain about.

A query parameter always wins over the position it would have had, so
`?url=...` remains the escape hatch for an address a URI cannot spell.

Annotate nothing and the mapping is the one that predates schemas: `url` gets
everything before `?` and every parameter stays a string. That is also what a
plugin describing no schema at all gets, so nothing changes under an older
plugin.

#### A plugin that is a gateway

A plugin reaching a family of protocols rather than one — a compatibility layer,
a driver host — names the protocol in the scheme, after a `+`:

```
mq-bridge --input 'redpanda+mqtt://localhost:1883/orders' --output 'kafka://...'
```

This is the spelling `git+ssh://`, `svn+ssh://` and SQLAlchemy's
`postgresql+psycopg2://` made familiar. The part before the `+` names the
plugin, so that is what the host looks the factory up by; the part after it is
the plugin's own vocabulary, and a field annotated `subscheme` receives it.

Everything after the scheme then describes the inner protocol rather than the
plugin, so `origin` and `url` are handed over carrying the inner scheme:

| From `redpanda+mqtt://host:1883/orders` | |
| --- | --- |
| `subscheme` | `mqtt` |
| `origin` | `mqtt://host:1883` |
| `url` | `mqtt://host:1883/orders` |
| `path` | `orders` |

A scheme may hold only letters, digits, `+`, `-` and `.`
([RFC 3986](https://www.rfc-editor.org/rfc/rfc3986#section-3.1)) — no `_`. A
plugin whose protocol names contain one accepts `-` in its place and maps it
back itself, which stays unambiguous only as long as no name of its own uses
`-`.

A schema the host cannot use — not an object, two fields claiming the same
position, a misspelled `x-mqb-uri` — fails at **load** time rather than at
whichever call site looked first.

#### Enforcing it

A schema is read and mapped, **not enforced** — your factory still checks what it
is given. Set `MQB_PLUGIN_VALIDATE_CONFIG=1` and the host checks a route's config
against it first, which turns a deserializer's complaint into a message naming
the field:

```
endpoint `redpanda` configuration: unknown field `topci`; this endpoint takes
batch_size, group, topic, url
```

It is off by default on purpose. The schema is yours, not the route's, and the
host checks only the subset the `transform` middleware already validates against
— `type`, `required`, `enum`, `items`, nested `properties`, plus unknown
top-level fields when you set `additionalProperties: false`. A schema using more
than that (`oneOf`, `$ref` to a remote document, `patternProperties`) is logged
as uncheckable and passed through rather than rejected, so describing yourself
richly for the sake of a form never costs you a working endpoint.

### Ordered publishing

A sink whose correctness depends on batches arriving in source order — anything
keyed, where a stale write can overwrite a newer one — overrides
`MessagePublisher::requires_ordered_publish`. Since **ABI 1.1** the host reads
that through the plugin boundary too, so a plugin-loaded sink gets its sends
sequenced exactly like a directly linked one, whatever the route's
`concurrency`.

A plugin built against ABI 1.0 has no such entry. The host cannot ask, so it
assumes unordered — which is what those plugins already do today. Rebuild
against 1.1 to have the flag honoured.

### Partial publishes

A publisher that returns `SentBatch::Partial` says some of the batch landed and
some did not. Since **ABI 1.1** that survives the boundary: the host hands the
plugin a byte per message and the plugin marks the ones that failed, so the route
nacks or dead-letters only those and acknowledges the rest.

Nothing new to write — the SDK derives the marks from the `Partial` your
`send_batch` already returns, matching each failure back to its position by
message id. Two details are worth knowing:

- **The class travels per message, the text does not.** Each mark says retryable
  or permanent; one error string describes the batch. Per-message text would cost
  an allocation per failure, and can be appended in a later minor if it is ever
  needed.
- **A batch where nothing landed stays a batch error**, not a `Partial` listing
  every message, so a connection-level failure can still mean "reconnect this
  endpoint" — something no per-message mark can express.

A plugin built against ABI 1.0 has no such entry. The host falls back to the
all-or-nothing send, where a partial failure is reported as the first failure's
class and the whole batch — including the part that succeeded — is retried or
dead-lettered.

### Limits of ABI v1

- No per-message publish *responses*, so no request/reply through a plugin. A
  `Partial`'s `responses` are dropped; only its failures cross.
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

Neither is needed for a plugin installed under its conventional file name: the
endpoint name alone resolves it, in a config or in a URI scheme. See
[Installing a plugin by name](#installing-a-plugin-by-name).

---

## Versioning and compatibility

The ABI has its own major/minor version
(`mq_bridge::support::plugin_abi::MQB_PLUGIN_ABI_MAJOR` / `_MINOR`), independent
of the mq-bridge release it ships in:

- A different **major** is rejected at load with an actionable error.
- Within a major, fields are only ever appended to the function table, and both
  sides use its recorded size to decide what exists — so an older plugin keeps
  working with a newer host.

Minor versions so far:

| Minor | Added |
| --- | --- |
| 1.0 | The initial table. |
| 1.1 | `publisher_requires_ordered_publish`, so a plugin sink can ask the route to keep its sends in source order; `publisher_send_batch_outcomes`, so a partly failed batch reports which messages failed; and `factory_config_schema`, so a plugin describes its configuration as a JSON Schema for a host to render and to map a URI onto. |

Publish the supported ABI range in your package metadata, and test each packaged
plugin against the oldest and newest mq-bridge you claim to support.

---

## See also

- [EXTENDING.md](EXTENDING.md) — custom endpoints and middleware, including
  Python and JavaScript ones
- [REFERENCE.md](REFERENCE.md) — every built-in endpoint and middleware
- [ARCHITECTURE.md](ARCHITECTURE.md) — how routes, batching and commits fit together
