#!/usr/bin/env python3
"""Lookup latency benchmark: per-message vs batched `lookup` behind an HTTP input.

Every request is `POST /u/<6 random digits>`; the route enriches it from Postgres and answers
with the enriched payload. Raw oha JSON per measurement lands in $RESULTS/http/.
"""
import argparse
import json
import os
import random
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / os.environ.get("RESULTS", "results/current")
RAW = RESULTS / "http"
WORK = HERE / ".work"
PORT = 18080
URL = f"http://127.0.0.1:{PORT}/u/[0-9]{{6}}"
PG_URL = "postgres://bench:bench@127.0.0.1:55432/bench"

SELECT = "SELECT path, id, name, tier FROM users WHERE path"
VARIANTS = {
    # name: (where clause, lookup concurrency, route concurrency, `parallel` lanes or None,
    #        input `buffer` max_delay_ms or None)
    "per_message_c16": ("= ${metadata:key}", 16, 1, None, None),
    "per_message_c64": ("= ${metadata:key}", 64, 1, None, None),
    "batched_rc1": ("IN (${metadata:key})", 16, 1, None, None),
    "batched_rc4": ("IN (${metadata:key})", 16, 4, None, None),
    "per_message_c16_p8": ("= ${metadata:key}", 16, 1, 8, None),
    "batched_p8": ("IN (${metadata:key})", 16, 1, 8, None),
    "batched_p8_buf1": ("IN (${metadata:key})", 16, 1, 8, 1),
    "batched_p8_buf2": ("IN (${metadata:key})", 16, 1, 8, 2),
}
# netem adds timer overhead: 300us measured as +0.49 ms on a pgbench `SELECT 1` (0.047 -> 0.533 ms).
NETS = {"loopback": None, "rtt+0.5ms": "300us"}
CLOSED = [1, 8, 64, 256]
RATES = [1000, 2500, 5000, 10000, 20000, 40000]
OPEN_CONNECTIONS = 256


def sh(*cmd, check=True, capture=True, timeout=600):
    return subprocess.run(cmd, check=check, capture_output=capture, text=True, cwd=HERE, timeout=timeout)


def route_yaml(variant):
    where, lookup_c, route_c, lanes, buf = VARIANTS[variant]
    marker = f"\n        - parallel: {{ max_in_flight: {lanes} }}" if lanes else ""
    if buf:
        marker += f"\n        - buffer: {{ max_messages: 512, max_delay_ms: {buf} }}"
    return f"""log_level: warn
routes:
  lookup_bench:
    concurrency: {route_c}
    batch_size: 512
    input:
      http:
        url: "127.0.0.1:{PORT}"
      middlewares:
        - lookup:
            from:
              sqlx:
                url: "{PG_URL}"
                table: users
                max_connections: 64
                lookup_query: "{SELECT} {where}"
            # lookup strips http_path from the request it sends to `from`; pass it on as `key`.
            metadata:
              key: "${{metadata:http_path}}"
            into: user
            concurrency: {lookup_c}{marker}
    output:
      response: {{}}
"""


def set_netem(delay):
    sh("docker", "compose", "exec", "-T", "netem", "tc", "qdisc", "del", "dev", "eth0", "root", check=False)
    if delay:
        sh("docker", "compose", "exec", "-T", "netem", "tc", "qdisc", "add", "dev", "eth0", "root", "netem", "delay", delay)


def wait_port(proc, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"mq-bridge-app exited with {proc.returncode}")
        try:
            socket.create_connection(("127.0.0.1", PORT), 0.2).close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("mq-bridge-app did not open its port")


def start_bridge(variant):
    WORK.mkdir(exist_ok=True)
    log = open(WORK / f"{variant}.log", "a")
    proc = subprocess.Popen(
        ["mq-bridge-app", "--config-str", route_yaml(variant), "--color", "never"],
        cwd=WORK, stdout=log, stderr=subprocess.STDOUT,
    )
    wait_port(proc)
    return proc


def oha(duration, connections, rate=None):
    cmd = ["oha", "--no-tui", "--output-format", "json", "--rand-regex-url", URL,
           "-m", "POST", "-H", "content-type: application/json", "-d", '{"event":"view"}',
           "-z", duration, "-c", str(connections)]
    if rate:
        cmd += ["-q", str(rate), "--latency-correction"]
    return json.loads(sh(*cmd).stdout)


def check_once():
    out = subprocess.run(["curl", "-s", "-m", "10", "-X", "POST", "-d", '{"event":"view"}',
                          f"http://127.0.0.1:{PORT}/u/000042"], capture_output=True, text=True,
                         timeout=15).stdout
    if '"user_42"' not in out:
        raise RuntimeError(f"unexpected response: {out!r}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--warmup", default="5s")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--only", nargs="*", help="limit to these variants")
    ap.add_argument("--nets", nargs="*", choices=list(NETS), default=list(NETS))
    ap.add_argument("--closed", nargs="*", type=int, default=CLOSED, help="client counts")
    ap.add_argument("--rates", nargs="*", type=int, default=RATES, help="open-load req/s")
    args = ap.parse_args()

    RAW.mkdir(parents=True, exist_ok=True)
    variants = args.only or list(VARIANTS)
    rng = random.Random(args.seed)
    for rep in range(1, args.reps + 1):
        # Randomised order per repetition spreads slow drift (thermal, background load) evenly.
        cells = [(n, v) for n in args.nets for v in variants]
        rng.shuffle(cells)
        for net, variant in cells:
            set_netem(NETS[net])
            proc = start_bridge(variant)
            try:
                check_once()
                oha(args.warmup, 64)
                points = [("closed", c) for c in args.closed] + [("open", r) for r in args.rates]
                for mode, load in points:
                    if mode == "closed":
                        result = oha(args.duration, load)
                    else:
                        result = oha(args.duration, OPEN_CONNECTIONS, rate=load)
                    name = f"{net}__{variant}__{mode}{load}__r{rep}.json"
                    (RAW / name).write_text(json.dumps(result))
                    print(f"rep {rep} {net:10} {variant:16} {mode:6} {load:>6}  "
                          f"rps={result['summary']['requestsPerSec']:.0f}", flush=True)
            finally:
                proc.terminate()
                proc.wait(10)
    set_netem(None)


if __name__ == "__main__":
    sys.exit(main())
