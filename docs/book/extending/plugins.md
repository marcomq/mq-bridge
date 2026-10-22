# Native plugins

A plugin is a shared library holding an endpoint — and optionally a middleware — that
this binary never compiled: a proprietary broker, an in-house transport. Loading one
registers it under its own name, after which routes address it like any built-in
connector.

> **Pulsar is compiled in, so it is not a plugin here, and `meilisearch` is a built-in endpoint.** Address Pulsar directly, as
> `pulsar: { url: "pulsar://localhost:6650", topic: ..., subscription: ... }` in a route,
> or `pulsar://localhost:6650?topic=...` from `copy`. Passing
> `libmq_bridge_pulsar.{so,dylib}` to `--plugin` or `plugins:` fails at startup with
> `` `pulsar` ... is already registered by another factory `` — that rejection is
> deliberate, since a second factory under a live name would silently reroute traffic.
> Other hosts that did *not* compile it in, such as the Python binding (`mq-bridge` on PyPI), do load it as a plugin.
>
> To run a plugin in place of a built-in endpoint of the same name, install it (Homebrew,
> conda) or put its directory in `MQB_PLUGIN_DIR`, and set `MQB_PLUGIN_OVERRIDE=<name>`. For
> example, `MQB_PLUGIN_OVERRIDE=meilisearch` uses the Meilisearch plugin, which has
> [options the built-in endpoint leaves out](../connectors/meilisearch.md#built-in-or-plugin). `--plugin` and `plugins:` do not work for this. With `-v`,
> startup logs which copy is in use.

This is the runtime counterpart to [custom endpoints](custom-endpoints.md): a custom
endpoint is registered programmatically by code you compile in, a plugin is loaded from
a file named in the config.

## Loading

List the libraries under `plugins:`. Paths go through the usual placeholder expansion,
so `${VAR}` works:

```yaml
plugins:
  - "${MQB_PLUGIN_DIR}/libmq_bridge_acme.so"

routes:
  orders:
    input:
      acme: { url: "acme://localhost:9000" }
    output:
      file:
        path: "orders.jsonl"
```

The key is the name the plugin exports, not the file name. The long form
`custom: { name: acme, config: {...} }` is the same endpoint, for configs validated against
`mq-bridge.schema.json`. A plugin providing a
middleware registers that under the same name, usable in any `middlewares:` chain.

Plugins are loaded only from trusted startup configuration. The UI and `POST /config` may
reorder or repeat the same canonical paths, but cannot add, remove, or retarget a plugin.
Edit the startup config and restart the process for every plugin change. Loaded native
libraries remain mapped and registered for the process lifetime; they are never unloaded.

Every build can load plugins; there is no cargo feature to enable.

## From the CLI

`--plugin <path>` loads a library without touching the config, repeatable, and valid on
every subcommand:

```bash
mqb --plugin ./libmq_bridge_acme.so --config config.yml
```

It combines with `plugins:` rather than replacing it. Listing the same library both ways
is harmless — a library already loaded is not loaded twice.

`copy` takes plugin endpoints too: once a factory is registered, its name works as a URI
scheme, with the query params becoming its config fields.

```bash
mqb copy --plugin ./libmq_bridge_acme.so \
  --from "acme://localhost:9000?stream=orders" --to "file:///tmp/orders.jsonl"
```

The URI carries strings only — `url` is the part before the `?` (override it with an
explicit `?url=`), and every other param is passed to the factory as a string field. Use
config mode for a factory whose config needs numbers, booleans, or nested objects.

## Failure modes

Loading is strict, because the alternative is a confusing "unknown endpoint" much later,
once a route asks for something nobody registered:

| Condition | Result |
|---|---|
| File missing or unreadable | startup fails naming the path |
| Built against an incompatible ABI major version | rejected |
| Declares neither an endpoint nor a middleware | rejected |
| Name already taken by a different factory | rejected — traffic would silently reroute |

At startup a bad path aborts the process. A runtime config whose canonical plugin set differs
from the startup set is rejected before route validation, storage changes, or saving.

## Security

A plugin is native code loaded into this process, with the same privileges — it is not
sandboxed. Treat the startup configuration and CLI flags as trusted inputs, and treat the
libraries they name exactly like any other native dependency. Runtime config updates cannot
load a new library.

Writing one is covered in [Writing a plugin](../engine/plugins.md).
