# Secrets & interpolation

Keep credentials out of committed config, and template message bodies from request data. There
are two related interpolation systems: **config-value env references** (resolved when the config
loads) and **`${namespace:selector}` message templating** (resolved per message, in `static`
bodies and a few endpoint fields).

## Config values from the environment

Reference environment variables anywhere in JSON/YAML/UI values with
`${ENV_VARIABLE_NAME:-default_if_not_found}`:

```yaml
orders_out:
  output:
    kafka:
      topic: "orders"
      url: "${KAFKA_URL:-localhost:9092}"
      # a password sourced from the environment, never committed
      username: "svc-orders"
      password: "${KAFKA_PASSWORD}"
```

For local development, drop a `.env` file in the working directory — it is loaded automatically.
In containers/Kubernetes, set the vars in the environment (and override any config field with
`MQB__{ROUTE}__{PATH}`; see [Configuration grammar](../engine/configuration.md#environment-variables)).
This is the recommended way to keep secrets out of source — see the
[deploying security checklist](../operations/deploying.md#security-checklist-for-production).

## Custom endpoints and plugins

When the app stores secrets outside the config file (the `balanced` config security mode), it
also takes them out of a plugin or custom endpoint's or middleware's config, such as a
[Connect](../connectors/connect.md) component:

- a string field whose name contains `password`, `passphrase`, `secret` or `credentials`, or
  ends in `token`, `api_key`, `apikey`, `access_key`, `account_key`, `private_key` or
  `connection_string`, at any depth;
- a URL with `user:pass@` in it, also inside a list such as `urls`, and a `dsn` containing `@`.

Left in the file, because an environment variable cannot carry them back unchanged:

- a field whose name is not lowercase `snake_case` (`apiKey`, `X-Api-Key`), and everything below it;
- a value that reads as a number or boolean (`"12345"`, `"true"`);
- a value that already is a reference (`"${DB_PASSWORD}"`);
- a Connect `yaml` document.

For those, reference the secret from the environment as above.

## Encryption keys from the environment

The [`encryption`](encryption.md) middleware and the file endpoints read their key with the
`${env:VAR}` form:

```yaml
- encryption: { key: "${env:MQB_ENC_KEY}" }
```

## Message templating with `${namespace:selector}`

The [`static`](../engine/reference.md#static) endpoint's `body` is a template compiled once at
startup. Tokens use the `${namespace:selector}` form:

| Token | Resolves to |
|---|---|
| `${payload:a.b.c}` | a field of the incoming JSON payload (dotted path; array indices allowed) |
| `${metadata:key}` | a metadata value |
| `${message:id}` | the message id (UUID string) |
| `${gen:uuid}` | a fresh UUID v7 |
| `${gen:now}` / `${gen:timestamp}` | current time (RFC3339 UTC / Unix epoch ms) |
| `${gen:counter}` | a per-endpoint counter, starting at 0 |
| `${gen:random(1,100)}` | a random integer in `[min, max]` |
| `${env:VAR}` | an environment variable, resolved once at startup |

```yaml
output:
  static:
    body: '{"error":"not found","id":"${message:id}","at":"${gen:now}"}'
    raw: true
    metadata: { content-type: "application/json" }
```

- `payload`/`metadata`/`message` read the request, so they're the useful ones on an **output**
  (e.g. an error reply echoing the request); on an **input** (a load-test source) only `gen`/`env`
  produce values.
- When the body's `content-type` is a JSON type, interpolated request values are **JSON-escaped
  by default** so external data can't break the structure — append `| raw` to a token to splice
  it verbatim.
- To emit a literal `${…}`, write `$${…}`. Any `${…}` with an unknown namespace is left
  untouched.

## SQL / query token mapping

The `sqlx` `insert_query` supports the same request-binding tokens so a typed value goes into the
statement: `${payload:field}` binds a typed JSON value, `${metadata:key}` binds a metadata
string. See [Upserts & insert-if-absent](upserts.md).
