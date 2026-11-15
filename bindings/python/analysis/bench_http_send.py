"""HTTP client benchmark: sending POSTs from Python with mq-bridge vs requests/httpx/aiohttp.

Every client sends the same 200-byte JSON POST to a local nginx (`return 200`), so the
target is never a Python server. Each cell runs in its own subprocess for a fixed duration
and counts the requests answered with 2xx.

Usage:
    uv run python analysis/bench_http_send.py --concurrency 1,16,64 --duration 5

Requires `nginx` on PATH (`brew install nginx`). The peer clients are optional; a missing
one is skipped.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
from pathlib import Path

HOST = "127.0.0.1"
BODY = json.dumps({"id": 1, "name": "x" * 160, "tier": "pro"}).encode()
TARGETS = [
    "requests",
    "httpx-sync",
    "httpx-async",
    "aiohttp",
    "mqb-send-threads",
    "mqb-send-batch",
]


def parse_csv_ints(value: str) -> list[int]:
    return [int(p) for p in value.split(",") if p.strip()]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind((HOST, 0))
        return sock.getsockname()[1]


def start_nginx(workers: int) -> tuple[subprocess.Popen, int, Path]:
    port = free_port()
    root = Path(tempfile.mkdtemp(prefix="mqb-nginx-"))
    (root / "logs").mkdir()
    conf = root / "nginx.conf"
    conf.write_text(
        textwrap.dedent(
            f"""
            worker_processes {workers};
            daemon off;
            error_log {root}/logs/error.log crit;
            pid {root}/nginx.pid;
            events {{ worker_connections 4096; }}
            http {{
                access_log off;
                keepalive_requests 10000000;
                keepalive_timeout 120s;
                client_body_buffer_size 16k;
                server {{
                    listen {HOST}:{port} reuseport;
                    location / {{ return 200 "ok"; }}
                }}
            }}
            """
        )
    )
    proc = subprocess.Popen(["nginx", "-p", str(root), "-c", str(conf)])
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            socket.create_connection((HOST, port), timeout=0.2).close()
            return proc, port, root
        except OSError:
            time.sleep(0.05)
    proc.kill()
    raise SystemExit("nginx did not start")


# ---------------------------------------------------------------------------------------
# Workers: each builds its clients once, runs a warmup, then returns (count, elapsed) for
# the 2xx responses received in the measured window.


def measure(run, warmup: float, duration: float) -> tuple[int, float]:
    run(time.perf_counter() + warmup)
    start = time.perf_counter()
    count = run(start + duration)
    return count, time.perf_counter() - start


def run_threads(sends: list, deadline: float) -> int:
    counts = [0] * len(sends)
    errors: list[BaseException] = []

    def loop(i: int) -> None:
        send = sends[i]
        try:
            while time.perf_counter() < deadline:
                send()
                counts[i] += 1
        except BaseException as exc:  # surfaced after join
            errors.append(exc)

    threads = [threading.Thread(target=loop, args=(i,)) for i in range(len(sends))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if errors:
        raise errors[0]
    return sum(counts)


def measure_threads(sends: list, warmup: float, duration: float) -> tuple[int, float]:
    return measure(lambda deadline: run_threads(sends, deadline), warmup, duration)


def w_requests(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    import requests

    def make_send():
        session = requests.Session()
        adapter = requests.adapters.HTTPAdapter(pool_connections=1, pool_maxsize=1)
        session.mount("http://", adapter)

        def send():
            session.post(url, data=BODY, headers={"content-type": "application/json"}).raise_for_status()

        return send

    return measure_threads([make_send() for _ in range(concurrency)], warmup, duration)


def w_httpx_sync(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    import httpx

    limits = httpx.Limits(max_connections=concurrency, max_keepalive_connections=concurrency)
    client = httpx.Client(limits=limits)

    def send():
        client.post(url, content=BODY, headers={"content-type": "application/json"}).raise_for_status()

    return measure_threads([send] * concurrency, warmup, duration)


def run_async(concurrency: int, warmup: float, duration: float, make_client, post) -> tuple[int, float]:
    import asyncio

    try:
        import uvloop

        uvloop.install()
    except ImportError:
        pass

    async def main() -> tuple[int, float]:
        client = await make_client()

        async def run(deadline: float) -> int:
            counts = [0] * concurrency

            async def loop(i: int) -> None:
                while time.perf_counter() < deadline:
                    await post(client)
                    counts[i] += 1

            await asyncio.gather(*(loop(i) for i in range(concurrency)))
            return sum(counts)

        try:
            await run(time.perf_counter() + warmup)
            start = time.perf_counter()
            count = await run(start + duration)
            return count, time.perf_counter() - start
        finally:
            await (getattr(client, "aclose", None) or client.close)()

    return asyncio.run(main())


def w_httpx_async(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    import httpx

    async def make_client():
        limits = httpx.Limits(max_connections=concurrency, max_keepalive_connections=concurrency)
        return httpx.AsyncClient(limits=limits)

    async def post(client):
        r = await client.post(url, content=BODY, headers={"content-type": "application/json"})
        r.raise_for_status()

    return run_async(concurrency, warmup, duration, make_client, post)


def w_aiohttp(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    import aiohttp

    async def make_client():
        return aiohttp.ClientSession(connector=aiohttp.TCPConnector(limit=concurrency))

    async def post(client):
        async with client.post(url, data=BODY, headers={"content-type": "application/json"}) as r:
            r.raise_for_status()
            await r.read()

    return run_async(concurrency, warmup, duration, make_client, post)


def mqb_publisher(url: str, concurrency: int):
    from mq_bridge import Publisher

    return Publisher.from_config(
        {"http": {"url": url, "method": "POST", "batch_concurrency": concurrency}}
    )


def w_mqb_send_threads(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    publisher = mqb_publisher(url, concurrency)
    return measure_threads([lambda: publisher.send(BODY)] * concurrency, warmup, duration)


def w_mqb_send_batch(url: str, concurrency: int, warmup: float, duration: float) -> tuple[int, float]:
    publisher = mqb_publisher(url, concurrency)
    batch = [BODY] * max(256, concurrency * 16)

    def run(deadline: float) -> int:
        n = 0
        while time.perf_counter() < deadline:
            publisher.send_batch(batch)
            n += len(batch)
        return n

    return measure(run, warmup, duration)


WORKERS = {
    "requests": w_requests,
    "httpx-sync": w_httpx_sync,
    "httpx-async": w_httpx_async,
    "aiohttp": w_aiohttp,
    "mqb-send-threads": w_mqb_send_threads,
    "mqb-send-batch": w_mqb_send_batch,
}

MODULES = {
    "requests": "requests",
    "httpx-sync": "httpx",
    "httpx-async": "httpx",
    "aiohttp": "aiohttp",
    "mqb-send-threads": "mq_bridge",
    "mqb-send-batch": "mq_bridge",
}


def child(target: str, url: str, concurrency: int, duration: float, warmup: float) -> None:
    count, elapsed = WORKERS[target](url, concurrency, warmup, duration)
    print(json.dumps({"count": count, "elapsed": elapsed}))


def available(target: str) -> bool:
    try:
        __import__(MODULES[target])
        return True
    except ImportError:
        return False


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--concurrency", type=parse_csv_ints, default=[1, 16, 64])
    parser.add_argument("--duration", type=float, default=5.0)
    parser.add_argument("--warmup", type=float, default=1.0)
    parser.add_argument("--targets", default=",".join(TARGETS))
    parser.add_argument("--nginx-workers", type=int, default=4)
    parser.add_argument("--child", nargs=3, metavar=("TARGET", "URL", "CONCURRENCY"), help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.child:
        target, url, concurrency = args.child
        child(target, url, int(concurrency), args.duration, args.warmup)
        return

    if not shutil.which("nginx"):
        raise SystemExit("nginx not found on PATH")
    targets = [t for t in args.targets.split(",") if t]
    proc, port, root = start_nginx(args.nginx_workers)
    url = f"http://{HOST}:{port}/ingest"
    results: dict[tuple[str, int], float] = {}
    try:
        for concurrency in args.concurrency:
            for target in targets:
                if not available(target):
                    print(f"skip {target}: not installed", file=sys.stderr)
                    continue
                out = subprocess.run(
                    [sys.executable, __file__, "--duration", str(args.duration), "--warmup", str(args.warmup),
                     "--child", target, url, str(concurrency)],
                    capture_output=True, text=True, env=os.environ.copy(),
                )
                if out.returncode != 0:
                    print(f"{target} c={concurrency} failed:\n{out.stderr[-2000:]}", file=sys.stderr)
                    continue
                data = json.loads(out.stdout.strip().splitlines()[-1])
                rps = data["count"] / data["elapsed"]
                results[(target, concurrency)] = rps
                print(f"{target:18} c={concurrency:<4} {rps:>10,.0f} req/s", flush=True)
    finally:
        proc.terminate()
        proc.wait(timeout=5)
        shutil.rmtree(root, ignore_errors=True)

    print()
    print("| client | " + " | ".join(f"c={c}" for c in args.concurrency) + " |")
    print("| :--- | " + " | ".join("---:" for _ in args.concurrency) + " |")
    for target in targets:
        cells = [results.get((target, c)) for c in args.concurrency]
        print(f"| {target} | " + " | ".join(f"{v:,.0f}" if v else "–" for v in cells) + " |")


if __name__ == "__main__":
    main()
