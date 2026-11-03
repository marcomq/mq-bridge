# TechEmpower FrameworkBenchmarks entries for mq-bridge

Benchmark entries shaped like the [TechEmpower FrameworkBenchmarks](https://github.com/TechEmpower/FrameworkBenchmarks)
project, for both the Rust core (`mq-bridge`) and the Python bindings
(`mq-bridge-py`).

> **Status:** TechEmpower FrameworkBenchmarks is now archived (read-only), so
> these are no longer submitted upstream — they live on as a **verified local
> conformance + throughput harness** (`verify.sh`). For maintained public
> benchmark suites, see the sibling entries under
> [`scripts/the-benchmarker/`](../the-benchmarker/) (the-benchmarker/web-frameworks)
> and [`scripts/httparena/`](../httparena/) (HttpArena).

## Scope

| Test            | Route                    | Entry        | Response |
|-----------------|--------------------------|--------------|----------|
| JSON            | `GET /json`              | Rust + Python | `{"message":"Hello, World!"}` serialized **per request** |
| Plaintext       | `GET /plaintext`         | Rust + Python | `Hello, World!` (the pipelined throughput test) |
| Single Query    | `GET /db`                | Rust only     | one random `World` row as JSON |
| Multiple Queries| `GET /queries?queries=N` | Rust only     | N random `World` rows (N clamped 1..500) |

Fortunes (HTML templating) and Data Updates are intentionally not implemented —
mq-bridge is a message-bridging library, not a full-stack web framework. The DB
tests are included for the Rust entry as a SQLx/Postgres example (see below);
they are omitted from the Python entry on purpose.

## How it works

Both entries listen on `0.0.0.0:8080`. The shared HTTP server keys on the listen
address, so multiple routes (or, for Python, the single route) share one
process/port.

- **Rust entry: two routes.** A path-filtered `GET /plaintext -> static` route
  answers the Plaintext test **handler-free**, and a catch-all `http -> response`
  route dispatches the rest (`/json`, `/db`, `/queries`) on the request's
  `http_path` metadata inside the handler. The router prefers the more specific
  (path-filtered) route, so `/plaintext` hits the static reply and everything
  else falls through to the handler.
- **Python entry: one route.** A single `http -> response` route with no path
  filter; the handler dispatches both `/json` and `/plaintext` on `http_path`.
  (The static path is intentionally not used here — see "On bypassing the
  handler" below.)

Key properties (see the inline comments in each entry):

- **Inline fast path.** Both `http -> response` and `http -> static` routes reply
  inline, bypassing the route worker/disposition pipeline. So route
  `concurrency`/`batch_size` do *not* gate the hot path; we use `concurrency: 1`.
  Per-connection parallelism comes from the Tokio runtime spawning a task per
  accepted connection.
- **Off-GIL work (Python).** All HTTP framing and JSON (de)serialization run in
  Rust off the GIL; the single Python worker thread only runs the trivial
  dispatch. This is the differentiator that should place `mq-bridge-py` near the
  top of the Python-language entries on the JSON test.
- **`concurrency_limit`.** A per-request semaphore (`acquire_owned` per request,
  default 100). We raise it to 65536 so it never throttles TechEmpower's many
  connections; the acquire cost itself is inherent to the current server.
- **Buffered pipelined writes.** The core HTTP server enables hyper's
  `pipeline_flush(true)`, which coalesces the responses of pipelined HTTP/1.1
  requests into a single buffered write — the throughput win for the Plaintext
  test, and a no-op for non-pipelined traffic.
- **Headers.** The handler's returned message metadata becomes response headers
  (`Content-Type`, `Server`); hyper adds `Date` and `Content-Length`. The status
  comes from `http_status_code` metadata (default 200).

## Database tests (Rust entry)

`/db` and `/queries` run `SELECT id, randomnumber FROM world WHERE id = $1` for a
random id (1..10000) and return the row(s) as JSON (`{"id":..,"randomNumber":..}`).

Why a handler-owned `sqlx::PgPool` rather than mq-bridge's `sqlx` endpoint: that
endpoint models a table as a **message queue** — the publisher does `INSERT`, the
consumer runs a *polling* `SELECT` whose only bind is the route `batch_size` and
emits rows as a stream of messages. It has no per-request parameter and no
request↔reply correlation, so it cannot serve TechEmpower's random-id,
reply-synchronously pattern. The handler owns a pool and runs the query directly.

The pool is optional: without a reachable `DATABASE_URL` the DB routes return 503
and `/json` + `/plaintext` still run. The Docker image points `DATABASE_URL` at
TechEmpower's `tfb-database` host.

**Not added to the Python entry on purpose.** The Python handler runs
synchronously on the single Python worker thread, so a Python DB driver there
would serialize every query through one thread and badly misrepresent the
framework. The Python entry's value is off-GIL JSON serialization, not DB I/O.

## On bypassing the handler (switch + static)

mq-bridge has a `static` endpoint (emits a fixed response body) and a `switch`
endpoint (routes on a metadata key), so `http → switch on http_path → static`
(or just `http → static`) can answer with **no handler at all**, replying inline
on the HTTP fast path.

The `static` endpoint supports the two properties this needs:

1. **`raw: true`** sends the body verbatim (so `Hello, World!` is not JSON-quoted), and
2. **`metadata:`** entries are attached to the response; for an HTTP response
   they become headers — set `content-type` (and optionally `server`) here so the
   reply doesn't default to `application/octet-stream`.

```yaml
plaintext:
  input:  { http:   { url: "0.0.0.0:8080", path: "/plaintext" } }
  output:
    static:
      body: "Hello, World!"
      raw: true
      metadata:
        content-type: "text/plain"
```

For **plaintext** this is the only test where a static (handler-free) reply is
allowed by the rules, and it shaves the per-request handler hop. **The Rust entry
uses it** (the `GET /plaintext -> static` route above). The **Python entry does
not**: there the plaintext reply stays on the trivial handler. Routing Python
plaintext through `static` would mean running a second handler-free route
alongside the JSON handler route — and since the Python `Route.run()` owns its
own runtime, that means two runtimes against the one process-global HTTP server.
It's viable but unverified, and the Python entry's headline result is off-GIL
JSON throughput, not plaintext — so it's kept simple on purpose.

For **JSON** the static path is *not* an option for either entry: the rules
require per-request serialization, not a pre-rendered string — the handler is the
conformant choice there.

## Layout

```text
scripts/techempower/
  verify.sh                       # local conformance + optional wrk read
  postgres.yml                    # local Postgres for the /db tests (not submitted)
  seed.sql                        # World table seed (10,000 rows)
  Rust/mq-bridge/                 # Rust entry (cargo binary)
    README.md                     # framework README submitted upstream
    src/main.rs
    Cargo.toml                    # path-dep on this repo; swap to git tag for the PR
    benchmark_config.json
    mq-bridge.dockerfile
  Python/mq-bridge-py/            # Python entry
    README.md                     # framework README submitted upstream
    server.py
    benchmark_config.json
    mq-bridge-py.dockerfile
```

This top-level README is the developer overview and is **not** submitted
upstream. The per-framework `README.md` files inside `Rust/mq-bridge/` and
`Python/mq-bridge-py/` are the ones TechEmpower expects (one per entry) and get
copied with their directories.

## Verify locally

```bash
# Rust: builds and checks /json + /plaintext conform, then a quick wrk read if present
scripts/techempower/verify.sh rust

# Rust including the DB tests: bring up local Postgres, then point DATABASE_URL at it
docker compose -f scripts/techempower/postgres.yml up -d
DATABASE_URL="postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:5433/hello_world" \
  scripts/techempower/verify.sh rust
docker compose -f scripts/techempower/postgres.yml down -v

# Python: requires the wheel installed in the active venv
#   (cd bindings/python && uv run maturin develop --release \
#       --no-default-features -F http -F pyo3/extension-module)
scripts/techempower/verify.sh python
```

For a head-to-head throughput comparison against FastAPI/Sanic/etc., the
existing wrk harness still applies:

```bash
cd bindings/python && uv run python analysis/bench_http_native.py
```

## Historical note (upstream is archived)

TechEmpower FrameworkBenchmarks was archived after Round 23, so there is no live
PR target anymore. The entries kept their submission shape (per-framework
`README.md`, `benchmark_config.json`, Dockerfile) so they remain a faithful,
runnable reference — and so they can still be dropped into a fork of the archived
repo to reproduce a round locally with `./tfb --mode verify --test mq-bridge
mq-bridge-py`. The Dockerfiles `git clone` this repo at `MQB_REF` (pin to a
released tag) and the Rust `Cargo.toml` path dependency can be swapped for a
`git`/crates.io release for reproducible image builds.

For maintained public suites, the equivalent (and PR-able) entries now live
under [`scripts/the-benchmarker/`](../the-benchmarker/) and
[`scripts/httparena/`](../httparena/).

> Honest framing: `mq-bridge-py` is the interesting entry (top-tier Python JSON
> throughput via off-GIL serialization). The Rust entry is a mid-pack
> hyper-based result — it optimizes for bridging/routing, not raw req/s.
