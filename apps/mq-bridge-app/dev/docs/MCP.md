# MCP server

<p align="center">
  <img src="images/logo.png" alt="mq-bridge-app" width="128" height="128">
</p>
<p style="margin-top:-12px" align="center"><em>data movement as a tool call</em></p>

`mqb mcp` runs the bridge as a [Model Context Protocol](https://modelcontextprotocol.io)
server, so an agent can move data between **any two supported endpoints** — 15+
connectors covering databases, queues, brokers, HTTP, and files — from natural
language.

Nothing is preconfigured. `publish` and `start_route` take their endpoints inline
as JSON keyed by connector type, so the model picks both ends ad hoc; there is no
YAML to write first and no per-connector tool to install. `list_routes`,
`route_status`, `route_messages`, and `stop_route` manage what is already running.

**The rows never enter the model's context.** The agent describes the job; the
engine moves the bytes. Moving a 116.3 MiB dataset costs three tool calls and
~385 tokens — the same ~385 tokens whether the job is 1,000 rows or 1,000,000.

| | Registry name | `io.github.marcomq/mq-bridge-app` |
| --- | --- | --- |
| **Transports** | `stdio`, streamable HTTP | |
| **Tools** | 10 | [see below](#tools) |
| **Connectors** | 15+ | [see below](#endpoints) |
| **Install** | `mqb mcp install` | Docker / cargo / Homebrew / binaries — [Installation](INSTALL.md) |

## Quick start

```bash
# 1. get the binary (any install method from the Installation page)
brew install marcomq/tap/mq-bridge-app

# 2. register it with every MCP client detected on this machine
mqb mcp install

# 3. restart the client fully — reopening a tab is not enough
```

Then ask the agent for the job in words:

> *"Drain the `events` Redis stream into the `events` table in Postgres, creating
> the table if needed."*

## Running the server

```bash
# stdio — for local clients (Claude Code, Claude Desktop, Cursor)
mqb mcp

# streamable HTTP — for remote/shared clients
mqb mcp --transport http --bind 127.0.0.1:9092
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--transport` | `stdio` | `stdio` for local clients, `http` for streamable HTTP (served over hyper) |
| `--bind` | `127.0.0.1:9092` | Listen address; `--transport http` only |
| `--report-to-ui` | off | Also report route/publisher activity to a running desktop or web UI |

No web UI is started in this mode. Logs go to **stderr**, because `stdio`
transport owns stdout for the protocol itself.

## Tools

| Tool | Key arguments | Purpose |
| --- | --- | --- |
| `publish` | `publisher`, `message` \| `messages`, `name` | Send one message or a batch to any endpoint. Independent of routes. |
| `start_route` | `route` (`input`/`output`), `name`, `batch_size`, `concurrency`, `capture_last` | Run a route moving messages from source to sink. Returns the route name. |
| `list_routes` | — | Every route started by this server, with live connection health and rates. |
| `route_status` | `name` (optional) | Health, totals and rates for one route, or all of them. |
| `wait_route` | `name`, `timeout_ms` | Block until a route finishes, then report how it ended. One call instead of a polling loop. |
| `route_messages` | `name` | The most recent messages captured on a route. Requires `capture_last`; reads drain the buffer. |
| `stop_route` | `name` | Stop a route; returns total messages and the rate it achieved. |
| `server_info` | — | Crate version, git hash, build profile and build time, plus the agent bus. |
| `agent_listen` | `name`, `input`, `capture_last` | Open this server's agent inbox so other agents can reach it. Off until called. |
| `agent_send` | `to`, `message`, `output` | Send a message to another agent's inbox. Always available. |

Call `server_info` before quoting any throughput number: a debug binary reports
much slower rates, and the figure would be meaningless.

### Agent messaging

Two agents on one machine can message each other without a broker. `agent_listen`
opens this server's inbox — a `dir_spool` directory under `$MQB_AGENTS_DIR`
(default `~/.mqb-agents/<name>`) — and tails it for the session. Nothing can reach
an agent that has not called it.

```text
agent_listen {"name": "claude"}     # opt in; off by default
agent_send   {"to": "gpt-worker", "message": "..."}   # always available
route_messages {"name": "agent-inbox"}                # collect mail
```

Mail collected this way is held in a ring of the last `capture_last` messages
(default 200), and the inbox file is deleted as it is read. An agent that leaves a
session running without calling `route_messages` therefore loses the overflow —
raise `capture_last`, or collect more often.

`server_info` reports the bus: who this server listens as, and which peers have an
inbox and are currently listening. One message is one atomically renamed file, so a
peer that is not running collects its mail when it next listens, and any process
that can write a file can take part — no MCP client required:

```bash
f=~/.mqb-agents/claude/$(printf '%09d' $SEQ)-$(uuidgen | tr -d - | cut -c1-12)
printf '%s' '{"from":"gpt","message":"hi"}' > "$f.bin.tmp" && mv "$f.bin.tmp" "$f.bin"
```

Both tools take an optional endpoint (`input` / `output`) replacing the local
directory, so the same bus works over NATS, Kafka or anything else when the peer is
not on this machine.

#### Being woken by mail

MCP has no way to wake an agent: a server cannot start a turn, so `agent_listen`
can only hold the mail until the agent asks for it. A background job *can* wake
one, and `agent-listen` is that job — it blocks until mail arrives, writes it out
and exits, so its completion is the notification:

```bash
mqb agent-listen claude --wait 600    # blocks; exits when mail lands
```

It is sugar for `copy --from spool://<inbox> --wait <secs>`, and writes each
envelope as one line. Point it somewhere explicit with `--to`.

Use `agent_listen` for a mailbox you poll during a session, and `agent-listen`
when you want the arrival itself to interrupt you — but only one of them per
inbox. `dir_spool` allows a single consumer, so while an MCP server is listening
as `claude`, `mqb agent-listen claude` fails with `already held by a consumer`.

The refusal works in both directions: `agent-listen` holds the inbox for its
whole wait rather than polling it, so a second listener on the same name fails
instead of quietly splitting the mail with the first.

It stops at the first delivery, not at an empty inbox, so a burst that is still
arriving may leave messages queued. They are not lost — the next `agent-listen`
picks them up, and the summary line says how many came through this time.

`start_route` and `route_messages` are annotated **not read-only** — starting a
route moves real data, and reading captured messages consumes the buffer — so a
client should not auto-approve them the way it may auto-approve a true reader.

### Endpoints

Every endpoint is a single-key JSON object naming the connector:

```json
{"kafka":   {"url": "localhost:9092", "topic": "orders"}}
{"nats":    {"url": "nats://localhost:4222", "stream": "ORDERS", "subject": "ORDERS.new"}}
{"sqlx":    {"url": "postgres://user:pass@localhost:5432/db", "table": "events"}}
{"file":    {"path": "/tmp/out.jsonl", "format": "json"}}
{"null":    null}
```

The key is the connector; the object is its configuration:

| JSON key | Connector | Details |
| --- | --- | --- |
| `sqlx` | PostgreSQL / MySQL / MariaDB / SQLite | [connectors](connectors/postgres.md) · [parameters](reference/postgres.md) |
| `postgres_cdc` | PostgreSQL logical replication (CDC) | [connectors](connectors/postgres.md#postgresql-cdc) · [parameters](reference/postgres-cdc.md) |
| `clickhouse` | ClickHouse | [connectors](connectors/clickhouse.md) · [parameters](reference/clickhouse.md) |
| `mongodb` | MongoDB | [connectors](connectors/mongodb.md) · [parameters](reference/mongodb.md) |
| `kafka` | Apache Kafka | [connectors](connectors/kafka.md) · [parameters](reference/kafka.md) |
| `amqp` | RabbitMQ (AMQP) | [connectors](connectors/rabbitmq.md) · [parameters](reference/rabbitmq.md) |
| `nats` | NATS / JetStream | [connectors](connectors/nats.md) · [parameters](reference/nats.md) |
| `mqtt` | MQTT | [connectors](connectors/mqtt.md) · [parameters](reference/mqtt.md) |
| `redis_streams` | Redis Streams (alias `redis`) | [connectors](connectors/redis.md) · [parameters](reference/redis.md) |
| `aws` | AWS SQS / SNS | [connectors](connectors/aws.md) · [parameters](reference/aws.md) |
| `ibmmq` | IBM MQ (optional feature) | [connectors](connectors/ibmmq.md) · [parameters](reference/ibmmq.md) |
| `zeromq` | ZeroMQ | [connectors](connectors/zeromq.md) · [parameters](reference/zeromq.md) |
| `http` | HTTP client & server | [connectors](connectors/http.md) · [parameters](reference/http.md) |
| `websocket` | WebSocket | [connectors](connectors/websocket.md) · [parameters](reference/websocket.md) |
| `grpc` | gRPC | [connectors](connectors/grpc.md) · [parameters](reference/grpc.md) |
| `file` | Files — CSV / JSON / JSONL / raw | [connectors](connectors/file.md) · [parameters](reference/file.md) |
| `object_store` | Cloud object storage (alias `s3`) | [parameters](reference/file.md) |
| `sled` | Embedded local store | [endpoints](reference/endpoints.md) |
| `memory` | In-process topic | [endpoints](reference/endpoints.md) |
| `null` | Discards everything — `{"null": null}` as an `output` drains a source | [endpoints](reference/endpoints.md) |

Structural endpoints (`fanout`, `switch`, `stream_buffer`, `request`, `reader`,
`response`, `static`, `ref`) are accepted here too and are documented under
[Middleware & structural endpoints](engine/reference.md).

A `file`/`object_store` **source** must repeat the `compression`/`encryption` its
data was written with — neither is auto-detected, and a mismatch ends the route as
`completed` having moved few or no messages rather than failing. Check the
moved-message count.

### Messages

A message is a `payload` plus optional string `metadata` headers and an optional
`message_id` (a UUIDv7 is generated when omitted). A string payload is sent
verbatim; any other JSON value is serialized to JSON bytes.

```json
{
  "payload": {"id": 1, "sku": "A-100", "qty": 3},
  "metadata": {"kind": "order", "correlation_id": "abc-123"}
}
```

Conventional metadata keys: `kind` (message type, drives type-based routing),
`correlation_id` and `reply_to` (request/reply). Keys starting with `mqb.src.`
are reserved for provenance and are stripped on input.

### Route options

Alongside `input` and `output`, a route accepts `batch_size`, `concurrency`,
`exit_on_empty` (drain the source, then exit) and `capture_last`. Without
`exit_on_empty` a route polls indefinitely until `stop_route`.

`batch_size` and `concurrency` may be written inside `route` or as top-level
`start_route` arguments. They resolve most-specific-first:

1. the top-level argument, if given;
2. otherwise the same key inside `route`, if given;
3. otherwise the app default — **1024** and **4**, rather than the library's
   `512`/`1`.

Only a value you never wrote is replaced, so an explicit `1` is honoured either
way. `start_route` echoes the resolved `batch_size`/`concurrency` in its result.

`capture_last: N` keeps the last N messages that flow through the route for
`route_messages` to return. It is off by default: captured payloads are held in
memory and handed to the model verbatim, which is exactly what "the rows never
enter the context" otherwise avoids. Use it to *sample* a stream, not to move it.

### Middleware

Either endpoint may carry a `middlewares` array, so delivery behaviour is added
without changing the route:

```json
{
  "input":  {"kafka": {"url": "localhost:9092", "topic": "orders", "group_id": "agent"}},
  "output": {
    "sqlx": {"url": "postgres://user:pass@localhost:5432/db", "table": "orders"},
    "middlewares": [
      {"retry": {"max_attempts": 5}},
      {"dlq": {"endpoint": {"file": {"path": "/tmp/failed.jsonl"}}}}
    ]
  }
}
```

Available: `retry`, `dlq`, `deduplication`, `limiter`, `transform`, `compression`,
`encryption`, `buffer`, `delay`, `weak_join`, `cookie_jar`, `metrics`. Each is
documented with its options in the [Cookbook](cookbook/retries.md) and
[Middleware reference](engine/reference.md).

### Waiting for a job to finish

For a job started with `exit_on_empty`, call `wait_route` rather than polling
`route_status` in a loop:

```json
{"name": "redis-to-postgres", "timeout_ms": 300000}
```

It returns once the route's task ends, reporting `finished`, `outcome`,
`messages`, `elapsed_s` and `average_messages_per_second`. On timeout it returns
`finished: false` and **leaves the route running**, so the agent can wait again
or stop it. The default timeout is 60 s and the ceiling is 5 h; the server polls
internally every 50 ms, which the client never pays for.

This is what keeps agent token cost flat: one call covers a job of any duration,
where a polling loop costs a call per tick.

### Route status

`route_status` (and `list_routes`) report two rates, and they answer different
questions:

| Field | Meaning |
| --- | --- |
| `messages` | Total messages the route has moved. |
| `messages_per_second` | **Instantaneous** rate, smoothed over ~0.5 s. Decays to ~0 within a second of a route going idle. |
| `elapsed_s` | The span over which those messages moved: route start → last message seen. Stops growing once the route goes idle. |
| `average_messages_per_second` | `messages / elapsed_s` — the rate the route **actually achieved**. |

For a route that is running now, read `messages_per_second`. For one that has
finished — anything started with `exit_on_empty` — read
`average_messages_per_second`: the instantaneous rate of a completed job is ~0 by
the time any status call observes it, which says nothing about how fast it was.
`stop_route` returns the same two fields as its parting summary.

`elapsed_s` and `average_messages_per_second` are `null` until the route has been
sampled at least once (the sampler runs every 200 ms), so a job that finishes
inside one tick reports no average.

## Examples

Publish a batch to NATS JetStream:

```json
{
  "publisher": {"nats": {"url": "nats://localhost:4222", "stream": "ORDERS", "subject": "ORDERS.new"}},
  "messages": [
    {"payload": {"id": 1, "sku": "A-100"}, "metadata": {"kind": "order"}},
    {"payload": {"id": 2, "sku": "B-200"}, "metadata": {"kind": "order"}}
  ]
}
```

Drain a Redis stream into Postgres, then exit:

```json
{
  "name": "redis-to-postgres",
  "route": {
    "input":  {"redis_streams": {"url": "redis://localhost:6379", "stream": "events",
                                 "group": "g1", "read_from_start": true}},
    "output": {"sqlx": {"url": "postgres://user:pass@localhost:5432/db",
                        "table": "events", "auto_create_table": true}},
    "exit_on_empty": true
  }
}
```

Tail a live Kafka topic and keep the last 20 messages for inspection:

```json
{
  "name": "orders-tail",
  "route": {
    "input":  {"kafka": {"url": "localhost:9092", "topic": "orders", "group_id": "agent"}},
    "output": {"null": null}
  },
  "capture_last": 20
}
```

## Performance

The interface costs **one round-trip, not a per-row tax**: a route started through
`start_route` moves data at the rate the `copy` CLI does, and what separates them is
a fixed **~72 ms** of route startup and completion polling — 0.426 s against the CLI's
0.354 s on the same 1M-row dataset in the same session. Most of that is the measuring
client asking `route_status` every 50 ms, so completion is noticed up to one poll
late; the two tool calls bracketing the job cost ~0.05 ms each at p50. The gap is
fixed, so it shrinks to nothing as a proportion as the dataset grows — and it is more
visible than it used to be only because the job itself got ~2.6x faster.

{{#include ../../benches/etl/README.md:mcp_results}}

Agent token cost is **flat in the number of rows moved** — three tool calls
(`start_route`, one `route_status`, `stop_route`) totalling ~1.5 KB of JSON-RPC
move 1M rows or 1,000 alike. Passing the same 116.3 MiB through a context window
instead would cost ~30.5M tokens, which no context window holds at any price.

Methodology and the client used to measure it — a real MCP client over stdio, not
an in-process harness — are in
[`benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/benches/etl/README.md),
scenario 7. Engine-level tuning is on the
[Performance tuning](operations/tuning.md) page.

## Registering with a client

`mcp install` writes the client config for you, registering the **absolute path
of the binary you just ran** — so a `target/debug` build and an installed
release binary each register themselves correctly.

```bash
# every client detected on this machine
mqb mcp install

# a single client, project-scoped rather than global
mqb mcp install --client cursor --local

# bake --report-to-ui into the registered command
mqb mcp install --report-to-ui
```

| Command | Purpose |
| --- | --- |
| `mcp install` | Register this binary. `--client`, `--local`, `--report-to-ui`, `--print-config`. |
| `mcp uninstall` | Remove the registration. `--client`, `--local`. |
| `mcp status` | Show where it is registered and whether the path is still current. |

| Client | Global config | Project config (`--local`) |
| --- | --- | --- |
| `claude` (Claude Code) | `claude mcp add --scope user`, else `~/.claude.json` | `--scope project`, else `./.mcp.json` |
| `claude-desktop` | `claude_desktop_config.json` | not supported |
| `cursor` | `~/.cursor/mcp.json` | `./.cursor/mcp.json` |

Where the client ships its own CLI (Claude Code) that CLI is driven, since it
stays correct across config-schema changes; otherwise the entry is merged into
the client's JSON, leaving every other registered server untouched. Installing
twice is a no-op.

With `--client` omitted, every client detected on the machine is configured.
For anything not listed above, `mcp install --print-config` prints the snippet:

```json
{
  "mcpServers": {
    "mq-bridge": {
      "command": "mq-bridge-app",
      "args": ["mcp", "--transport", "stdio"]
    }
  }
}
```

Use an absolute path to the binary if it is not on `PATH`. With
`--transport http`, point your client at `http://127.0.0.1:9092/` instead.

Restart the client fully after installing — reopening a tab is not enough.

Under `stdio` the client spawns the server process, so a rebuilt binary is only
picked up after the client restarts the server — an edit to `mcp.rs` alone will
not change the behaviour of an already-running session.

## Errors

Invalid endpoints, duplicate route names, and unknown route names come back as
JSON-RPC `-32602` (invalid params); a connection failure at publish time reports
the underlying transport error:

```text
invalid publisher endpoint: IO error: Connection refused (os error 61)
```

A partially failed batch returns a result flagged `is_error` with a
`{"status": "Partial", "sent": N, "failed": M, "errors": [...]}` summary, so a
partial send is not mistaken for success.

## Known limitations

This originates in the upstream `mq-bridge` crate, not in the MCP layer.

- **Finished routes are not reaped.** A route started with `exit_on_empty` stays
  in `list_routes` after it has drained and exited; call `stop_route` to clear
  it. It is no longer indistinguishable from a running one, though: since
  mq-bridge 0.3.6 each entry carries `"finished"` plus an `"outcome"`. While the
  route runs these are `false` / `null`; once its task ends, `"finished": true`
  and `"outcome"` is `completed` (drained cleanly) or `failed` (permanent error
  — the cause is in `status.error`).

  `stop_route` removes the entry from `list_routes` as it stops the route, so
  the third outcome, `stopped`, is not observable through these tools.

## MCP Registry (`server.json`)

`mcp install` and the registry solve different problems, and neither needs the
other:

| | `mqb mcp install` | MCP Registry |
| --- | --- | --- |
| For | Someone who already has the binary | Someone who has not heard of it yet |
| Does | Writes the client config directly | Publishes discovery metadata only |
| Needs `server.json` | No | Yes |

So `install` remains the shortest path on a machine that already has the binary
— it is not superseded by publishing to the registry.

[`server.json`](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/server.json)
at the repository root is the registry entry. It publishes the server under
`io.github.marcomq/mq-bridge-app` and offers two package types, both of which
run the same `mcp --transport=stdio` command:

| `registryType` | Identifier | Requires |
| --- | --- | --- |
| `oci` | `ghcr.io/marcomq/mq-bridge-app:<version>` | Docker; no Rust toolchain |
| `cargo` | `mq-bridge-app` | `cargo install`, so a Rust toolchain |

The registry only stores metadata — it never hosts the artifact — so it verifies
that each package really belongs to this project by looking for an **ownership
marker** inside the published artifact itself:

- **OCI** — a `LABEL io.modelcontextprotocol.server.name` in the [`Dockerfile`](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/Dockerfile) final stage.
- **Cargo** — an `mcp-name:` line in the crate README, which crates.io renders. It must be *visible* markdown: crates.io strips HTML comments during rendering, so the hidden `<!-- mcp-name: … -->` form that works for PyPI and NuGet is silently dropped here.

Both markers must equal the `name` field in `server.json`.

Publishing happens in the `publish-mcp-registry` job of the release workflow. It
runs after the ghcr manifest and the crates.io release exist, rewrites the
versions in `server.json` from the release tag, authenticates with
`mcp-publisher login github-oidc` (the `io.github.*` namespace is proven by the
workflow's own OIDC token, so there is no secret to rotate) and publishes. To do
it by hand:

```bash
brew install mcp-publisher
mcp-publisher login github
mcp-publisher publish
```

## Testing against local brokers

The compose files in the `mq-bridge` repo bring up the brokers:

```bash
cd ../mq-bridge/tests/integration/docker-compose
docker compose -f nats.yml -f postgres.yml -f redis.yml up -d
```

Verified end-to-end against these: batch publish to file/NATS/Redis Streams,
`NATS → file` and `Redis Streams → Postgres` routes (including
`auto_create_table`), the full route lifecycle, and the error paths above.
