"""Native-load HTTP benchmark for mq-bridge vs other Python frameworks.

This harness drives each server with an external, non-Python load generator
(``wrk``) instead of an in-process ``http.client`` loop. An in-process Python
client is GIL-bound and plateaus around ~13k req/s regardless of the server
under test, which makes every framework look the same and hides mq-bridge's
real throughput. ``wrk`` removes that client-side ceiling so the numbers
reflect the server, not the load tool.

Usage:
    uv run python analysis/bench_http_native.py --connections 1,8,32
    uv run python analysis/bench_http_native.py --targets mqb-worker,mqb-direct,faststream

Requires ``wrk`` on PATH (``brew install wrk``). The FastAPI/Starlette/Sanic/
aiohttp/FastStream targets additionally require those packages to be importable.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
from contextlib import contextmanager
from pathlib import Path

HOST = "127.0.0.1"
PATH = "/bench"
KIND = "bench.tick"
BODY = '{"value":0}'

REQ_RE = re.compile(r"Requests/sec:\s*([0-9.]+)")
LAT_RE = re.compile(r"Latency\s+([0-9.]+\w+)")


def parse_csv_ints(value: str) -> list[int]:
    out = [int(p.strip()) for p in value.split(",") if p.strip()]
    if not out or any(v < 1 for v in out):
        raise argparse.ArgumentTypeError("expected positive integers")
    return out


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--connections", type=parse_csv_ints, default=[1, 8, 32])
    parser.add_argument("--duration", type=int, default=5, help="wrk duration seconds")
    parser.add_argument(
        "--targets",
        default="mqb-worker,mqb-direct,faststream,fastapi,starlette,sanic,aiohttp",
        help=(
            "Comma-separated subset of: mqb-worker, mqb-direct, faststream, "
            "fastapi, starlette, sanic, aiohttp."
        ),
    )
    parser.add_argument("--route-concurrency", type=int, default=8)
    args = parser.parse_args()
    if args.duration <= 0:
        parser.error("--duration must be a positive integer")
    return args


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((HOST, 0))
        return int(sock.getsockname()[1])


def wait_for_port(port: int, timeout: float = 15.0) -> None:
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        try:
            with socket.create_connection((HOST, port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.02)
    raise TimeoutError(f"server on {HOST}:{port} did not become ready")


def wrk_lua(send_kind: bool) -> str:
    lines = [
        'wrk.method = "POST"',
        f"wrk.body = '{BODY}'",
        'wrk.headers["Content-Type"] = "application/json"',
    ]
    if send_kind:
        lines.append(f'wrk.headers["kind"] = "{KIND}"')
    return "\n".join(lines) + "\n"


def run_wrk(port: int, connections: int, duration: int, lua_path: str) -> tuple[float, str]:
    threads = min(connections, os.cpu_count() or 8)
    out = subprocess.run(
        [
            "wrk",
            f"-t{threads}",
            f"-c{connections}",
            f"-d{duration}s",
            "-s",
            lua_path,
            f"http://{HOST}:{port}{PATH}",
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout
    req = REQ_RE.search(out)
    lat = LAT_RE.search(out)
    return (float(req.group(1)) if req else 0.0, lat.group(1) if lat else "?")


@contextmanager
def mqb_server(executor: str, route_concurrency: int):
    port = free_port()
    cfg = textwrap.dedent(
        f"""
        routes:
          http_bench:
            concurrency: {route_concurrency}
            batch_size: 128
            input:
              http:
                url: "{HOST}:{port}"
                path: "{PATH}"
                method: "POST"
                internal_buffer_size: 8192
                request_timeout_ms: 30000
                concurrency_limit: 512
                inline_response_fast_path: true
            output:
              response: {{}}
        """
    ).lstrip()
    # Route.from_file expects a path, so write the YAML to a temp dir and pass the
    # path. Track the dir so it can be removed once the child exits.
    cfg_dir = Path(tempfile.mkdtemp(prefix="mqb-native-"))
    cfg_path = cfg_dir / "route.yaml"
    cfg_path.write_text(cfg, encoding="utf-8")
    launcher = textwrap.dedent(
        f"""
        from mq_bridge import Route
        route = Route.from_file({str(cfg_path)!r}, "http_bench")
        route.add_handler("{KIND}", lambda data: {{"value": data["value"] + 1}})
        route.run()
        """
    ).lstrip()
    env = os.environ.copy()
    if executor == "direct":
        env["MQ_BRIDGE_PY_HANDLER_EXECUTOR"] = "direct"
    else:
        env.pop("MQ_BRIDGE_PY_HANDLER_EXECUTOR", None)
    proc = subprocess.Popen([sys.executable, "-c", launcher], env=env)
    try:
        wait_for_port(port)
        yield port, True  # send_kind=True
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(cfg_dir, ignore_errors=True)


@contextmanager
def faststream_server():
    port = free_port()
    app = textwrap.dedent(
        """
        import json
        from faststream.asgi import AsgiFastStream

        async def bench(scope, receive, send):
            if scope["type"] != "http" or scope["method"] != "POST":
                await send({"type": "http.response.start", "status": 404, "headers": []})
                await send({"type": "http.response.body", "body": b""})
                return
            body = bytearray(); more = True
            while more:
                e = await receive(); body.extend(e.get("body", b"")); more = e.get("more_body", False)
            data = json.loads(body); data["value"] += 1
            rb = json.dumps(data, separators=(",", ":")).encode()
            await send({"type": "http.response.start", "status": 200,
                        "headers": [(b"content-type", b"application/json")]})
            await send({"type": "http.response.body", "body": rb})

        app = AsgiFastStream(asgi_routes=[("/bench", bench)], logger=None)
        """
    ).lstrip()
    tmp = Path(tempfile.mkdtemp(prefix="mqb-fs-"))
    (tmp / "fs_app.py").write_text(app, encoding="utf-8")
    proc = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "fs_app:app", "--host", HOST,
         "--port", str(port), "--log-level", "warning"],
        cwd=str(tmp),
        # Suppress the cosmetic lifespan-teardown traceback uvicorn/FastStream
        # prints when terminated mid-run; it does not affect the measurement.
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for_port(port)
        yield port, False  # send_kind=False
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


@contextmanager
def _spawn_python(source: str, prefix: str, port: int):
    """Write ``source`` to a temp module and run it as its own process."""
    tmp = Path(tempfile.mkdtemp(prefix=prefix))
    script = tmp / "server.py"
    script.write_text(textwrap.dedent(source).lstrip(), encoding="utf-8")
    proc = subprocess.Popen(
        [sys.executable, str(script)],
        cwd=str(tmp),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for_port(port)
        yield port, False  # send_kind=False (only mq-bridge routes on the kind header)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


@contextmanager
def _uvicorn_asgi(app_source: str, prefix: str):
    """Run an ASGI app (exposed as ``app``) under uvicorn, one worker."""
    port = free_port()
    tmp = Path(tempfile.mkdtemp(prefix=prefix))
    (tmp / "app_mod.py").write_text(textwrap.dedent(app_source).lstrip(), encoding="utf-8")
    proc = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "app_mod:app", "--host", HOST,
         "--port", str(port), "--log-level", "warning"],
        cwd=str(tmp),
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for_port(port)
        yield port, False
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def fastapi_server():
    return _uvicorn_asgi(
        f"""
        from fastapi import FastAPI, Request
        from fastapi.responses import JSONResponse

        app = FastAPI()

        @app.post("{PATH}")
        async def bench(req: Request):
            data = await req.json()
            data["value"] += 1
            return JSONResponse(data)
        """,
        prefix="mqb-fastapi-",
    )


def starlette_server():
    return _uvicorn_asgi(
        f"""
        from starlette.applications import Starlette
        from starlette.responses import JSONResponse
        from starlette.routing import Route

        async def bench(request):
            data = await request.json()
            data["value"] += 1
            return JSONResponse(data)

        app = Starlette(routes=[Route("{PATH}", bench, methods=["POST"])])
        """,
        prefix="mqb-starlette-",
    )


def sanic_server():
    port = free_port()
    return _spawn_python(
        f"""
        from sanic import Sanic
        from sanic.response import json as sjson

        app = Sanic("bench")

        @app.post("{PATH}")
        async def bench(request):
            data = request.json
            data["value"] += 1
            return sjson(data)

        if __name__ == "__main__":
            app.run(host="{HOST}", port={port}, access_log=False,
                    single_process=True, motd=False)
        """,
        prefix="mqb-sanic-",
        port=port,
    )


def aiohttp_server():
    port = free_port()
    return _spawn_python(
        f"""
        from aiohttp import web

        async def bench(request):
            data = await request.json()
            data["value"] += 1
            return web.json_response(data)

        app = web.Application()
        app.add_routes([web.post("{PATH}", bench)])
        web.run_app(app, host="{HOST}", port={port}, print=None, access_log=None)
        """,
        prefix="mqb-aiohttp-",
        port=port,
    )


TARGETS = {
    "mqb-worker": lambda a: mqb_server("worker", a.route_concurrency),
    "mqb-direct": lambda a: mqb_server("direct", a.route_concurrency),
    "faststream": lambda a: faststream_server(),
    "fastapi": lambda a: fastapi_server(),
    "starlette": lambda a: starlette_server(),
    "sanic": lambda a: sanic_server(),
    "aiohttp": lambda a: aiohttp_server(),
}


def main() -> None:
    if shutil.which("wrk") is None:
        raise SystemExit("wrk not found on PATH (brew install wrk)")
    args = parse_args()
    targets = [t.strip() for t in args.targets.split(",") if t.strip()]
    for t in targets:
        if t not in TARGETS:
            raise SystemExit(f"unknown target '{t}'; choices: {', '.join(TARGETS)}")

    results: dict[str, dict[int, float]] = {}
    for target in targets:
        results[target] = {}
        with TARGETS[target](args) as (port, send_kind):
            lua = Path(tempfile.mkdtemp(prefix="mqb-wrk-")) / "post.lua"
            lua.write_text(wrk_lua(send_kind), encoding="utf-8")
            # warmup
            run_wrk(port, max(args.connections), 2, str(lua))
            for conn in args.connections:
                rps, lat = run_wrk(port, conn, args.duration, str(lua))
                results[target][conn] = rps
                print(f"{target:14s} c={conn:<4d} {rps:12,.0f} req/s  (lat {lat})", flush=True)

    print("\nreq/s by connections (native wrk load):")
    header = "target".ljust(14) + "".join(f"{c:>14d}" for c in args.connections)
    print(header)
    for target in targets:
        row = target.ljust(14) + "".join(
            f"{results[target].get(c, 0):>14,.0f}" for c in args.connections
        )
        print(row)


if __name__ == "__main__":
    main()
