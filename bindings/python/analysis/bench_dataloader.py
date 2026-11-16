"""Batched key lookups from Python: mq-bridge `Caller` vs GraphQL DataLoaders vs one query per key.

Each client fetches one user per request by primary key, for a fixed duration, against the
lookup bench's Postgres (1M rows, `benches/lookup/docker-compose.yml`). Variants:

- `asyncpg-direct`: one `SELECT ... WHERE path = $1` per request on an asyncpg pool.
- `strawberry-dataloader` / `aiodataloader`: one shared DataLoader (no cache, batches of
  up to 256) whose batch function runs `WHERE path = ANY($1)` on the same pool.
- `mqb-caller`: `Caller.call(path=key)` from one thread per client into a route with a
  batched `lookup` and `parallel`, the same route as `benches/lookup/caller`.
- `mqb-caller-async`: the same route through `await Caller.acall(path=key)`.

The asyncio variants (incl. `mqb-caller-async`) run one event loop (uvloop when installed)
with one task per client; `mqb-caller` runs one OS thread per client. Every answer is checked.

Usage (from bindings/python, with the lookup bench's Postgres up):
    uv run python analysis/bench_dataloader.py --clients 1,16,64,256,1024 --duration 5
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import threading
import time

PG_URL = os.environ.get("POSTGRES_URL", "postgres://bench:bench@127.0.0.1:55432/bench")
POOL = 8
BATCH = 256
TARGETS = ["asyncpg-direct", "strawberry-dataloader", "aiodataloader", "mqb-caller", "mqb-caller-async"]
BY_KEY = "SELECT path, id, name, tier FROM users WHERE path = $1"
BY_KEYS = "SELECT path, id, name, tier FROM users WHERE path = ANY($1::text[])"


def key(rng: random.Random) -> tuple[str, str]:
    n = rng.randrange(1_000_000)
    return f"/u/{n:06d}", f"user_{n}"


def summarize(count: int, elapsed: float, latencies: list[float]) -> dict:
    latencies.sort()
    pick = lambda q: latencies[min(len(latencies) - 1, int(q * len(latencies)))] * 1000 if latencies else 0.0
    return {"rps": count / elapsed, "p50_ms": pick(0.50), "p99_ms": pick(0.99)}


# ---------------------------------------------------------------------------------------
# asyncio variants


def run_asyncio(clients: int, duration: float, warmup: float, variant: str) -> dict:
    import asyncio

    import asyncpg

    try:
        import uvloop

        uvloop.install()
    except ImportError:
        pass

    async def main() -> dict:
        pool = None
        if variant != "mqb-caller-async":
            pool = await asyncpg.create_pool(PG_URL, min_size=POOL, max_size=POOL)

        async def batch_fn(keys):
            rows = await pool.fetch(BY_KEYS, list(keys))
            by_path = {r["path"]: dict(r) for r in rows}
            return [by_path.get(k) for k in keys]

        if variant == "mqb-caller-async":
            users = users_caller()

            def lookup(k):
                return users.acall(path=k)
        elif variant == "asyncpg-direct":
            async def lookup(k):
                row = await pool.fetchrow(BY_KEY, k)
                return dict(row) if row else None
        elif variant == "strawberry-dataloader":
            from strawberry.dataloader import DataLoader

            loader = DataLoader(load_fn=batch_fn, max_batch_size=BATCH, cache=False)
            lookup = loader.load
        else:
            from aiodataloader import DataLoader

            loader = DataLoader(batch_load_fn=batch_fn, max_batch_size=BATCH, cache=False)
            lookup = loader.load

        async def phase(seconds: float, record: bool):
            deadline = time.perf_counter() + seconds
            counts = [0] * clients
            lats: list[list[float]] = [[] for _ in range(clients)]

            async def client(i: int) -> None:
                rng = random.Random(i)
                while time.perf_counter() < deadline:
                    k, name = key(rng)
                    t0 = time.perf_counter()
                    user = await lookup(k)
                    if record:
                        lats[i].append(time.perf_counter() - t0)
                    if user is None or user["name"] != name:
                        raise RuntimeError(f"wrong answer for {k}: {user}")
                    counts[i] += 1

            start = time.perf_counter()
            await asyncio.gather(*(client(i) for i in range(clients)))
            return sum(counts), time.perf_counter() - start, [x for l in lats for x in l]

        await phase(warmup, False)
        count, elapsed, lats = await phase(duration, True)
        if pool is None:
            users.stop()
        else:
            await pool.close()
        return summarize(count, elapsed, lats)

    return asyncio.run(main())


# ---------------------------------------------------------------------------------------
# mq-bridge Caller, one thread per client


def users_caller():
    from mq_bridge import Caller

    route = {
        "batch_size": BATCH,
        "concurrency": 4,
        "input": {
            "middlewares": [
                {
                    "lookup": {
                        "from": {
                            "sqlx": {
                                "url": PG_URL,
                                "table": "users",
                                "max_connections": POOL,
                                "lookup_query": "SELECT path, id, name, tier FROM users WHERE path IN (${payload:path})",
                            }
                        },
                        "into": "$",
                    }
                },
                {"parallel": {"max_in_flight": 16}},
            ]
        },
    }
    return Caller.from_config(route, "users")


def run_caller(clients: int, duration: float, warmup: float) -> dict:
    users = users_caller()

    def phase(seconds: float, record: bool):
        deadline = time.perf_counter() + seconds
        counts = [0] * clients
        lats: list[list[float]] = [[] for _ in range(clients)]
        errors: list[BaseException] = []

        def client(i: int) -> None:
            rng = random.Random(i)
            try:
                while time.perf_counter() < deadline:
                    k, name = key(rng)
                    t0 = time.perf_counter()
                    user = users.call(path=k)
                    if record:
                        lats[i].append(time.perf_counter() - t0)
                    if user.get("name") != name:
                        raise RuntimeError(f"wrong answer for {k}: {user}")
                    counts[i] += 1
            except BaseException as exc:  # surfaced after join
                errors.append(exc)

        threads = [threading.Thread(target=client, args=(i,)) for i in range(clients)]
        start = time.perf_counter()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        if errors:
            raise errors[0]
        return sum(counts), time.perf_counter() - start, [x for l in lats for x in l]

    phase(warmup, False)
    count, elapsed, lats = phase(duration, True)
    users.stop()
    return summarize(count, elapsed, lats)


def child(target: str, clients: int, duration: float, warmup: float) -> None:
    if target == "mqb-caller":
        result = run_caller(clients, duration, warmup)
    else:
        result = run_asyncio(clients, duration, warmup, target)
    print(json.dumps(result))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--clients", default="1,16,64,256,1024")
    parser.add_argument("--duration", type=float, default=5.0)
    parser.add_argument("--warmup", type=float, default=1.0)
    parser.add_argument("--runs", type=int, default=1, help="runs per cell; the median is reported")
    parser.add_argument("--targets", default=",".join(TARGETS))
    parser.add_argument("--child", nargs=2, metavar=("TARGET", "CLIENTS"), help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.child:
        child(args.child[0], int(args.child[1]), args.duration, args.warmup)
        return

    clients = [int(c) for c in args.clients.split(",") if c]
    targets = [t for t in args.targets.split(",") if t]
    results: dict[tuple[str, int], dict] = {}
    for c in clients:
        for target in targets:
            runs = []
            for _ in range(args.runs):
                out = subprocess.run(
                    [sys.executable, __file__, "--duration", str(args.duration), "--warmup", str(args.warmup),
                     "--child", target, str(c)],
                    capture_output=True, text=True,
                )
                if out.returncode != 0:
                    print(f"{target} clients={c} failed:\n{out.stderr[-2000:]}", file=sys.stderr)
                    break
                runs.append(json.loads(out.stdout.strip().splitlines()[-1]))
            if not runs:
                continue
            best = sorted(runs, key=lambda r: r["rps"])[len(runs) // 2]
            results[(target, c)] = best
            print(f"{target:22} clients={c:<5} {best['rps']:>10,.0f} keys/s  "
                  f"p50 {best['p50_ms']:.2f} ms  p99 {best['p99_ms']:.2f} ms", flush=True)

    print()
    print("| variant | " + " | ".join(f"{c} clients" for c in clients) + " |")
    print("| :--- | " + " | ".join("---:" for _ in clients) + " |")
    for target in targets:
        cells = []
        for c in clients:
            r = results.get((target, c))
            cells.append(f"{r['rps']:,.0f} (p99 {r['p99_ms']:.1f} ms)" if r else "–")
        print(f"| {target} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
