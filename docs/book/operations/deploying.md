# Deploying

The CLI / server form is what you deploy — a single headless binary that runs a long-lived
bridge in [config mode](../getting-started/run-forms.md#cli--server). This page covers running it
as a container and as a service, and the config/secrets patterns that suit each.

## Docker

The CLI is published as a multi-arch image (`amd64` + `arm64`):

```bash
docker run --rm --name mq-bridge -p 9091:9091 ghcr.io/marcomq/mq-bridge-app:latest
```

Mount the working directory at `/app` and seed the config on first run from one of the
templates baked into the image at `/config`:

```bash
touch input.log
docker run --rm --name mq-bridge -p 9091:9091 -v "$(pwd)":/app \
  ghcr.io/marcomq/mq-bridge-app:latest --ui --init-config=/config/file-to-http.yml
```

- The default `latest` image is a plain multi-arch image for `amd64` and `arm64`.
- **IBM MQ** support is published separately as the `latest-ibm-mq` / `ibm-mq` tags, `amd64`
  only (no redistributable arm64 client yet). Start it with `--platform=linux/amd64`, or build
  yourself with `cargo build --release --features=ibm-mq`.

### Ports in containers

On a host, the UI is [never opened implicitly](../reference/cli.md#starting-the-web-ui) and
metrics bind loopback. In a container those defaults would be wrong for the opposite reason —
nothing is reachable until you publish it — so the image's `CMD` is `--ui`, the UI binds
`0.0.0.0:9091` through `ENV MQB_UI_DEFAULT_ADDR`, and metrics bind
`0.0.0.0:9090` through `ENV MQB__METRICS_ADDR` rather than the command line: a Kubernetes pod
that sets `args:` replaces `CMD` wholesale, and the environment survives that. The container
boundary is the gate: without `-p` (or a Kubernetes Service), neither port leaves the container.

Docker replaces `CMD` **wholesale** as soon as you pass any argument of your own. That is what
keeps the headless modes headless:

The two settings are carried differently, and that difference is the whole design:

| | Carried by | Survives an `args:` / command override? |
|---|---|---|
| Metrics on `0.0.0.0:9090` | `ENV MQB__METRICS_ADDR` | **Yes** |
| Web UI | `CMD ["--ui"]` | No |
| UI address `0.0.0.0:9091`, once the UI is asked for | `ENV MQB_UI_DEFAULT_ADDR` | **Yes** |

Metrics live in `ENV` because a Kubernetes pod almost always sets `args:`, which replaces
`CMD` wholesale. Had the bind address ridden along in `CMD`, every such pod would silently
fall back to the host default of `127.0.0.1:9090` and go unscrapeable. As an environment
variable it survives any command-line override, and a pod that wants something else just
sets `MQB__METRICS_ADDR` (or `metrics_addr` in its ConfigMap).

The UI stays in `CMD` precisely *because* it is dropped on override — that is what keeps the
headless modes headless:

| Invocation | Result |
|---|---|
| `docker run image` | Config mode, UI + metrics served |
| `docker run image copy …` / `mcp …` | `CMD` dropped — headless, as those modes always are |
| `docker run image --config /app/x.yml` | `CMD` dropped — **add `--ui` if you want the UI** |
| Kubernetes with `args: […]` | No UI; metrics still served |
| Kubernetes with no `args` | UI + metrics served, reachable only via a Service |

The second `docker run` example above passes `--init-config`, which is why it also passes
`--ui`.

To build the image from source, see
[`BUILD.md`](https://github.com/marcomq/mq-bridge/blob/main/docs/book/BUILD.md).

## Configuration in containers / Kubernetes

Configuration is **hierarchical** — files plus environment variables — which is exactly what
container and Kubernetes deployments want:

- Bake a base `config.yml` into the image or mount it as a ConfigMap.
- Override any field per environment with `MQB__{ROUTE}__{PATH}` env vars (double underscores
  between segments), e.g. `MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC=my-topic`.
- Reference secrets inline with `${ENV_VARIABLE_NAME:-default}`; a `.env` file in the working
  directory is auto-loaded for local development.

See [Configuration grammar](../engine/configuration.md) for the full env-var mapping and
[Secrets & interpolation](../cookbook/secrets.md) for keeping credentials out of committed
config.

## Choosing the run shape

| You want… | Run it as… |
|---|---|
| A one-shot batch move (finite source), exit 0 on success | `copy … --drain` (see [Quick start](../quick-start.md)) |
| A long-lived bridge with one or more routes | config mode: `mqb --config config.yml` |
| The bridge driven by an LLM agent | [`mcp` mode](../MCP.md) |

In config mode the CLI can also serve the browser UI on the configured port, but never
implicitly: it needs `ui_addr` in the config or an explicit `--ui`. An unattended start — a
service unit, a container, a script — is headless unless you asked for the UI, so production
deployments opt in rather than opt out. Where you do serve it, front it appropriately.
See [Starting the web UI](../reference/cli.md#starting-the-web-ui).

## Security checklist for production

- **TLS on every sensitive endpoint** (`tls.required: true` + `ca_file`, mTLS where supported).
  Never set `accept_invalid_certs: true`. Pick the crypto provider feature (`rustls-aws-lc` for
  FIPS-capable / post-quantum, or `rustls-ring`).
- **Limit an exposed HTTP input**: set `header_read_timeout_ms` (for example `30000`) so a
  half-sent request does not hold a connection, and `max_body_bytes` to the largest body you
  expect. Both are off or wide by default; the timeout costs some throughput.
- **Keep payloads out of logs**: run above `trace` level (payloads log at `trace`).
- **Do not commit secrets**: source them from a secrets manager or env vars.
- Consider the config **security modes** (plain, extracted secrets, encrypted config, encrypted
  history) based on the runtime target and available key storage.

Full hardening notes (including the PCI-DSS-oriented checklist) are in the
[TLS & security hardening](../engine/configuration.md#tls--security-hardening) section.

## Observability

Wire up the [`metrics`](observability.md) middleware and scrape the Prometheus endpoint; ship
the JSON logs to your aggregator. See [Observability & metrics](observability.md).

## Continuous deployment of this book

The book is published to GitHub Pages by
[`.github/workflows/docs.yml`](https://github.com/marcomq/mq-bridge/blob/main/.github/workflows/docs.yml)
on every push to `main` that touches the root engine docs or
`docs/book/**`. It runs the local
`docs/book/sync-engine-docs.sh` before building the book. To
build it locally, run the same commands from the repository root. See
[the book's README](https://github.com/marcomq/mq-bridge/blob/main/docs/book/README.md).
