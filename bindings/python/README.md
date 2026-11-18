<p align="center">
  <img src="https://raw.githubusercontent.com/marcomq/mq-bridge/main/apps/mq-bridge-app/crates/desktop/icons/icon.png" alt="mq-bridge" width="112" height="112">
</p>
<h1 align="center">mq-bridge for Python</h1>

<p align="center">
<strong>Move and reshape data between Kafka, NATS, RabbitMQ, MQTT, MongoDB, SQL, HTTP, files and more, from plain synchronous Python. The I/O, batching and JSON work run in a Rust engine, outside the GIL.</strong>
</p>

<p align="center">
<a href="https://pypi.org/project/mq-bridge/"><img src="https://img.shields.io/pypi/v/mq-bridge.svg" alt="PyPI"></a>
<a href="https://pypi.org/project/mq-bridge/"><img src="https://img.shields.io/pypi/pyversions/mq-bridge.svg" alt="Python versions"></a>
<a href="https://marcomq.github.io/mq-bridge/"><img src="https://img.shields.io/badge/docs-book-orange" alt="Book"></a>
<a href="https://www.http-arena.com/#sort=rps:-1&q=python"><img src="https://img.shields.io/badge/http--arena-listed-blue" alt="http-arena"></a>
<a href="https://github.com/marcomq/mq-bridge#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg" alt="License"></a>
</p>

<p align="center">
📚 <a href="https://marcomq.github.io/mq-bridge/">Book</a> ·
📖 <a href="https://github.com/marcomq/mq-bridge/blob/main/docs/REFERENCE.md">Endpoint &amp; middleware reference</a> ·
⚙️ <a href="https://github.com/marcomq/mq-bridge/blob/main/docs/CONFIGURATION.md">Config</a> ·
📊 <a href="https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/benches/etl/README.md">Benchmarks</a> ·
🦀 <a href="https://github.com/marcomq/mq-bridge">Rust engine</a>
</p>

---

## Why mq-bridge?

- **One API for many transports.** Kafka, NATS, AMQP, MQTT, MongoDB, PostgreSQL/MySQL/SQLite, ClickHouse, Redis Streams, HTTP, WebSocket, gRPC, ZeroMQ, AWS SQS/SNS, IBM MQ, object storage, files and in-memory channels share one config shape. To switch the broker, you change the config, not your handler.
- **Rust does the heavy lifting.** Broker I/O, batching, ack/nack, commit ordering, retries and JSON (de)serialization run in the Tokio-based Rust core. Python only runs your handler. From plain synchronous code, one `send_batch()` call sends ~8x the HTTP requests `aiohttp` manages, and a route serves HTTP at ~2x the fastest asyncio framework (see [Performance](#performance)).
- **Pipelines from configuration.** Transform, filter, switch, fan out, enrich, deduplicate, rate-limit, retry or send to a DLQ with a few lines of YAML, with no Python code in the hot path.
- **Plain, synchronous Python.** You don't need `asyncio` or callbacks. `send_json()` blocks until the broker acks, a handler is an ordinary function, and `Consumer.poll()` hands you a list.
- **The same config runs everywhere.** You can prototype a route in the [`mqb` CLI or desktop app](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) and then load the identical YAML in Python, Rust or Node.js.

## Install

```bash
pip install mq-bridge
```

Both packages install the same import path, `mq_bridge`:

| Package | Install | Includes |
| :--- | :--- | :--- |
| Default | `pip install mq-bridge` | Full set on glibc-Linux/macOS/Windows-x64 (Kafka, AWS, gRPC, MongoDB, SQLx + basic); reduced set automatically on musl/Alpine and Windows arm64 (no Kafka/SQLx/gRPC) |
| Basic | `pip install mq-bridge-basic` | HTTP, NATS, MQTT, AMQP, WebSocket, ZeroMQ, MongoDB, AWS — the lean set on **every** platform |

pip picks the right wheel for your platform, so you don't need a marker or a manual choice. Kafka, SQLx, gRPC and static IBM MQ don't build on musl/Alpine and Windows arm64; calling them there raises a clear runtime error, and everything else works the same. Install `mq-bridge-basic` only if you want the lean build on a full-support system too. Memory and file endpoints are always present.

On conda, the package is `mq-bridge-py` on conda-forge
([feedstock](https://github.com/conda-forge/mq-bridge-feedstock)), with the same import path:

```bash
conda install -c conda-forge mq-bridge-py   # or: pixi add mq-bridge-py
```

## Quick start

### Publish a message, with no route and no config file

`Publisher.from_config` takes a plain dict, and `send_json` blocks until the broker acks:

```python
from mq_bridge import Publisher

pub = Publisher.from_config({"kafka": {"brokers": "localhost:9092", "topic": "orders"}})
for i in range(5):
    pub.send_json({"order_id": i, "amount": i * 10})
```

Swap the dict for any other transport (`nats`, `amqp`, `mqtt`, `mongodb`, `http`, `memory`, `file`, ...). `send_json` takes an optional `metadata` dict as its second argument (e.g. `{"kind": "order.created"}`). `send_batch()` sends many messages in one call. `Publisher` has no `close()` or context-manager form, so for short scripts let the script exit once the sends finish.

### Serve HTTP with a Python handler

```python
from mq_bridge import Route

config = {
    "input": {"http": {"url": "127.0.0.1:8080", "path": "/score", "method": "POST"}},
    "output": {"response": {}},
}

def score(message):
    data = message.json()                 # parsed in Rust
    return {"id": data["id"], "score": len(data["text"])}

Route.from_config(config, "score").with_handler(score).run()   # blocks
```

```bash
curl -s -X POST localhost:8080/score -d '{"id": 1, "text": "hello"}'
```

### Consume from a broker

```python
from mq_bridge import Consumer

with Consumer.from_config({"nats": {"subject": "orders", "stream": "orders", "url": "nats://localhost:4222"}}) as consumer:
    while True:
        batch = consumer.poll(max=500, timeout_ms=1000)
        for message in batch:
            print(message.json())
        consumer.commit()                 # ack only after handling
```

If parsing dominates your handler, `orjson.loads(message.payload)` decodes ~1.5x faster than `message.json()` (1M 106-byte records: 2.5M/s vs 1.7M/s). End to end on a Kafka topic the gain was 0–10 %, since at ~900k records/s the parse is no longer the bottleneck.

## Performance

The Python package is a thin layer over the same engine as the Rust crate and the `mqb` CLI. Python touches only your handler; connections, batching, concurrency and JSON stay in Rust, outside the GIL.

All figures below were measured on an Apple M1 (8 cores), CPython 3.14.7, mq-bridge 1.5.0 from PyPI, one Python process per contender. The scripts are under [Benchmarks](#benchmarks).

**Sending HTTP requests** (200-byte JSON POST to a local nginx, so the target is never the limit):

| Client | 1 in flight | 16 in flight | 64 in flight |
| :--- | ---: | ---: | ---: |
| `requests` (one `Session` per thread) | 2,151 req/s | 2,850 req/s | 2,969 req/s |
| `httpx.Client` (threads) | 3,422 req/s | 2,752 req/s | 2,896 req/s |
| `httpx.AsyncClient` (uvloop) | 1,946 req/s | 630 req/s | 154 req/s |
| `aiohttp` (uvloop) | 14,062 req/s | 20,718 req/s | 20,560 req/s |
| mq-bridge `Publisher.send()` (threads) | **21,130 req/s** | **88,055 req/s** | **82,388 req/s** |
| mq-bridge `Publisher.send_batch()` (one thread) | **23,981 req/s** | **132,527 req/s** | **160,955 req/s** |

One `send_batch()` call from plain synchronous code sends ~8x what `aiohttp` reaches with an event loop, and ~50x `requests`. You don't need `asyncio` for it: `batch_concurrency` in the endpoint config sets how many requests are in flight at once.

**Serving HTTP** (handler: parse JSON, add 1, return JSON; driven by `wrk` so the load generator isn't the bottleneck):

| Framework | 1 connection | 8 connections | 32 connections |
| :--- | ---: | ---: | ---: |
| FastAPI (uvicorn) | 13,555 req/s | 18,100 req/s | 19,017 req/s |
| Starlette (uvicorn) | 19,952 req/s | 30,553 req/s | 30,979 req/s |
| aiohttp | 15,339 req/s | 28,726 req/s | 31,225 req/s |
| Sanic | 23,676 req/s | 40,893 req/s | 42,263 req/s |
| FastStream (ASGI, uvicorn) | 23,814 req/s | 42,989 req/s | 44,308 req/s |
| mq-bridge `Route` + handler | 21,041 req/s | **84,078 req/s** | **109,606 req/s** |

Under load, mq-bridge serves ~2–2.5x the fastest asyncio framework and ~5x FastAPI. The application logic is Python in every case; mq-bridge parses and serializes the JSON in Rust, outside the GIL. At a single connection the asyncio frameworks are on par.

**Consuming Kafka into dicts** (1M JSON records of ~106 B on 4 partitions, local broker, every record parsed into a `dict`; mq-bridge 1.6.1, median of 3):

| Consumer | records/s |
| :--- | ---: |
| `aiokafka` `getmany()` + `json.loads` (uvloop) | 293,000 |
| `confluent-kafka` `consume(1000)` + `json.loads` | 469,000 |
| mq-bridge `Consumer.poll(1000)` + `message.json()` | **845,000** |
| mq-bridge `Route` + batch handler + `message.json()` | **857,000** |

~1.8x `confluent-kafka` and ~2.9x `aiokafka`, with no tuning. The `confluent-kafka` row already sets `fetch.queue.backoff.ms=10`; with librdkafka's default it stalls at 159,000 records/s. Swapping `json.loads` for `orjson.loads` brings `confluent-kafka` level with mq-bridge.

**Loading JSONL into Postgres** (1M rows, 6 columns; mq-bridge 1.6.1, median of 3):

| Loader | rows/s |
| :--- | ---: |
| psycopg 3 `executemany` (pipelined) | 64,000 |
| asyncpg `copy_records_to_table` | 246,000 |
| psycopg 3 `COPY` + `json.loads` | 263,000 |
| psycopg 3 `COPY` + `orjson.loads` | 349,000 |
| mq-bridge `Route` file → sqlx, `bulk_copy`, `batch_size: 5000`, `concurrency: 4` | **696,000** |

One process and one config line give ~2x the fastest single-connection Python loader: the route keeps 4 `COPY` statements in flight from Rust while Python stays idle. Splitting the file over 4 processes by hand gets psycopg into the same range (520,000 rows/s in a separate run where the route reached 657,000). Batching and concurrency are explained under [Fast bulk loads](https://github.com/marcomq/mq-bridge/blob/main/docs/book/connectors/postgres.md#fast-bulk-loads).

```python
Route.from_config({
    "input": {"file": {"path": "readings.jsonl", "format": "payload"}},
    "output": {"sqlx": {"url": PG_URL, "table": "readings", "columns": "auto", "bulk_copy": True}},
    "batch_size": 5000,
    "concurrency": 4,
    "exit_on_empty": True,
}).run()
```

**Data movement** (engine figures, measured through the `mqb` CLI on the same machine as each baseline; a Python route that stays in config runs the same engine code):

| Job | mq-bridge | Baseline |
| :--- | ---: | :--- |
| Postgres → JSONL, 100k rows | **197,628 rows/s** | Meltano `tap-postgres` → `target-jsonl`: 16,330 rows/s (**~12x**) |
| CSV → JSONL, 1M mixed-type rows | **3,134,796 rows/s**, ~29 MiB RAM | Meltano `tap-csv` → `target-jsonl`: 9,771 rows/s, ~444 MiB RAM (**~321x**) |
| CSV → JSONL, 1M rows, typing `transform` | **1,594,896 rows/s** | Redpanda Connect, Bloblang `mapping`: 81,893 rows/s (**~19.5x**) |
| Kafka → file, 1M rows | **878,105 rows/s** | Sea Streamer: 492,465 rows/s (**~1.8x**) |

Every baseline, command and caveat is in the [ETL benchmark harness](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/benches/etl/README.md). mq-bridge's HTTP path is also listed on the third-party [http-arena.com](https://www.http-arena.com/#sort=rps:-1&q=python) leaderboard, which updates live, so rankings shift over time.

## API at a glance

| Class | Use it to | Build with |
| :--- | :--- | :--- |
| `Publisher` | send messages (`send`, `send_json`, `send_batch`) or make request/reply calls (`request`, `request_json`) | `from_config` / `from_str` / `from_file` |
| `Route` | run an input → output pipeline, optionally with a Python handler (`with_handler`, `with_batch_handler`, `add_handler(kind, ...)`) | same |
| `Caller` | call a route like a function and get its answer | same, plus `from_route` |
| `Consumer` | pull batches on your own schedule and ack them yourself | same |
| `Message` | payload + metadata, with lazy `json()` / `text()` readers and `with_json()` / `with_payload()` response helpers | `Message(...)`, `Message.from_json(...)` |
| `RetryableError` / `NonRetryableError` | tell the engine whether a failed message should be retried | raise from a handler |

The three constructors differ only by source: `from_file` (path to YAML/JSON), `from_str` (YAML/JSON string), `from_config` (Python `dict`). The `name` argument is optional: pass it to pick one entry out of a `routes:` / `publishers:` / `consumers:` document, or omit it to treat the whole config as a single bare route or endpoint. `Publisher.send_json()` and `request_json()` serialize the Python value in Rust. `from_yaml` / `from_yaml_str` remain as deprecated aliases for `from_file` / `from_str`.

The Python surface is synchronous and blocking. Tokio, broker I/O, routing and batching all stay in Rust.

## Running a route

`Route.run()` **blocks the calling thread** until another thread calls `stop()`: it deploys the route and then parks. That suits a process whose only job is the route, but nothing after `route.run()` executes until the route stops.

To keep running Python code after the route is up, use `start()` (non-blocking) or the context-manager form:

```python
route = Route.from_config(config, "orders_route").with_handler(handle)

# Non-blocking: deploys, returns, and runs on a background thread.
route.start()
publisher.send_json({"order_id": 42}, {"kind": "order.created"})
route.stop()
route.join()   # optional: wait for a clean shutdown

# Or scope it to a block — starts on enter, stops + joins on exit:
with Route.from_config(config, "orders_route").with_handler(handle):
    publisher.send_json({"order_id": 42}, {"kind": "order.created"})
```

Configuration and connection errors surface from `start()` itself, not from a background thread.

Handlers come in three forms:

- `with_handler(fn)` gets one `Message` and returns a result (a `Message`, `bytes`, `str`, a JSON value, or `None`).
- `with_batch_handler(fn)` gets the whole batch and returns one result per message, in order. Use it when the work is cheaper per batch, e.g. one database transaction for all messages.
- `add_handler(kind, fn)` dispatches on the message's `kind` metadata and passes decoded JSON.

## Calling a route like a function

`Caller` deploys a route whose input is an in-process `memory` endpoint and returns each call's answer, like a function call. Calls made at the same time from several threads reach the route as one batch, so batched lookups and the `parallel` middleware still apply.

```python
from mq_bridge import Caller

# `users` in routes.yaml: an `input` with only a `lookup` middleware, no `output`.
with Caller.from_file("routes.yaml", "users") as users:
    user = users.call(path="/u/7")                   # the parsed JSON answer
    found = users.call_many([{"path": "/u/7"}, {"path": "/u/8"}])
```

`call()` sends its keyword arguments (or one mapping) as a JSON object and returns the parsed answer; `call_many()` does that for several and returns one answer or `RuntimeError` per request. `call_message()` / `call_many_messages()` take and return `Message`s, e.g. to keep metadata. `Caller.from_file` / `from_config` take the same shapes as `Route`, but a route without `input` reads from the caller and one without `output` answers with the message. `Caller.from_route(route)` deploys a route built in code, with its handler.

A message the route fails raises `RuntimeError` with the error text at once. `send()` returns `None` when the route accepts a message without an answer; `call()` raises then, so leave out the output, end it with `response: {}`, or use an endpoint that answers.

## Pull-based consumer

`Route` is push-based: you attach a handler and the route drives it. When you instead want to **pull** messages on your own schedule, e.g. to feed a generator-style sink such as a [`dlt`](https://dlthub.com) resource, use `Consumer`. It wraps any input endpoint and hands batches back to Python:

```python
from mq_bridge import Consumer

consumer = Consumer.from_config({"nats": {"subject": "orders", "stream": "orders", "url": "nats://localhost:4222"}})

while not consumer.exhausted:
    batch = consumer.poll(max=500, timeout_ms=1000)   # [] on timeout
    if not batch:
        continue
    for message in batch:
        handle(message.json())
    consumer.commit()                                 # ack only after handling
```

`poll()` receives up to `max` messages **without** acknowledging them; `commit()` acks every batch returned since the last commit, advancing the consumer offset (or removing them from the queue). Committing only after the downstream write succeeds gives at-least-once delivery: a crash before `commit()` re-delivers the batch. `poll()` returns `[]` once `timeout_ms` elapses with nothing received (omit it to block until a message arrives), and sets `exhausted` once a bounded source (e.g. a file) is fully drained — streaming brokers never set it.

> [!IMPORTANT]
> **You must call `commit()`.** It is the only thing that tells the broker a batch is done. If you keep polling without committing:
> - the consumer offset never advances, so every message is **re-delivered** on the next run (and you reprocess from the start);
> - most brokers stop sending once their unacknowledged/prefetch window fills, so `poll()` eventually **stalls** and returns nothing;
> - the uncommitted batches are held in memory pending their ack, so the process **grows unbounded**.
>
> Commit after each batch you have durably handled (as in the loops above). `commit()` also acks every earlier uncommitted batch, so if a batch fails downstream, don't poll on and commit later batches past it: retry that batch until it succeeds and then commit, or stop the consumer without committing (`close()`) so a new one gets it redelivered. To release only the failed batch, use the per-batch tokens below.

### Per-batch tokens: `poll_batch` / `ack` / `nack`

When you need to ack or release **specific** batches (rather than everything since the last commit), use the token form. `poll_batch(max, timeout_ms)` returns `(messages, token)`; `ack(token)` commits just that batch, and `nack(token)` releases it for redelivery (`nack()` with no argument nacks every outstanding batch). This is the shape a [`dlt`](https://dlthub.com) resource wants — poll → yield records → load package commits → `ack(token)` — and is demonstrated in [`examples/dlt_source.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/dlt_source.py) with the wiring brief in [`examples/OMNILOAD_INTEGRATION.md`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/OMNILOAD_INTEGRATION.md).

```python
messages, token = consumer.poll_batch(max=500, timeout_ms=1000)  # ([], None) on timeout
if token is not None:                  # nothing returned on an idle timeout
    # ... persist the batch downstream ...
    consumer.ack(token)                # or consumer.nack(token) to redeliver
```

Tokens stay outstanding until acked/nacked; `commit()` still acks every outstanding batch at once, so don't mix the two styles on one consumer. On cumulative-ack transports (Kafka), acking a later batch would implicitly ack the earlier ones, so `ack(token)` must follow receive order — acking out of order raises; ack the oldest outstanding batch first, or use `commit()`. Transports that ack each batch individually (NATS JetStream, AMQP, MQTT) accept any order.

> [!NOTE]
> **At-least-once + idempotent merge.** Redelivery (after a nack, a missed `commit()`, or an expired broker ack deadline) means a record can arrive twice. A downstream loader must dedup on a stable key — `message.id` is globally unique per source position (Kafka `partition:offset`, NATS `stream_sequence`, AMQP delivery tag) and makes a natural primary key. Source cursor fields are also available in `message.metadata` (`mqb.src.kafka_topic`/`mqb.src.kafka_offset`, `mqb.src.nats_subject`/`mqb.src.nats_stream_sequence`, `mqb.src.amqp_routing_key`/`mqb.src.amqp_delivery_tag`) when its Kafka, NATS, or AMQP source config sets `source_metadata: true` (off by default).
>
> **Ack deadlines vs slow loads.** JetStream `AckWait` (default 30s), AMQP prefetch/consumer-timeout and MQTT inflight windows each bound how long a batch may stay un-acked. Keep `batch_size × per-record handling cost` under the smallest deadline (or raise it in the endpoint config); past it the broker redelivers — correctness is preserved by idempotent merge, but reload work is wasted. **Kafka has no per-message nack:** `nack` there leaves the offset unadvanced, so redelivery happens on the next run/rebalance, not immediately.

`consumer.status()` returns a snapshot dict (`healthy`, `target`, `pending`, `capacity`, `error`, `details`). `pending` is the broker backlog/lag where the transport reports it — Kafka offset lag, AMQP queue depth, NATS JetStream `num_pending` — so `pending == 0` is a precise "caught up" check for a bounded drain; it is `None` where the broker exposes no backlog (core NATS, MQTT), where you fall back to a `timeout_ms` that returns `[]`. It's a point-in-time snapshot, not a guarantee.

`consumer.close()` releases the broker connection; it's idempotent, and `poll()` / `status()` raise afterwards. Python is garbage-collected, so close explicitly (or use the context-manager form, which closes on exit) rather than relying on the object being collected:

```python
with Consumer.from_config(cfg) as consumer:
    batch = consumer.poll(max=500, timeout_ms=1000)
    ...
    consumer.commit()
# connection released here
```

The endpoint config decides durability exactly as a route input does: a consumer-group config resumes from the last commit, a subscriber config receives only new messages. `Consumer.from_file` / `from_str` accept the same shapes, plus a named entry under a `consumers:` document section.

As a `dlt` resource this is a few lines:

```python
import dlt
from mq_bridge import Consumer

@dlt.resource(name="orders")
def orders():
    consumer = Consumer.from_config({"nats": {"subject": "orders", "stream": "orders", "url": "nats://localhost:4222"}})
    while not consumer.exhausted:
        batch = consumer.poll(max=500, timeout_ms=1000)
        if not batch:
            break                 # nothing more pending this run
        yield [m.json() for m in batch]
        consumer.commit()
```

## Custom endpoints and middleware

Register a Python object as an endpoint, and use its name in any route config. This helps when a system has a good Python SDK but no Rust one.

```python
import mq_bridge

class PulsarSource:
    def __init__(self, config):
        self.consumer = make_client(config["url"]).subscribe(config["topic"])

    def receive_batch(self, max_messages):
        # [] / None = nothing right now; raise StopIteration for end of stream
        return [m.data() for m in self.consumer.batch(max_messages)]

    def commit(self, dispositions):   # optional: one "ack"/"nack" per message
        ...

mq_bridge.register_endpoint("pulsar", lambda route_name, config: PulsarSource(config))

route = mq_bridge.Route.from_config({
    "input": {"pulsar": {"url": "pulsar://localhost:6650", "topic": "orders"}},
    "output": {"file": {"path": "orders.jsonl"}},
}, "ingest")
route.start()
```

Implement `send_batch(messages)` instead of `receive_batch` for an output. A middleware works the same way via `register_middleware`, with `on_receive` / `on_send` hooks that return one slot per input message (`None` drops it).

Each endpoint instance runs on its own thread and never sees concurrent calls, so it need not be thread-safe. Register before starting a route that names it; the registry is process-global and rejects a duplicate name, so use `unregister_endpoint(name)` / `unregister_middleware(name)` to release one once the routes using it have stopped.

**Full guide, including the Rust path:** [EXTENDING.md](https://github.com/marcomq/mq-bridge/blob/main/docs/EXTENDING.md).

## Config types and schema

`mq_bridge.config` ships `TypedDict` definitions for the `from_config` / `from_str` mappings, so editors autocomplete the config keys (`input`, `output`, `batch_size`, every transport config, middleware, …):

```python
from mq_bridge import Route
from mq_bridge.config import ConfigDocument

config: ConfigDocument = {
    "routes": {
        "orders": {
            "input": {"memory": {"topic": "orders.in", "capacity": 1600}},
            "output": {"response": {}},
            "batch_size": 128,
        }
    }
}
route = Route.from_config(config, "orders")
```

The types are generated from the JSON Schema, which the extension produces on demand from the Rust models, so there is no checked-in schema copy to drift:

```python
from mq_bridge import config_schema

schema = config_schema()        # the JSON Schema as a dict
```

`config_schema()` is handy for editor validation of YAML configs too: dump it to a file and point your `# yaml-language-server: $schema=` line at it.

> [!TIP]
> [`mq-bridge-app`](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app) can create and test route and endpoint configs through its UI. It doesn't replace your Python handlers, but it gets you a known-good connection and route shape to paste into `Route.from_config`, `Route.from_file`, `Publisher.from_config` or `Publisher.from_file`.

## Logging

By default the Rust core's internal `tracing` events go nowhere. Call `init_logging` once at startup to route them into the standard `logging` module, then configure output as usual:

```python
import logging
from mq_bridge import init_logging

logging.basicConfig(level=logging.INFO)
init_logging()  # or init_logging("debug")
```

Events land on a logger named after the emitting Rust module, `::` mapped to `.` (e.g. `mq_bridge.route`). `level` seeds the Rust-side filter (default `"warn"`); the `MQ_BRIDGE_LOG` / `RUST_LOG` environment variables take precedence over it. Filtering happens in Rust, so suppressed events never cross into Python. Call it once per process; a second call raises.

## Tuning (environment variables)

These knobs are read from the environment at startup:

| Variable | Default | Effect |
| :--- | :--- | :--- |
| `MQ_BRIDGE_PY_HANDLER_EXECUTOR` | `worker` | `worker` runs handlers on a dedicated interpreter thread that coalesces queued batches under one GIL acquisition (best under load); `direct` calls the handler inline. |
| `MQ_BRIDGE_PY_HANDLER_CONCURRENCY` | CPU count | Max in-flight handler batches. `0` disables the limit. |
| `MQ_BRIDGE_PY_GC_MODE` | `default` | `default` leaves CPython's cyclic GC alone; `count` disables it and runs `gc.collect()` every N messages; `off` disables it entirely (pure refcounting). |
| `MQ_BRIDGE_PY_GC_THRESHOLD` | `100000` | Messages between collections when `MQ_BRIDGE_PY_GC_MODE=count`. |

## Examples

| Example | What it shows |
| :--- | :--- |
| [`examples/raw_route.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/raw_route.py) | raw `Message` handler |
| [`examples/json_route.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/json_route.py) | `kind`-based JSON handler |
| [`examples/consume_pull.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/consume_pull.py) | pull-based `Consumer` |
| [`examples/dlt_source.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/dlt_source.py) | `dlt` resource with `poll_batch` / `ack` |
| [`examples/bench_memory.py`](https://github.com/marcomq/mq-bridge/blob/main/bindings/python/examples/bench_memory.py) | in-memory throughput benchmark |

From a checkout:

```bash
cd bindings/python
uv run python examples/raw_route.py
uv run python examples/json_route.py
```

---

## Development

`uv` handles the Python-side workflow, while `maturin` stays the build backend:

```bash
cd bindings/python
uv sync --group dev --no-install-project
uv run maturin develop
uv run pytest -q
```

Performance smoke tests are skipped by default because they start routes and measure local throughput:

```bash
MQ_BRIDGE_RUN_PERF_TESTS=1 uv run pytest -q -m performance
```

### Regenerating the config types

`mq_bridge/config.pyi` and `mq_bridge/config.py` are **generated; do not edit them by hand**. Regenerate them whenever the Rust config models change (e.g. adding a field or a new endpoint). The fastest way skips the extension build and reads the schema the engine writes to `mq-bridge.schema.json`:

```bash
# from bindings/python/
cargo test -p mq-bridge --features schema --lib schema_tests::generate_json_schema
uv run --no-sync python scripts/gen_config_types.py --schema ../../mq-bridge.schema.json
```

Without `--schema`, the generator reads the schema from the compiled extension, so rebuild it first with `uv run --no-sync maturin develop`.

Then commit the updated `config.pyi` / `config.py`. `tests/test_config_types.py` asserts the checked-in output matches the schema, so CI (the "Python package smoke test" job) fails if you skip this after a models change.

### Benchmarks

Memory throughput:

```bash
uv run maturin develop --release
uv run python examples/bench_memory.py --messages 100000
```

HTTP comparison, driven by a native load generator (`wrk`) so the client is never the bottleneck. It boots each server itself (mq-bridge in worker and direct executor modes, plus FastAPI, Starlette, Sanic, aiohttp and FastStream when installed) and drives each with `wrk`:

```bash
uv run maturin develop --release
uv sync --group bench   # optional Python HTTP peers
uv run python analysis/bench_http_native.py --connections 1,8,32 --duration 8
```

Requires `wrk` on `PATH` (`brew install wrk`). The FastStream target compares its ASGI custom-route path over Uvicorn; it is not a broker-backed subscriber/publisher benchmark.

HTTP sending, mq-bridge `Publisher` vs `requests`, `httpx` and `aiohttp`, against a local nginx (`brew install nginx`):

```bash
uv run python analysis/bench_http_send.py --concurrency 1,16,64 --duration 5
```

Kafka consume and Postgres load, against the Compose services in `tests/integration/docker-compose/` (`kafka.yml`, `postgres.yml`); needs `confluent-kafka`, `aiokafka`, `uvloop`, `orjson`, `psycopg` and `asyncpg`:

```bash
python analysis/bench_kafka_consume.py seed --records 1000000
python analysis/bench_kafka_consume.py run --records 1000000 --repeats 3
python analysis/bench_pg_load.py gen --rows 1000000 --file /tmp/readings.jsonl
python analysis/bench_pg_load.py run --file /tmp/readings.jsonl --repeats 3
```

> [!WARNING]
> `bench_pg_load.py run` drops and recreates the `readings` table before every variant. Point `POSTGRES_URL` at a disposable database.

Run the scripts from a directory where `import mq_bridge` resolves to the build you want to measure. In `bindings/python/` that is the in-tree package, which is only current after `maturin develop --release`.
