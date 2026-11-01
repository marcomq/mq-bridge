#!/usr/bin/env python3
"""CPU cost per request of the Rust handler prototype in server/, measured like run.py.

Writes carry a real JWT from the seed's tokens.json, so every like and create verifies HS256.
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

from run import FEED_IDS, HERE, MIN_OK, SEED, WORK, cpu_seconds, oha

BIN = os.environ.get("SERVER_BIN", str(HERE / "../../target/release/twelve-dollar-server"))
PORT = 13002
BASE = f"http://127.0.0.1:{PORT}"
TOKEN = json.loads((WORK / "challenge/seed/tokens.json").read_text())[6]["token"]
AUTH = ["-H", f"authorization: Bearer {TOKEN}"]
ENDPOINTS = {
    "feed": (["--rand-regex-url", f"{BASE}/feed"], 1.0),
    "post": (["--rand-regex-url", f"{BASE}/posts/{FEED_IDS}"], 1.0),
    "like": (["--rand-regex-url", f"{BASE}/posts/[1-9][0-9]{{4}}/like", "-m", "POST", *AUTH], 0.15),
    "create": (["--rand-regex-url", f"{BASE}/posts", "-m", "POST", *AUTH, "-H", "content-type: application/json",
                "-d", '{"body":"bob says hi at 2026-10-31T12:00:00.000Z (VU 1, iter 1)"}'], 0.02),
}


def start(env):
    db = WORK / "server.db"
    for suffix in ("", "-wal", "-shm"):
        Path(f"{db}{suffix}").unlink(missing_ok=True)
    shutil.copy(SEED, db)
    log = open(WORK / "server.log", "w")
    proc = subprocess.Popen([BIN], env={**os.environ, **env, "SQLITE_PATH": str(db), "PORT": str(PORT)},
                            stdout=log, stderr=subprocess.STDOUT)
    while True:
        if proc.poll() is not None:
            raise RuntimeError(f"{BIN} exited with {proc.returncode}, see {log.name}")
        try:
            socket.create_connection(("127.0.0.1", PORT), 0.2).close()
            return proc
        except OSError:
            time.sleep(0.1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", default="6s")
    ap.add_argument("--warmup", default="2s")
    ap.add_argument("--connections", type=int, default=64)
    ap.add_argument("--only", nargs="*", help="limit to these endpoints")
    args = ap.parse_args()
    proc = start({})
    try:
        us, failed = {}, []
        for name, (oha_args, _) in ENDPOINTS.items():
            if args.only and name not in args.only:
                continue
            oha(oha_args, args.warmup, args.connections)
            before = cpu_seconds(proc.pid)
            result = oha(oha_args, args.duration, args.connections)
            spent = cpu_seconds(proc.pid) - before
            codes = result["statusCodeDistribution"]
            done = sum(codes.values())
            ok = sum(n for code, n in codes.items() if code.startswith("2"))
            us[name] = spent / done * 1e6
            if ok / done < MIN_OK:
                failed.append(name)
            p99 = result["latencyPercentiles"]["p99"] * 1000
            print(f"{name:7} {us[name]:7.1f} µs/req  {result['summary']['requestsPerSec']:8.0f} req/s  "
                  f"p99 {p99:6.1f} ms  2xx {ok / done:.1%}", flush=True)
        if failed:
            print(f"mix     n/a, failed: {', '.join(failed)}", flush=True)
        elif len(us) == len(ENDPOINTS):
            mix = sum(us[n] * w for n, (_, w) in ENDPOINTS.items()) / sum(w for _, w in ENDPOINTS.values())
            print(f"mix     {mix:7.1f} µs/req  -> ~{1e6 / mix:,.0f} req/s on one core", flush=True)
    finally:
        proc.terminate()
        proc.wait(10)


if __name__ == "__main__":
    sys.exit(main())
