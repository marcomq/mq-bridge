"""Load a JSONL file into a Postgres table: mq-bridge vs psycopg3 and asyncpg.

Every variant starts from the same JSONL file (sensor readings, one JSON object per line) and
an empty `readings` table, and ends when all rows are committed (checked with `count(*)`).

- `psycopg-copy` / `psycopg-copy-orjson`: read lines, `json.loads` / `orjson.loads`, `COPY ... FROM STDIN` via `write_row`.
- `psycopg-executemany`: `executemany` of an `INSERT` in chunks of 1000 (psycopg 3 pipelines it).
- `asyncpg-copy`: `copy_records_to_table` in chunks of 10,000; `ts` parsed with `datetime.fromisoformat`.
- `mqb-route-copy` / `mqb-route-insert`: mq-bridge `Route` file -> sqlx with `columns: auto`,
  with and without `bulk_copy`, batches of 1000, one connection. Python only starts the route.
- `mqb-route-copy-c4`: as `mqb-route-copy` with `batch_size: 5000`, `concurrency: 4` (4 connections).
- `mqb-route-copy-b5000`: as `mqb-route-copy` with `batch_size: 5000`, still one connection.
- `psycopg-copy-orjson-4proc` / `mqb-route-copy-4proc`: the file is split into 4 parts up front
  (not timed); 4 processes each load one part, psycopg with one `COPY` + `orjson`, mq-bridge with
  one route (`bulk_copy`, `batch_size: 5000`, `concurrency: 1`).
- `mqb-publisher`: Python reads the lines and hands raw bytes to `Publisher.send_batch` (1000 per call),
  sqlx sink with `columns: auto` + `bulk_copy`.

Usage (Postgres from tests/integration/docker-compose/postgres.yml on localhost:5432):
    python analysis/bench_pg_load.py gen --rows 1000000 --file /tmp/readings.jsonl
    python analysis/bench_pg_load.py run --file /tmp/readings.jsonl --repeats 3
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import statistics
import subprocess
import sys
import time
from datetime import datetime

PG_URL = os.environ.get("POSTGRES_URL", "postgres://testuser:testpass@127.0.0.1:5432/testdb")
TABLE = "readings"
COLS = ("id", "sensor", "ts", "temp", "humidity", "status")
DDL = f"""
DROP TABLE IF EXISTS {TABLE};
CREATE TABLE {TABLE} (
    id bigint NOT NULL,
    sensor text NOT NULL,
    ts timestamptz NOT NULL,
    temp double precision NOT NULL,
    humidity double precision NOT NULL,
    status text NOT NULL
);
"""
VARIANTS = [
    "psycopg-copy",
    "psycopg-copy-orjson",
    "psycopg-executemany",
    "asyncpg-copy",
    "mqb-route-copy",
    "mqb-route-copy-b5000",
    "mqb-route-copy-c4",
    "psycopg-copy-orjson-4proc",
    "mqb-route-copy-4proc",
    "mqb-route-insert",
    "mqb-publisher",
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


def gen(rows: int, path: str) -> None:
    with open(path, "w") as f:
        for i in range(rows):
            f.write(json.dumps(record(i), separators=(",", ":")))
            f.write("\n")
    print(f"wrote {rows} rows to {path} ({os.path.getsize(path) / 1e6:.1f} MB)")


def run_psycopg_copy(path: str, loads) -> None:
    import psycopg

    with psycopg.connect(PG_URL) as conn, conn.cursor() as cur, open(path, "rb") as f:
        with cur.copy(f"COPY {TABLE} ({', '.join(COLS)}) FROM STDIN") as cp:
            for line in f:
                d = loads(line)
                cp.write_row((d["id"], d["sensor"], d["ts"], d["temp"], d["humidity"], d["status"]))


def run_psycopg_executemany(path: str) -> None:
    import psycopg

    sql = f"INSERT INTO {TABLE} ({', '.join(COLS)}) VALUES (%s, %s, %s, %s, %s, %s)"
    with psycopg.connect(PG_URL) as conn, conn.cursor() as cur, open(path, "rb") as f:
        chunk = []
        for line in f:
            d = json.loads(line)
            chunk.append((d["id"], d["sensor"], d["ts"], d["temp"], d["humidity"], d["status"]))
            if len(chunk) == 1000:
                cur.executemany(sql, chunk)
                chunk.clear()
        if chunk:
            cur.executemany(sql, chunk)


def run_asyncpg_copy(path: str) -> None:
    import asyncio

    import asyncpg
    import uvloop

    async def main() -> None:
        conn = await asyncpg.connect(PG_URL)
        try:
            async with conn.transaction():
                chunk = []
                with open(path, "rb") as f:
                    for line in f:
                        d = json.loads(line)
                        ts = d["ts"]
                        if ts.endswith("Z"):
                            ts = ts[:-1] + "+00:00"
                        chunk.append((d["id"], d["sensor"], datetime.fromisoformat(ts),
                                      d["temp"], d["humidity"], d["status"]))
                        if len(chunk) == 10_000:
                            await conn.copy_records_to_table(TABLE, records=chunk, columns=COLS)
                            chunk = []
                if chunk:
                    await conn.copy_records_to_table(TABLE, records=chunk, columns=COLS)
        finally:
            await conn.close()

    uvloop.run(main())


def sqlx_sink(bulk_copy: bool) -> dict:
    return {"sqlx": {"url": PG_URL, "table": TABLE, "columns": "auto", "bulk_copy": bulk_copy}}


def run_mqb_route(path: str, bulk_copy: bool, batch_size: int = 1000, concurrency: int = 1) -> None:
    from mq_bridge import Route

    Route.from_config({
        "input": {"file": {"path": path, "format": "payload"}},
        "output": sqlx_sink(bulk_copy),
        "batch_size": batch_size,
        "concurrency": concurrency,
        "exit_on_empty": True,
    }).run()


PARTS = 4


def part_paths(path: str) -> list[str]:
    return [f"{path}.part{k}" for k in range(PARTS)]


def split_file(path: str) -> None:
    marker = f"{path}.parts-done"
    st = os.stat(path)
    stamp = f"{st.st_size}:{st.st_mtime_ns}"
    if os.path.exists(marker):
        with open(marker) as m:
            if m.read() == stamp:
                return
        os.remove(marker)
    outs = [open(p, "wb") for p in part_paths(path)]
    try:
        with open(path, "rb") as f:
            for i, line in enumerate(f):
                outs[i % PARTS].write(line)
    finally:
        for o in outs:
            o.close()
    with open(marker, "w") as m:
        m.write(stamp)


def _psycopg_part(path: str) -> None:
    import orjson

    run_psycopg_copy(path, orjson.loads)


def _mqb_part(path: str) -> None:
    run_mqb_route(path, True, 5000, 1)


def run_parallel(target, path: str) -> None:
    import multiprocessing as mp

    procs = [mp.Process(target=target, args=(p,)) for p in part_paths(path)]
    for p in procs:
        p.start()
    for p in procs:
        p.join()
        if p.exitcode != 0:
            raise SystemExit(f"worker failed with {p.exitcode}")


def run_mqb_publisher(path: str) -> None:
    from mq_bridge import Publisher

    pub = Publisher.from_config(sqlx_sink(True))
    with open(path, "rb") as f:
        chunk = []
        for line in f:
            chunk.append(line.rstrip(b"\n"))
            if len(chunk) == 1000:
                pub.send_batch(chunk)
                chunk = []
        if chunk:
            pub.send_batch(chunk)


def child(variant: str, path: str) -> None:
    import orjson

    runners = {
        "psycopg-copy": lambda: run_psycopg_copy(path, json.loads),
        "psycopg-copy-orjson": lambda: run_psycopg_copy(path, orjson.loads),
        "psycopg-executemany": lambda: run_psycopg_executemany(path),
        "asyncpg-copy": lambda: run_asyncpg_copy(path),
        "mqb-route-copy": lambda: run_mqb_route(path, True),
        "mqb-route-copy-b5000": lambda: run_mqb_route(path, True, 5000, 1),
        "psycopg-copy-orjson-4proc": lambda: run_parallel(_psycopg_part, path),
        "mqb-route-copy-4proc": lambda: run_parallel(_mqb_part, path),
        "mqb-route-copy-c4": lambda: run_mqb_route(path, True, 5000, 4),
        "mqb-route-insert": lambda: run_mqb_route(path, False),
        "mqb-publisher": lambda: run_mqb_publisher(path),
    }
    start = time.perf_counter()
    runners[variant]()
    print(json.dumps({"variant": variant, "seconds": time.perf_counter() - start}))


def reset_table() -> None:
    import psycopg

    with psycopg.connect(PG_URL, autocommit=True) as conn:
        conn.execute(DDL)


def row_stats() -> tuple[int, float]:
    import psycopg

    with psycopg.connect(PG_URL) as conn:
        n, s = conn.execute(f"SELECT count(*), coalesce(sum(temp), 0) FROM {TABLE}").fetchone()
    return n, s


def run(path: str, repeats: int, variants: list[str], timeout: float) -> None:
    with open(path, "rb") as f:
        rows = sum(1 for _ in f)
    want = sum(record(i)["temp"] for i in range(rows))
    split_file(path)
    results: dict[str, list[float]] = {v: [] for v in variants}
    for rep in range(repeats):
        for v in variants:
            reset_table()
            proc = subprocess.Popen(
                [sys.executable, __file__, "child", v, "--file", path],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True,
            )
            try:
                stdout, stderr = proc.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                # Kill the whole group, so -4proc workers stop too; communicate waits for their pipes.
                os.killpg(proc.pid, signal.SIGKILL)
                proc.communicate()
                print(f"rep {rep + 1} {v}: TIMEOUT after {timeout:.0f}s", flush=True)
                continue
            if proc.returncode != 0:
                print(f"rep {rep + 1} {v}: FAILED\n{stderr[-2000:]}", flush=True)
                continue
            secs = json.loads(stdout.strip().splitlines()[-1])["seconds"]
            n, s = row_stats()
            ok = n == rows and abs(s - want) < 1e-3 * rows
            if ok:
                results[v].append(secs)
            print(f"rep {rep + 1} {v:20} {secs:7.2f} s  {rows / secs:>11,.0f} rows/s"
                  f"{'' if ok else f'  WRONG rows={n} sum={s:.2f}'}", flush=True)

    print(f"\n| Variant | rows/s (median of {repeats}) | seconds per run |")
    print("| :--- | ---: | :--- |")
    for v in sorted((v for v in variants if results[v]), key=lambda v: statistics.median(results[v])):
        print(f"| {v} | {rows / statistics.median(results[v]):,.0f} | "
              f"{' / '.join(f'{x:.2f}' for x in results[v])} |")


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["gen", "run", "child"])
    p.add_argument("variant", nargs="?")
    p.add_argument("--rows", type=int, default=1_000_000)
    p.add_argument("--file", required=True)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--variants", default=",".join(VARIANTS))
    p.add_argument("--timeout", type=float, default=120)
    a = p.parse_args()
    if a.mode == "gen":
        gen(a.rows, a.file)
    elif a.mode == "child":
        child(a.variant, a.file)
    else:
        run(a.file, a.repeats, a.variants.split(","), a.timeout)


if __name__ == "__main__":
    main()
