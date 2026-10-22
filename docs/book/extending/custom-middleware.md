# Custom middleware

Middleware wraps the message flow between an input and an output — retries, dedup,
transforms, and so on. When the built-in set doesn't cover your need, register your own
and select it from config by name.

## Config

Middleware attaches to a route side, so the list goes under `input:` or `output:`:

```yaml
output:
  mongodb:
    url: "mongodb://localhost:27017"
  middlewares:
    - my_enricher: { lookup_url: "http://enrich.internal" }
```

The key is the registered factory's name; everything under it is its config. The explicit
form is the same middleware, for configs validated against `mq-bridge.schema.json`:

```yaml
middlewares:
  - custom:
      name: "my_enricher"
      config: { lookup_url: "http://enrich.internal" }
```

| Field | Type | Required |
|---|---|---|
| `name` | string | yes — matches the registered factory |
| `config` | any JSON | yes — passed through to your factory |

## Implementing it

Implement the `CustomMiddlewareFactory` trait and register it before starting routes.
It exposes `apply_consumer` and/or `apply_publisher` — each defaults to pass-through, so
you only implement the side you need. The consumer and publisher sides are separate
because middleware wraps a `MessageConsumer` on the way in and a `MessagePublisher` on
the way out; wrap order matters (see the architecture doc).

Registration is process-global and keyed by name: it must happen before any route that
names it starts, and registering a name twice is an error rather than a silent
replacement. Python and Node.js can register a middleware directly, with the same
semantics — no Rust required.

## See also

- [`custom` (middleware) reference](../engine/reference.md#custom-middleware) — the authoritative field list.
- [Writing endpoints & middleware](../engine/extending.md) — traits, registration, wrap order, and the Python/Node equivalents.
- [Custom endpoints](custom-endpoints.md) — the endpoint equivalent.
