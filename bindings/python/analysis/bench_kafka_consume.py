"""Consume a Kafka topic and parse every JSON record into a Python dict.

Each variant reads the whole topic with a fresh consumer group from the earliest offset,
parses every value into a dict and sums one field (checked against the seeded total).

- `confluent-json` / `confluent-orjson`: confluent-kafka `consume(1000)` + `json.loads` / `orjson.loads`,
  with `fetch.queue.backoff.ms=10`; `confluent-json-default` keeps librdkafka's 1 s default.
- `aiokafka`: `getmany(max_records=1000)` + `json.loads` on uvloop.
- `mqb-consumer`: mq-bridge `Consumer.poll(max=1000)` + `Message.json()`, `commit()` per batch.
- `mqb-route`: mq-bridge `Route` (kafka -> null) with a batch handler calling `Message.json()`.
- `mqb-consumer-orjson` / `mqb-route-orjson`: as above, but `orjson.loads(m.payload)`.

Every variant runs in its own process. `steady` is records/s from the first record to the
last; `total` also counts connecting and the group join.

Usage (Kafka from tests/integration/docker-compose/kafka.yml on localhost:9092):
    python analysis/bench_kafka_consume.py seed --records 1000000
    python analysis/bench_kafka_consume.py run --records 1000000 --repeats 3
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import threading
import time
import uuid

BROKER = "localhost:9092"
TOPIC = "pybench.readings"
PARTITIONS = 4
BATCH = 1000
VARIANTS = [
    "confluent-json", "confluent-json-default", "confluent-orjson", "aiokafka",
    "mqb-consumer", "mqb-consumer-orjson", "mqb-route", "mqb-route-orjson",
]


def record(i: int) -> dict:
    return {
        "id": i,
        "sensor": f"s-{i % 5000:04d}",
        "ts": f"2026-11-16T12:{(i // 60000) % 60:02d}:{(i // 1000) % 60:02d}.{i % 1000:03d}Z",
        "temp": round(15 + (i % 1000) / 50, 2),
        "humidity": round(30 + (i % 700) / 20, 2),
        "status": "ok" if i % 97 else "warn",
    }


def expected_sum(n: int) -> float:
    return sum(record(i)["temp"] for i in range(n))


def seed(n: int) -> None:
    from confluent_kafka import Producer
    from confluent_kafka.admin import AdminClient, NewTopic

    admin = AdminClient({"bootstrap.servers": BROKER})
    if TOPIC in admin.list_topics(timeout=10).topics:
        for f in admin.delete_topics([TOPIC]).values():
            f.result()
        time.sleep(2)
    for f in admin.create_topics([NewTopic(TOPIC, PARTITIONS, 1)]).values():
        f.result()

    producer = Producer({"bootstrap.servers": BROKER, "linger.ms": 20, "batch.num.messages": 10000})
    payload_bytes = 0
    for i in range(n):
        value = json.dumps(record(i), separators=(",", ":")).encode()
        payload_bytes += len(value)
        while True:
            try:
                producer.produce(TOPIC, value)
                break
            except BufferError:
                producer.poll(0.05)
        if i % 10000 == 0:
            producer.poll(0)
    producer.flush()
    print(f"seeded {n} records into {TOPIC}, avg {payload_bytes / n:.0f} B")


class Tally:
    def __init__(self) -> None:
        self.count = 0
        self.total = 0.0
        self.first: float | None = None
        self.last: float | None = None

    def begin(self, n: int) -> None:
        if n and self.first is None:
            self.first = time.perf_counter()

    def add(self, n: int, s: float) -> None:
        self.count += n
        self.total += s
        self.last = time.perf_counter()


def run_confluent(n: int, loads, tuned: bool = True) -> Tally:
    from confluent_kafka import Consumer

    conf: dict[str, object] = {
        "bootstrap.servers": BROKER,
        "group.id": f"bench-{uuid.uuid4()}",
        "auto.offset.reset": "earliest",
    }
    if tuned:
        # librdkafka's 1 s default idles a consumer slower than the fetcher once its queue is full.
        conf["fetch.queue.backoff.ms"] = 10
    c = Consumer(conf)
    c.subscribe([TOPIC])
    t = Tally()
    while t.count < n:
        s = 0.0
        ok = 0
        msgs = c.consume(num_messages=BATCH, timeout=1.0)
        t.begin(len(msgs))
        for m in msgs:
            if m.error():
                continue
            s += loads(m.value())["temp"]
            ok += 1
        t.add(ok, s)
    c.close()
    return t


def run_aiokafka(n: int) -> Tally:
    import asyncio

    import uvloop
    from aiokafka import AIOKafkaConsumer

    async def main() -> Tally:
        c = AIOKafkaConsumer(
            TOPIC,
            bootstrap_servers=BROKER,
            group_id=f"bench-{uuid.uuid4()}",
            auto_offset_reset="earliest",
        )
        await c.start()
        t = Tally()
        try:
            while t.count < n:
                batches = await c.getmany(timeout_ms=1000, max_records=BATCH)
                for msgs in batches.values():
                    t.begin(len(msgs))
                    t.add(len(msgs), sum(json.loads(m.value)["temp"] for m in msgs))
        finally:
            await c.stop()
        return t

    return uvloop.run(main())


def kafka_endpoint() -> dict:
    return {"kafka": {"url": BROKER, "topic": TOPIC, "group_id": f"bench-{uuid.uuid4()}"}}


def run_mqb_consumer(n: int, decode) -> Tally:
    from mq_bridge import Consumer

    t = Tally()
    with Consumer.from_config(kafka_endpoint()) as c:
        while t.count < n:
            msgs = c.poll(max=BATCH, timeout_ms=1000)
            t.begin(len(msgs))
            t.add(len(msgs), sum(decode(m)["temp"] for m in msgs))
            c.commit()
    return t


def run_mqb_route(n: int, decode) -> Tally:
    from mq_bridge import Route

    t = Tally()
    done = threading.Event()

    def handler(msgs):
        t.begin(len(msgs))
        t.add(len(msgs), sum(decode(m)["temp"] for m in msgs))
        if t.count >= n:
            done.set()
        return [None] * len(msgs)

    route = Route.from_config({"input": kafka_endpoint(), "output": None, "batch_size": BATCH})
    route.with_batch_handler(handler)
    route.start()
    done.wait(120)
    route.stop()
    return t


def child(variant: str, n: int) -> None:
    import orjson

    start = time.perf_counter()
    if variant == "confluent-json":
        t = run_confluent(n, json.loads)
    elif variant == "confluent-json-default":
        t = run_confluent(n, json.loads, tuned=False)
    elif variant == "confluent-orjson":
        t = run_confluent(n, orjson.loads)
    elif variant == "aiokafka":
        t = run_aiokafka(n)
    elif variant == "mqb-consumer":
        t = run_mqb_consumer(n, lambda m: m.json())
    elif variant == "mqb-consumer-orjson":
        t = run_mqb_consumer(n, lambda m: orjson.loads(m.payload))
    elif variant == "mqb-route":
        t = run_mqb_route(n, lambda m: m.json())
    elif variant == "mqb-route-orjson":
        t = run_mqb_route(n, lambda m: orjson.loads(m.payload))
    else:
        raise SystemExit(f"unknown variant {variant}")
    end = time.perf_counter()
    print(json.dumps({
        "variant": variant,
        "count": t.count,
        "sum": t.total,
        "steady": t.count / ((t.last or end) - (t.first or start)),
        "total": t.count / (end - start),
    }))


def run(n: int, repeats: int, variants: list[str], timeout: float) -> None:
    want = expected_sum(n)
    rows: dict[str, list[dict]] = {v: [] for v in variants}
    for rep in range(repeats):
        for v in variants:
            try:
                proc = subprocess.run(
                    [sys.executable, __file__, "child", v, "--records", str(n)],
                    capture_output=True, text=True, timeout=timeout,
                )
            except subprocess.TimeoutExpired:
                print(f"{v}: TIMEOUT after {timeout:.0f}s", flush=True)
                continue
            if proc.returncode != 0:
                print(f"{v}: FAILED\n{proc.stderr[-2000:]}", flush=True)
                continue
            r = json.loads(proc.stdout.strip().splitlines()[-1])
            if r["count"] != n or abs(r["sum"] - want) > 1e-3 * n:
                print(f"{v}: WRONG count={r['count']} sum={r['sum']:.2f} want={want:.2f}", flush=True)
                continue
            rows[v].append(r)
            print(f"rep {rep + 1} {v:18} steady {r['steady']:>11,.0f}/s  total {r['total']:>11,.0f}/s", flush=True)

    print("\n| Variant | steady records/s (median) | incl. startup (median) |")
    print("| :--- | ---: | ---: |")
    for v in sorted(variants, key=lambda v: -statistics.median([r["steady"] for r in rows[v]] or [0])):
        if rows[v]:
            print(f"| {v} | {statistics.median(r['steady'] for r in rows[v]):,.0f} | "
                  f"{statistics.median(r['total'] for r in rows[v]):,.0f} |")
        else:
            print(f"| {v} | failed | failed |")


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["seed", "run", "child"])
    p.add_argument("variant", nargs="?")
    p.add_argument("--records", type=int, default=1_000_000)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--variants", default=",".join(VARIANTS))
    p.add_argument("--timeout", type=float, default=90)
    a = p.parse_args()
    if a.mode == "seed":
        seed(a.records)
    elif a.mode == "child":
        child(a.variant, a.records)
    else:
        run(a.records, a.repeats, a.variants.split(","), a.timeout)


if __name__ == "__main__":
    main()
