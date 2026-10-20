---
name: mq-bridge
description: Move, copy, stream or replicate data between brokers (Kafka, NATS, RabbitMQ, MQTT, Redis, SQS, IBM MQ), databases (Postgres, MySQL, SQLite, MongoDB, ClickHouse, incl. CDC), files, object storage (S3, GCS, Azure as Parquet/JSONL) and HTTP with the mq-bridge CLI (`mqb`, also installed as `mq-bridge-app`). Use when the user mentions mq-bridge or mqb, or asks for a one-off copy, an ETL/CDC job, a bridge between two systems, or a route config, and mqb is installed or acceptable to install. Not for writing Rust code against the mq-bridge library.
---

# mq-bridge CLI

`mqb` and `mq-bridge-app` are the same binary. Check it is there with `mqb --version`; if not,
install with `brew install marcomq/tap/mq-bridge-app`, `cargo install mq-bridge-app`, or run
`docker run --rm ghcr.io/marcomq/mq-bridge-app:latest <args>`. Ask before installing.

Never guess an endpoint parameter. The binary documents itself, and what it prints is
authoritative for the installed version:

```bash
mqb list                 # every endpoint scheme and middleware
mqb describe postgres    # URI form, example, every param with type and default
mqb describe retry       # same for a middleware
mqb describe kafka --json
```

## Pick the form

| Task | Use |
|---|---|
| One source → one sink, ad hoc or scripted | `mqb copy SOURCE TARGET` |
| Several routes, fanout/switch, long-running service | YAML config: `mqb --config routes.yml` |
| mq-bridge MCP tools are available in this session | `start_route` / `wait_route` / `publish` / `describe` |

Start with `copy`. Move to a config file only when one URI per side can't express it.

## copy

```bash
mqb copy 'SCHEME://host/...?param=value|middleware?param=value' 'SCHEME://...' [flags]
```

- The scheme picks the endpoint, query params set its config (`mqb describe <scheme>`).
- Append middlewares with `|`, applied in order: `...?topic=orders|retry?max_attempts=5`.
  A literal `|` inside a URI is `%7C`. Quote every URI in the shell.
- `--drain` exits when the source is empty: use it for files and table reads. Without it
  `copy` runs until Ctrl-C, which is right for brokers and CDC.
- `--wait SECS` drains but waits up to SECS for the first message (source still filling).
- `--filter 'amount > 100 && country == "DE"'` keeps matching top-level JSON fields.
- `--limit N` stops after N delivered rows: use it for a preview before a full run.
- `--resume` continues from the source's stored position and refuses to start when that
  is unsafe; `mqb checkpoint show|reset|set` inspects or edits that position.
- `--concurrency`, `--batch-size` tune throughput; `-v` logs what happens.

Preview first, then run:

```bash
mqb copy "postgres://app:$PGPASSWORD@localhost/shop?table=orders" \
  'file:///dev/stdout?format=payload' --drain --limit 5
mqb copy "postgres://app:$PGPASSWORD@localhost/shop?table=orders" \
  's3://bucket/orders/?format=parquet' --drain
```

The run prints a `copied N of M rows ...` summary on stdout; report it to the user.

## Config file

Write the config in YAML with one entry per route under `routes:`:

```yaml
routes:
  orders_to_kafka:
    input:
      sqlx: { url: "postgres://app:${PGPASSWORD}@localhost/shop", table: orders }
    output:
      middlewares:
        - retry: { max_attempts: 5 }
      kafka: { url: "localhost:9092", topic: orders }
```

- The endpoint key is the `endpoint type` that `mqb describe` prints (`sqlx` for postgres,
  mysql and sqlite), not the URI scheme; any other key is parsed as a custom endpoint.
- Field names are the params `mqb describe` lists. `${VAR}` and `${VAR:-default}` read the
  environment; keep credentials there, never inline.
- Schema for validation and editor completion: `mqb --schema mqb.schema.json` (`-` prints it).
  Add `# yaml-language-server: $schema=./mqb.schema.json` as the first line of the YAML.
- Run with `mqb --config routes.yml --no-ui`. `mqb --config routes.yml to-cli` prints the
  config as one self-contained command with credentials replaced by env placeholders.
- `mqb status` shows what every mq-bridge process on this machine is running.

## Traps

- `file` and object-store sinks default to `format=envelope`: each line is mq-bridge's message
  envelope (`{"message_id", "payload", "metadata"}`), and so is `-` for stdout. For plain JSON
  lines, read and write with `format=payload`.
- A drain that starts before its producer sees an empty source and exits with nothing
  copied: use `--wait`.
- Broker sources may start at the newest message. Check `mqb describe <scheme>` for an
  offset or initial-position param before expecting a topic's backlog.
- Never put the web UI on the network: pass `--no-ui` in scripts; `--ui` binds loopback only.

## Further reading

- Every endpoint and middleware, with examples: https://marcomq.github.io/mq-bridge/
- Machine-readable index: https://raw.githubusercontent.com/marcomq/mq-bridge/main/llms.txt
