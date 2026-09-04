# CLI commands

`mqb` is a single headless binary with three modes: **config mode** (the default —
run a long-lived bridge, optionally serving the browser UI), the **`copy`** subcommand (an
ad-hoc one-route job), and the **`mcp`** subcommand (expose the bridge as MCP tools).

```text
mqb [OPTIONS]                          # config mode
mqb copy SOURCE TARGET [COPY OPTIONS]  # one-route ad-hoc job
mqb mcp  [MCP OPTIONS]                 # MCP server
```

## Config mode (default)

Run with no subcommand to load a config and run a bridge; with no config at all it starts
empty and offers to serve the UI so you can build one interactively.

```bash
mqb --config config.yml
mqb --config config.yml --init-config dev/config/file-to-http.yml
mqb --ui                           # start empty, define config.yml in the UI
```

| Option | Meaning |
|---|---|
| `-c, --config <path>` | Config file to load **and** save (the UI writes back here). |
| `-i, --init-config <path>` | Initialize from a template file only if the main config doesn't exist yet. |
| `--init-config-str <str>` | Initialize from an inline config string if the main config doesn't exist yet. |
| `--config-str <str>` | Inline config that overrides the config file. |
| `--ui` | Serve the browser UI on the default port without asking — see [Starting the web UI](#starting-the-web-ui). |
| `--no-ui` | Never serve the browser UI, and don't ask. |
| `--metrics-addr <addr>` | Serve the Prometheus endpoint on `addr` (default `127.0.0.1:9090`), overriding `metrics_addr` from the config. |
| `--no-metrics` | Don't serve the Prometheus endpoint on its own port. |
| `--schema <path>` | Write the JSON Schema for `AppConfig` (use `-` for stdout) and exit. |
| `--plugin <path>` | Load a native endpoint/middleware library before starting. Repeatable, valid on every subcommand, and combines with `plugins:` in the config — see [Native plugins](../extending/plugins.md). |

Config is hierarchical (files + environment variables) — see
[Configuration grammar](../engine/configuration.md).

### Starting the web UI

The UI is a control surface, so its port is never opened implicitly. What happens
in config mode depends on where the address comes from:

| Situation | Result |
|---|---|
| `ui_addr` set in the config | Served on that address — configuring it *is* the consent |
| No `ui_addr`, `--ui` passed | Served on `0.0.0.0:9091` |
| No `ui_addr`, `--no-ui` passed | Not served, no prompt |
| No `ui_addr`, interactive terminal | Asks `Start the web UI on 0.0.0.0:9091? [y/N]` — anything but `y`/`yes` declines |
| No `ui_addr`, no terminal (script, service, CI) | **Not served.** Pass `--ui` to opt in |

The last row is the important one: a run started by a script or a service unit
never puts the UI on the network by accident. Nothing about the bridge itself is
gated — configured routes run either way.

In a container the calculation is reversed, because nothing is reachable until
you publish it. The Docker image's `CMD` therefore asks for the UI on your
behalf — see [Ports in containers](../operations/deploying.md#ports-in-containers).

### The metrics endpoint

Metrics are always collected, and always available at `/metrics` on the web UI
when it runs. Separately, config mode serves a standalone Prometheus endpoint on
**`127.0.0.1:9090`** by default.

It defaults to loopback rather than `0.0.0.0` because, while the endpoint is
read-only, it still describes the routes and endpoint types in use — a bare run
on a workstation shouldn't publish that to the local network. Scraping from
another host is an explicit choice:

```bash
mqb --config config.yml --metrics-addr 0.0.0.0:9090   # scrapeable
mqb --config config.yml --no-metrics                  # no separate port at all
```

`metrics_addr` in the config does the same thing; the flag overrides it.

## `copy` — ad-hoc one-route job

Builds a single route from two endpoint URIs and runs it headlessly (no web UI). The scheme
selects the endpoint and query parameters set its config.

```bash
# DB → DB, drain the source table then exit (exit code 0 on success)
mqb copy \
  'postgres://user:pass@localhost/db?table=src' \
  'postgres://user:pass@localhost/db?table=dst' \
  --drain

# Queue → DB as a continuous bridge (runs until Ctrl-C; omit --drain)
mqb copy \
  --from 'nats://localhost:4222?subject=orders' \
  --to   'postgres://user:pass@localhost/db?table=orders'
```

| Flag | Default | Meaning |
|---|---|---|
| `SOURCE TARGET` | required | Positional source and destination endpoint URIs. |
| `--from <uri> --to <uri>` | — | Backward-compatible alternative to the positional form. |
| `--filter <expr>` | off | Retain messages for which the expression is true. Top-level JSON scalar fields are variables. |
| `--resume` | off | Configure the source's safe native resume mechanism, or fail before route startup. |
| `--no-resume` | off | Ignore optional cursor/checkpoint resume state, including its warnings and errors. Conflicts with `--resume`; native queue/CDC offsets are unchanged. |
| `--drain` | off | Exit once the source yields an empty batch. Without it, `copy` runs as a continuous bridge until Ctrl-C. |
| `--concurrency <N>` | `4` | Route concurrency. |
| `--batch-size <N>` | `1024` | Batch size. |

> Note: `copy`'s defaults (`--concurrency 4`, `--batch-size 1024`) are higher than the
> library's route defaults (`concurrency: 1`, `batch_size: 512`), because `copy` is built for
> bulk throughput. See [Performance tuning](../operations/tuning.md).

### Resumable copies

By default a bounded `copy` re-reads the whole source every run. `--resume` derives a stable
state identity from the credential-redacted source, destination, and filter, then maps it to
the source's existing mechanism. Changing any of those pipeline semantics starts new state;
rotating a password does not.

```bash
mqb copy \
  'postgres://user:pass@localhost/app?table=orders&cursor_column=id' \
  'file:///data/orders.jsonl' \
  --resume \
  --drain
```

Currently supported mappings are Kafka consumer groups, MongoDB `capture_all`/`capture_new`
cursors, persistent Postgres CDC slots, SQL cursor readers with an explicit monotonic
`cursor_column`, and ClickHouse/object-store cursor readers with their required explicit
external `checkpoint_store`. Explicit `group_id`, `cursor_id`, `slot_name`, and
`checkpoint_store` URI settings remain advanced overrides.

File offsets are deliberately not accepted yet because partial batch failure can advance the
current file offset past a failed record. NATS is also rejected because its generated durable
consumer name cannot currently include the destination and filter. Other non-replayable sources,
including MQTT, fail early instead of silently ignoring `--resume`. Full checkpoint details:
[Checkpoints & resumable copies](../cookbook/checkpoints.md).

When a full re-copy is intentional, `--no-resume` skips optional `cursor_id` and
`checkpoint_store` handling for SQL/ClickHouse cursor readers, MongoDB change streams, and
object-store sources. A SQL or ClickHouse `cursor_column` is still required for paging.
For native-position sources such as Kafka, NATS, and Postgres CDC, `--no-resume` is intentionally
a no-op; configure their normal group, durable, slot, or start-position settings instead.

### Filtering

`--filter` is evaluated by mq-bridge after the source read and before URI-configured transform
middleware. It is not translated to SQL or MongoDB, so connector-native predicates remain a
separate optimization and keep their existing behavior.

```bash
mqb copy \
  'kafka://localhost:9092?topic=orders' \
  'postgres://localhost/app?table=orders' \
  --filter 'country == "DE" && amount >= 50' \
  --resume
```

An expression reads payload fields by bare name, including nested paths (`order.status`), and
message metadata under the reserved `meta.` prefix (`meta.kind`). Metadata is always text, as are
CSV fields and a SQL source's `numeric` and timestamp columns, but comparing one against a numeric
literal reads it as a number: `meta.retry_count < 3` and `amount >= 50` both work untouched. A
string literal still compares as text (`zip == "01234"`), and `number()` remains available.

A true result continues to the destination. A false result is an intentional successful drop
and advances the source acknowledgement/checkpoint. Invalid expressions and payloads that are
not a JSON object are errors. A referenced field that is absent, null, or holds an array or
object counts as *no match* instead, the way a SQL `WHERE` treats NULL, so one heterogeneous
record does not end a copy that is otherwise running fine. The first such field is logged once
as a warning, so a typo in the expression does not simply look like an empty source.

#### Object naming under a filter

An `object_store` sink names each object after the *contiguous* source range it covers whenever
`name_by` resolves to `source_position` — which is what `auto` picks for any source that stamps
a replay position. A filter leaves holes in every batch, and each hole starts another object:
measured at 159,474 objects instead of 977 on a 1M-row copy, a ~220x drop in throughput.

So when a row-dropping middleware is present — `--filter`, `deduplication`, `weak_join`, or a
`transform` with `on_error: reject` on either endpoint — and the sink is still on `name_by: auto`,
the route resolves it to `write_time` instead and warns that it did. mq-bridge applies this to
every route, so a `switch` in `when` mode with no `default` and a config-defined route get the
same treatment. That has two consequences
worth knowing:

- Objects are named by uuidv7 at write time, so at the default `--concurrency 4` their order is
  the order batches finish encoding, not source order. Order *within* an object is unchanged.
  Use `--concurrency 1` if order across objects matters.
- The copy is no longer effectively-once at the sink: a crash mid-batch rewrites those rows
  under fresh names rather than colliding harmlessly with identical ones.

Pass `name_by=source_position` explicitly to keep replay-safe names and accept the
fragmentation. An explicit setting — including the deprecated `idempotency` alias — is never
overridden.

### URI grammar

`scheme://…?param=a&next=b`: the **scheme selects the endpoint** and **query parameters set
its config**. Any query key that matches a field of that endpoint's config becomes endpoint
config; every other query param stays on the connection URL, so driver params pass through
unchanged (e.g. `postgres://…/db?table=src&sslmode=disable`).

- **Schemes**: `postgres` / `postgresql` / `mysql` / `mariadb` / `sqlite` → sqlx, `nats` →
  NATS, `mongodb` → MongoDB, `redis` → Redis streams, `file` → file,
  `s3` / `gs` / `az` / `abfs` → cloud object storage (credentials from the environment), and
  the rest by name.
- **Common config params**: `table`, `insert_query` (URL-encoded; supports
  `${metadata:<key>}` / `${payload:<field>}` token mapping), `delete_after_read`, `subject`,
  `stream`, `collection`, `database`, `format`, … — anything on the endpoint's config struct.
- For `nats`, the dominant target field can be given as the URL path
  (`nats://localhost:4222/orders` ≡ `?subject=orders`); the query form wins if both are given.
  A `redis` path is the connection's database number, so a redis stream target must use
  `?stream=…`.
- MongoDB sources are **non-destructive by default**: `consume` defaults to `capture_all`,
  which needs a replica set (a single-node one is enough). On a standalone `mongod` pass
  `?consume=snapshot` for a one-shot read. `?consume=consumer` opts into the destructive
  queue-drain mode.

### Middlewares in the URI

Append `|`-separated middlewares to either URI to wrap that endpoint. They apply in the order
written, and each takes its own config struct's fields as query params:

```bash
mqb copy \
  --from 'postgres://user:pass@localhost/db?table=src|retry?max_attempts=5&initial_interval_ms=200' \
  --to   'kafka://broker:9092?topic=orders|buffer?max_messages=500&max_delay_ms=50|metrics' \
  --drain
```

- Names: `retry`, `metrics`, `dlq`, `deduplication`, `transform`, `delay`, `limiter`,
  `buffer`, `weak_join`, `cookie_jar`, `random_panic`, `compression`, `encryption`, `custom`
  (`-` is accepted for `_`).
- `encryption`'s `key` is a shell-visible argument; prefer `${env:VAR}` to keep it out of the
  process list and shell history: `|encryption?key=$%7Benv:MQB_KEY%7D`.
- `compression` and `encryption` produce **binary** payloads, so a `file` sink holding them must
  use `format=normal`. `format=json`/`text` render the payload as a JSON value and it does not
  survive the round trip (it comes back as a JSON array, and the reader reports a bogus
  "unsupported encryption envelope version 91").
- Middlewares apply in list order on **both** ends, so a route that reads back what another wrote
  must list them in the **reverse** order. Writing with `|compression?algorithm=zstd|encryption?key=…`
  reads back with `|encryption?key=…|compression?algorithm=zstd`.
- A middleware with no params needs no `?` — `|metrics`.
- `dlq`'s `endpoint` is itself a URL-encoded endpoint URI:
  `|dlq?endpoint=file%3A%2F%2F%2Ftmp%2Ffailed.jsonl`.
- Object/array fields take a JSON literal:
  `|weak-join?group_by=cid&expected_count=2&timeout_ms=1000&required=["a","b"]`.
- A literal `|` inside the URI (e.g. in a password) must be written percent-encoded as `%7C`.

### Structural endpoints in the URI

Structural endpoints have no connection of their own — they arrange other endpoints. Their
nested endpoints are query params that are themselves endpoint URIs:

| URI | Meaning |
|---|---|
| `null:` | Discards everything. |
| `static:?body=…&raw=true` | A fixed message: a constant source or a constant reply. |
| `response:` | Replies to the caller; needs a source that carries a reply channel (`http`, `websocket`). |
| `fanout:?to=<uri>&mirror=<uri>` | Sends every message to each branch, in the order written. |
| `request:?to=<uri>&forward_to=<uri>` | Sends to a request-capable endpoint and forwards the response elsewhere; without `forward_to` the response is discarded. |
| `switch:?metadata_key=<key>&case.<value>=<uri>&default=<uri>` | Picks one destination by a metadata value. |
| `switch:?when=<expression>&to=<uri>&default=<uri>` | Picks the first destination whose predicate matches. |

A nested URI only needs percent-encoding when it carries `&`, `#` or `|` of its own —
`fanout:?to=http://prod.internal/?method=PUT` is fine as written, while an inner `&` must be encoded,
as in `fanout:?to=http%3A%2F%2Fprod.internal%2F%3Fmethod%3DPUT%26timeout_ms%3D1000`.

`switch` has the two modes the engine has, and takes one or the other, never both. Value lookup
branches on a metadata value; predicate mode takes `when=<expression>` / `to=<uri>` pairs in the
order written, first match wins, using the same expression language as [`--filter`](#filtering):

```bash
mqb copy \
  'kafka://localhost:9092?topic=orders' \
  'switch:?when=amount > 1000&to=kafka%3A%2F%2Flocalhost%3A9092%3Ftopic%3Dlarge&when=true&to=file%3A%2F%2F%2Fdata%2Frest.jsonl'
```

An expression travels as a query *value*, so its `==` needs no escaping. A literal `&` would
split the query, so write `and` / `or` rather than `&&` / `||` — the engine accepts both
spellings. A message matching no predicate goes to `default`, and without a `default` it is
dropped.

`fanout`'s two branch kinds differ in what may come back: a `to` branch is used as written, while
a `mirror` branch has its response **and its failures** discarded, so it can neither answer the
caller nor fail the message for the other branches. That makes the mirroring proxy one command —
serve requests, copy each to staging, answer from production:

```bash
mqb copy \
  --from 'http://0.0.0.0:8080' \
  --to   'fanout:?mirror=http://staging.internal/&to=http://prod.internal/'
```

> The engine does not forward a branch's response through a fan-out yet, so the caller currently
> gets `202 Accepted` rather than production's body. The mirroring half works today; a single
> `--to http://prod.internal/` (no fan-out) does reply with the real response.

An HTTP source is a listener, so `--from http://0.0.0.0:8080` binds that address, and `https://`
makes it a TLS listener. Its certificate is the `tls` field, which takes a JSON literal:

```bash
mqb copy \
  --from 'https://0.0.0.0:8443?tls={"required":true,"cert_file":"cert.pem","key_file":"key.pem"}' \
  --to   'http://prod.internal/'
```

### Escape hatch: full connection strings

Any query parameter that isn't a recognised config field (e.g. `sslmode`, `replicaSet`) stays
on the connection URL, so driver options just work. That includes a name shared with an
object-typed config field: `?tls=true` reaches the driver, while only an actual JSON literal —
`?tls={"required":true,"ca_file":"ca.pem"}` — is read as endpoint config.
If you already have a complete connection string, pass it verbatim with `?url=<url-encoded>`:

```bash
mqb copy \
  --from 'mongodb://_/?url=mongodb%3A%2F%2Fuser%3Apass%40host%2Fdb%3Ftls%3Dtrue&collection=orders' \
  --to null:
```

See the [Quick start](../quick-start.md) for complete, working `copy`
commands.

## `mcp` — MCP server

```bash
mqb mcp                                    # stdio (local clients)
mqb mcp --transport http --bind 127.0.0.1:9092   # streamable HTTP
```

| Flag | Default | Meaning |
|---|---|---|
| `--transport <stdio\|http>` | `stdio` | Transport. `stdio` for local clients (Claude Desktop/Code), `http` for streamable HTTP over hyper. |
| `--bind <addr>` | `127.0.0.1:9092` | Bind address; `--transport http` only. |
| `--report-to-ui` | off | Report running routes / publish targets to a local mq-bridge-app UI over a local IPC socket. Only names, connector types, health and counts are sent — never URLs or credentials. |

### `mcp install` / `uninstall` / `status`

Register the running binary with local MCP clients so you don't write the config by hand:

```bash
mqb mcp install                         # every detected client
mqb mcp install --client cursor --local  # one client, project-scoped
mqb mcp install --report-to-ui           # bake --report-to-ui into the entry
mqb mcp status
mqb mcp uninstall
```

| Subcommand | Flags | Purpose |
|---|---|---|
| `install` | `--client`, `--local`, `--report-to-ui`, `--print-config` | Register this binary (its absolute path). |
| `uninstall` | `--client`, `--local` | Remove the registration. |
| `status` | `--local` | Show where it is registered and whether the path is still current. |

`--print-config` prints the JSON snippet for a client not written directly. Full tool and
message reference is in [MCP server](../MCP.md).
