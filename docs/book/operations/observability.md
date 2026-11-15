# Observability & metrics

`mq-bridge-app` is production-ready with optional structured **JSON logging** and a **Prometheus** metrics
endpoint, plus per-route status you can query at runtime.

## Metrics

Attach the [`metrics`](../engine/reference.md#metrics) middleware to an endpoint to emit
throughput, latency, and error metrics. It requires the `metrics` feature and takes no options —
its presence enables collection. Input and output are labelled separately, so attaching it to
both sides is meaningful:

```yaml
orders_bridge:
  input:
    middlewares: [ { metrics: {} } ]
    kafka: { topic: "orders", url: "localhost:9092" }
  output:
    middlewares: [ { metrics: {} } ]
    nats: { subject: "orders.processed", stream: "orders_stream", url: "nats://localhost:4222" }
```

`mq-bridge-app` serves them for a Prometheus scrape at `/metrics` on `metrics_addr` (the
container image sets `0.0.0.0:9090`, see [Deploying](deploying.md)). Point your Prometheus (or
Grafana Agent) there and build dashboards on the emitted throughput/latency/error series.

> ⚠️ **Metrics are not free.** The `metrics` middleware records a measurement per message on
> whichever side it is attached, so it adds per-message overhead and can measurably reduce
> throughput — most noticeably on **high-throughput endpoints** where per-message cost dominates.
> Enable it where you need visibility, not blanket-on every side of every route: attach it to the
> one endpoint you actually want to watch, and leave it off the hot path when chasing peak
> throughput. Benchmark numbers should be taken **without** it attached (see
> [Reading throughput honestly](#reading-throughput-honestly)).

## Tracing (OpenTelemetry)

Attach the [`otel`](../engine/reference.md#otel) middleware to get one span per message. The
input span continues the incoming W3C `traceparent`, and the output writes a new
`traceparent` into the published message, so a trace runs through the bridge into the next
service:

```yaml
orders_bridge:
  input:
    middlewares: [ { otel: {} } ]
    kafka: { topic: "orders", url: "localhost:9092" }
  output:
    middlewares: [ { otel: {} } ]
    nats: { subject: "orders.processed", url: "nats://localhost:4222" }
```

`mq-bridge-app` exports the spans over OTLP/HTTP when `OTEL_EXPORTER_OTLP_ENDPOINT` (or
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is set; `OTEL_SERVICE_NAME` defaults to
`mq-bridge-app`. When neither endpoint variable is set, no exporter is installed and the
middleware is left out of the chain, so it costs nothing per message.

## Logging

Logs are plain text by default. Set `logger: json` in the config (or `MQB__LOGGER=json`) for
structured JSON, suited to shipping into a log aggregator. Three things to keep in mind:

- **Message payloads are emitted only at `trace` level.** Run production above `trace` so no
  sensitive data (e.g. personal data) reaches logs or traces. See the
  [TLS & security hardening](../engine/configuration.md#tls--security-hardening) notes.
- The MCP server's `stdio` transport owns **stdout** for the protocol, so its logs go to
  **stderr**. In HTTP transport this is not a concern.
- **At startup each route reports its inferred delivery guarantee** — `effectively-once` or
  `at-least-once`. It is read off the endpoint configuration, not enforced: the line tells you
  whether the source's identity and the sink's write add up to an idempotent pipeline. See
  [Delivery guarantees](../engine/delivery.md).

## Runtime route status

For a running bridge, query route health rather than reading logs. Every `mqb` process of your
user (a `copy`, a headless config run, the web UI, the desktop app, an MCP server) reports its
routes to a local status registry, and three views read it:

- **`mqb status`** prints one row per route and consumer on the machine: state, current and
  average rate, total, pending and uptime. It redraws every second; `--no-watch` prints once and `--json`
  prints the records.
  See [the CLI reference](../reference/cli.md#status--what-is-running-on-this-machine).
- **In the UI**, the **Status** tab shows the same rows with the last 60 seconds of each rate.
  The publisher and consumer views show health and message counts for this instance.
- **Through the MCP server**, `bridge_status` returns the same records.
- **Through the MCP server**, `list_routes` and `route_status` report `messages`,
  `messages_per_second` (instantaneous), `elapsed_s`, and `average_messages_per_second`. For a
  running route read the instantaneous rate; for a finished job read the average. See
  [MCP route status](../MCP.md#route-status).

A process drops out of these views within 7 seconds of exiting. Records carry endpoint types,
not URLs or credentials, and an error appears only as the word `error`. To see the error text,
set `MQB_STATUS_SHOW_ERRORS=1` for both the process that runs the route and the one that reads
the status; error text can contain connection details, so leave it off on shared machines.

## Reading throughput honestly

Whatever the source, a rate figure is only meaningful with its methodology:

- Measure on a **release build** — a debug build reports dramatically slower rates. Through MCP,
  call `server_info` first to confirm the build `profile`.
- The instantaneous rate of a **completed** job decays to ~0 within a second — use the *average*
  for finished work.
- Record CPU/cores/RAM and the exact `batch_size` / `concurrency` next to every number.

See [Performance tuning → Measuring](tuning.md#measuring) and the like-for-like ETL/CDC
methodology in
[`benches/etl/README.md`](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/benches/etl/README.md).
