#!/usr/bin/env python3
"""CPU cost per request of the $12 server challenge API, served by route.yaml (config only).

Each endpoint is loaded on its own with oha; the bridge's CPU time (user + sys) divided by the
requests answered gives µs/req, the figure the challenge leaderboard reports. The mix weights
them like one k6 user loop: feed + post + 0.15 like + 0.02 create.
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORK = HERE / ".work"
SEED = WORK / "challenge/seed/feed.db"
RESULTS = HERE / os.environ.get("RESULTS", "results/current")
APP = os.environ.get("MQB_APP", "mq-bridge-app")
PORT = 13000
BASE = f"http://127.0.0.1:{PORT}"

ENDPOINTS = {
    # name: (oha args, weight per user loop)
    "feed": (["--rand-regex-url", f"{BASE}/feed"], 1.0),
    "post": (["--rand-regex-url", f"{BASE}/posts/[1-9][0-9]{{4}}"], 1.0),
    "like": (["--rand-regex-url", f"{BASE}/posts/[1-9][0-9]{{4}}/like", "-m", "POST",
              "-H", "x-user-id: 7"], 0.15),
    "create": (["--rand-regex-url", f"{BASE}/posts", "-m", "POST", "-H", "x-user-id: 7",
                "-H", "x-username: bob", "-H", "content-type: application/json",
                "-d", '{"body":"bob says hi at 2026-10-31T12:00:00.000Z (VU 1, iter 1)"}'], 0.02),
}
VARIANTS = {
    # name: (route concurrency, sqlx pool size)
    "rc1_pool4": (1, 4),
    "rc4_pool4": (4, 4),
    "rc1_pool1": (1, 1),
}


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    parts = [float(p) for p in out.replace("-", ":").split(":")]
    seconds = 0.0
    for p in parts:
        seconds = seconds * 60 + p
    return seconds


def start(variant):
    concurrency, pool = VARIANTS[variant]
    db = WORK / "run.db"
    for suffix in ("", "-wal", "-shm"):
        Path(f"{db}{suffix}").unlink(missing_ok=True)
    shutil.copy(SEED, db)
    config = (HERE / "route.yaml").read_text()
    for key, value in {"__CONCURRENCY__": concurrency, "__PORT__": PORT,
                       "__SQLITE_PATH__": db, "__POOL__": pool}.items():
        config = config.replace(key, str(value))
    log = open(WORK / f"{variant}.log", "w")
    proc = subprocess.Popen([APP, "--config-str", config, "--color", "never"],
                            cwd=WORK, stdout=log, stderr=subprocess.STDOUT)
    deadline = time.time() + 20
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"{APP} exited with {proc.returncode}, see {log.name}")
        try:
            socket.create_connection(("127.0.0.1", PORT), 0.2).close()
            return proc
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"{APP} did not open its port")


def oha(args, duration, connections):
    cmd = ["oha", "--no-tui", "--output-format", "json", "-z", duration, "-c", str(connections), *args]
    return json.loads(subprocess.run(cmd, check=True, capture_output=True, text=True).stdout)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--warmup", default="3s")
    ap.add_argument("--connections", type=int, default=64)
    ap.add_argument("--only", nargs="*", help="limit to these variants")
    args = ap.parse_args()
    RESULTS.mkdir(parents=True, exist_ok=True)

    rows = []
    for variant in args.only or list(VARIANTS):
        proc = start(variant)
        try:
            us = {}
            for name, (oha_args, weight) in ENDPOINTS.items():
                oha(oha_args, args.warmup, args.connections)
                before = cpu_seconds(proc.pid)
                result = oha(oha_args, args.duration, args.connections)
                spent = cpu_seconds(proc.pid) - before
                summary = result["summary"]
                done = sum(result["statusCodeDistribution"].values())
                ok = sum(n for code, n in result["statusCodeDistribution"].items() if code.startswith("2"))
                us[name] = spent / done * 1e6
                p99 = result["latencyPercentiles"]["p99"] * 1000
                rows.append((variant, name, us[name], summary["requestsPerSec"], p99, ok / done))
                (RESULTS / f"{variant}__{name}.json").write_text(json.dumps(result))
                print(f"{variant:10} {name:7} {us[name]:7.1f} µs/req  {summary['requestsPerSec']:8.0f} req/s  "
                      f"p99 {p99:6.1f} ms  2xx {ok / done:.1%}", flush=True)
            weights = {n: w for n, (_, w) in ENDPOINTS.items()}
            mix = sum(us[n] * w for n, w in weights.items()) / sum(weights.values())
            print(f"{variant:10} mix     {mix:7.1f} µs/req  -> ~{1e6 / mix:,.0f} req/s on one core", flush=True)
            rows.append((variant, "mix", mix, 1e6 / mix, None, None))
        finally:
            proc.terminate()
            proc.wait(10)
    (RESULTS / "summary.json").write_text(json.dumps(rows, indent=1))


if __name__ == "__main__":
    sys.exit(main())
