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
mqb copy 'connect://…' 'postgres://…'
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

**Only the named file is opened.** A route never loads a library it did not name,
and the search runs only after the registry has missed, so it costs nothing when
every endpoint is built in. `plugin::discover_all_endpoint_plugins()` is the one
call that lists the directories and loads every `libmq_bridge_*` up front;
`mq-bridge-app` makes it at startup so its UI lists those endpoints. A file that
does not export `mq_bridge_plugin_v1` — a plugin's own helper library, such as
`libmq_bridge_connect_go` — is recognised from its export table and never opened.

Set `MQB_PLUGIN_DISCOVERY=0` (or `false`, `off`, `no`) to switch the search off
and resolve endpoints only from factories the host registered or a config listed
by path.

### Which files discovery trusts

Loading a library runs its code, so a library found by discovery is loaded only
if nobody but you or root could have put it there. On Linux and macOS the file
and every directory above it, symlinks resolved, must:

- belong to the user running mq-bridge, or to root, and
- not be world-writable. A sticky directory such as `/tmp` is fine, because
  others may add files there but not replace yours.

Group write is allowed, because Homebrew's `lib` directory is `775`. A library
that fails the check is refused with a message naming the offending directory.
When `discover_all_endpoint_plugins()` finds one, it logs a warning and skips it.

**Running as root, only root-owned libraries pass.** A service started as root
therefore never picks up a plugin a user installed into their own Homebrew
prefix or `~/.local/share`. Install system-wide plugins as root, or give the
service a user of its own.

Every library discovery loads is logged at `info` with its path and SHA-256, so
there is a record of what ran without being named.

A library loaded **by path** — `load_endpoint_plugin`, `--plugin`, a `plugins:`
entry — is not checked: naming the file is the trust decision. Windows has no
equivalent owner and mode to check, so discovery there relies on the directory
permissions of the install location; set `MQB_PLUGIN_DISCOVERY=0` and load by path
where that is not enough.

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

Both cover `pulsar`, `meilisearch` and `connect`. Neither depends on
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

Since **ABI 1.2** one library can export several endpoints. Each entry takes
the same arguments as `export_endpoint_plugin!`:

```rust
mq_bridge::export_endpoint_plugins! {
    { name: "pulsar", factory: PulsarFactory },
    { name: "pulsar-admin", factory: AdminFactory, capabilities: mq_bridge::plugin::sdk::CAPABILITIES_OUTPUT_ONLY },
}
```

Loading the library registers all of them, or none if one name is taken.
A route's lookup opens only the file named for the endpoint it asks for, so
install the library under each name a route may ask for (a symlink will do), or
load it explicitly. `discover_all_endpoint_plugins()` finds every entry either
way. A 1.0/1.1 host sees only the first entry.

### Shared helpers

Three things most endpoints need, so a plugin doesn't write them itself:

- **`mq_bridge::errors::InvalidConfig`.** Return a config error wrapped in it from
  `create_consumer` or `create_publisher`, and the route stops instead of
  reconnecting forever: `config::resolve(value).map_err(InvalidConfig)?`. The
  same wrapper works for both sides, linked directly or loaded as a plugin.
- **`mq_bridge::support::stream_batch::next_batch`.** Collects one batch from a
  client that hands out messages as a `Stream`. A live route waits for the first
  message; a draining one (`exit_on_empty`) gets an empty batch from an idle
  source after 250 ms, which is what ends the drain. A stream error comes back as
  a `PartialBatch` holding the messages collected before it: deliver those, then
  report the error on the next call.
- **`SentBatch::from_failures`.** `Ack` when nothing failed, otherwise a
  `Partial` naming the failed messages.

A batch's commit function gets exactly one disposition per message, so it needn't
count them; the plugin host rejects any other count before calling it.

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
impl CustomEndpointFactory for ConnectFactory {
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
`serde_json::to_value(schemars::schema_for!(ConnectConfig)).ok()`. Nothing but
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

A parameter the schema does not declare stays a string. A plugin that takes
open-ended options (`additionalProperties`) and cannot declare their types can
set `"x-mqb-uri-infer-scalars": true` at the schema's top level: an undeclared
value of `true` or `false` becomes a boolean, a plain integer an integer, and a
decimal like `0.5` a number. Anything else stays a string, including `007`,
`1.0.0` and integers too large for 64 bits.

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
mq-bridge --input 'connect+mqtt://localhost:1883/orders' --output 'kafka://...'
```

This is the spelling `git+ssh://`, `svn+ssh://` and SQLAlchemy's
`postgresql+psycopg2://` made familiar. The part before the `+` names the
plugin, so that is what the host looks the factory up by; the part after it is
the plugin's own vocabulary, and a field annotated `subscheme` receives it.

Everything after the scheme then describes the inner protocol rather than the
plugin, so `origin` and `url` are handed over carrying the inner scheme:

| From `connect+mqtt://host:1883/orders` | |
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
endpoint `connect` configuration: unknown field `topci`; this endpoint takes
batch_size, group, topic, url
```

It is off by default on purpose. The schema is yours, not the route's, and the
host checks only the subset the `transform` middleware already validates against
— `type`, `required`, `enum`, `items`, nested `properties`, plus unknown
top-level fields when you set `additionalProperties: false`. A schema using more
than that (`oneOf`, `$ref` to a remote document, `patternProperties`) is logged
as uncheckable and passed through rather than rejected, so describing yourself
richly for the sake of a form never costs you a working endpoint.

### Declaring a delivery guarantee

A route logs — and with `required_delivery` enforces — the guarantee it can give
(see [DELIVERY.md](DELIVERY.md)). For a custom endpoint only its author knows the
answer, so the factory states it. Two top-level schema annotations cover the
common case and cross the plugin boundary unchanged:

| Annotation | Meaning | Default |
| --- | --- | --- |
| `x-mqb-idempotent-sink` | writing the same record twice leaves one effect, so a route into this sink is effectively-once | `false` |
| `x-mqb-acknowledges` | the consumer acks, so a message lost in a crash is redelivered; `false` makes a route from it at-most-once | `true` |

```json
{ "type": "object", "x-mqb-idempotent-sink": true, "properties": { "...": {} } }
```

When the answer depends on the configuration — idempotent only with a key set —
override `CustomEndpointFactory::idempotent_sink` or `acknowledges` instead; both
receive the endpoint's `config`. Since ABI 1.2 the host asks a loaded plugin the
same way (`factory_delivery`); a 1.0/1.1 plugin is judged by its schema alone. Claim idempotency
only for a write keyed on something replay-stable: the route trusts it.

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

### Request/reply and status

Since **ABI 1.2** request/reply works through a plugin in both directions:

- **Publisher:** the responses your `send_batch` returns (`Sent::Response`,
  `SentBatch::Partial { responses, .. }`) cross the boundary. The route matches
  each one to its request by `message_id`, exactly as for a linked endpoint.
  Failures and responses in the same batch both survive.
- **Consumer:** a `MessageDisposition::Reply` reaches your batch's commit
  together with the reply message, so a plugin source can answer the request it
  received.

`status()` crosses too: the host asks the plugin, and your endpoint's
`EndpointStatus` is what a host shows. A consumer busy in `receive_batch`
answers healthy with `details.state = "receiving"` rather than waiting for its
next message.

Receive, commit, send and flush no longer tie up a host thread either: the host
starts the call and the plugin reports back through a completion callback when
its runtime has finished. Before 1.2 each of those calls held a thread of the
host's blocking pool for its whole duration.

That is measurably faster. The same in-memory endpoint, which does no work of its
own, was built against the 1.1 and the 1.2 SDK and driven by the same host
(messages per second, macOS arm64, release build):

| Batch | Call | 1.1 | 1.2 | Gain |
| ---: | :--- | ---: | ---: | ---: |
| 1 | send | 94k | 154k | 1.6× |
| 1 | receive + commit | 46k | 76k | 1.6× |
| 1 | send, 16 concurrent callers | 164k | 708k | 4.3× |
| 128 | send | 6.5M | 8.6M | 1.3× |
| 128 | receive + commit | 4.0M | 5.4M | 1.4× |
| 128 | send, 16 concurrent callers | 15.6M | 25.5M | 1.6× |

This measures only the boundary cost. It matters most for small batches and
many concurrent routes; a real broker's latency hides most of it at large
batches. Rebuilding a plugin against 1.2 is enough to get the gain.

Your `tracing` events and `metrics` samples reach the host as well. On load the
SDK installs a subscriber and a recorder inside the plugin that forward to the
host's own. The host re-emits every plugin event under the target
`mq_bridge::plugin` (`mq_bridge::plugin::PLUGIN_LOG_TARGET`), with your module
path in the `module` field, so `RUST_LOG=mq_bridge::plugin=debug` filters them.
Metrics keep their names and labels; forwarding them needs the `metrics` feature
of `mq-bridge` in the plugin and in the host. A plugin that installs its own
global subscriber or recorder first keeps it, and nothing is forwarded.

The SDK wires all of this up; there is nothing new to implement. A plugin built
against 1.0 or 1.1 publishes without responses, has a reply acknowledged like a
plain ack, reports the default status, runs its calls on the blocking pool, and
keeps its logs and metrics to itself.

### Limits of ABI v1

- The export macro defines the discovery symbols, so a crate uses it once and
  two plugin crates cannot be statically linked into one binary. Gate the macro
  behind a feature if that matters for your crate.

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

### Writing one in C or C++

The ABI is plain C, so a plugin does not have to be Rust. The usual reason is an
existing C library — a parser for a proprietary wire format, say — that should
run as a middleware without being rewritten.
[`include/mq_bridge_plugin.h`](../include/mq_bridge_plugin.h) declares the ABI;
[`include/mq_bridge_plugin_helpers.h`](../include/mq_bridge_plugin_helpers.h)
fills in every entry a plugin does not implement. A complete middleware that
drops heartbeat messages:

```c
#include <stdlib.h>
#include <string.h>
#include "mq_bridge_plugin_helpers.h"

static MqbStatus apply(MqbMiddlewareHandle middleware, const MqbMessage *messages, size_t len,
                       MqbFilterHandle *out_result, const MqbMessage **out_messages,
                       const uint8_t **out_kept, MqbBuffer *err) {
    uint8_t *kept = malloc(len + 1);
    if (kept == NULL) {
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    for (size_t i = 0; i < len; i++) {
        MqbSlice p = messages[i].payload;
        int ping = p.len == 4 && memcmp(p.ptr, "ping", 4) == 0;
        kept[i] = ping ? MQB_MESSAGE_DROPPED : MQB_MESSAGE_KEPT;
    }
    *out_result = kept;
    *out_messages = messages; /* kept messages pass through unchanged */
    *out_kept = kept;
    return MQB_OK;
}

static const MqbPluginVTable table = {
    MQB_TABLE_HEADER("drop_heartbeats", "0.1.0", MQB_CAP_MIDDLEWARE),
    MQB_DEFAULT_FACTORY,
    MQB_NO_CONSUMER,
    MQB_NO_PUBLISHER,
    MQB_STATELESS_MIDDLEWARE,
    .middleware_apply = apply,
    .middleware_result_free = free,
};

const MqbPluginVTable *mq_bridge_plugin_v1(void) { return &table; }
```

```sh
cc -shared -fPIC -I include examples/c-plugin/minimal.c -o libdrop_heartbeats.so
```

`middleware_apply` writes back two arrays as long as the input: the messages,
and one `MQB_MESSAGE_KEPT` / `MQB_MESSAGE_DROPPED` flag each. Both stay valid
until the host passes the result handle to `middleware_result_free`. The output
may point into the input, so an unchanged message — or a rewritten one's id and
metadata — needs no copy. The rules the Rust SDK enforces for you are yours to
keep:

- **Every function pointer must be set.** The host never checks for null; the
  helper macros cover what you don't implement.
- Other arguments are borrowed for the call only; copy what you keep.
- Error text goes into the `err` buffer (`mqb_set_error`), released through the
  table's `buffer_free`.
- Calls can arrive concurrently from several threads.
- In C++, never let an exception escape: catch it and return `MQB_ERR_PERMANENT`.
  The helper macros are C only; C++ fills the table in declaration order.

A plugin need not implement the non-blocking (`*_async`), per-message-outcome,
response or status entries. Where one answers `MQB_ERR_UNSUPPORTED`, the host
falls back to the plain blocking call, on a thread of its own, and reports a
healthy default status. `MQB_BLOCKING_PUBLISHER` fills in exactly those entries,
so a publisher sets only `publisher_create`, `_send_batch`, `_flush`, `_close`
and `_free`.

[`examples/c-plugin/plugin.c`](../examples/c-plugin/plugin.c) shows both sides
in one library, wrapping two unchanged "legacy" libraries. Its middleware turns
fixed-width records into JSON and logs the ones it drops (`mqb_log`). Its output
appends each message to a ledger file, taking the path from its config and a
mutex around the non-thread-safe library.

`mq_bridge_plugin.h` is generated from `src/support/plugin_abi.rs`, so it always
matches the host, and its `static_assert`s refuse to compile if the table layout
ever does not.

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
| 1.2 | `publisher_send_batch_responses` and `responses_free`, so publish responses reach the route; `batch_commit_replies` with `MQB_DISPOSITION_REPLY`, so a plugin consumer receives the reply to send; `consumer_status` / `publisher_status`, so `status()` reports the plugin's own state; and `*_async` twins of receive, commit, send and flush that finish through an `MqbCompletion` callback instead of blocking a host thread; `plugin_init` hands the plugin an `MqbHostVTable`, through which its logs and metrics reach the host; the optional `mq_bridge_plugin_v1_at` symbol exports several tables from one library; `factory_delivery` answers `idempotent_sink` / `acknowledges` per config. |

Publish the supported ABI range in your package metadata, and test each packaged
plugin against the oldest and newest mq-bridge you claim to support.

---

## See also

- [EXTENDING.md](EXTENDING.md) — custom endpoints and middleware, including
  Python and JavaScript ones
- [REFERENCE.md](REFERENCE.md) — every built-in endpoint and middleware
- [ARCHITECTURE.md](ARCHITECTURE.md) — how routes, batching and commits fit together
