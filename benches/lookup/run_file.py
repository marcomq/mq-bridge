#!/usr/bin/env python3
"""Throughput benchmark: file -> lookup (on the output) -> file, per-message vs batched.

Each input line is `{"path": "/u/<6 random digits>", "event": "view"}`. Throughput is taken
from 1 % of the input written until 90 % or `--window` seconds, whichever comes first, so
startup and the last partial batch drop out and slow variants need not finish. Every written
line is checked afterwards: same key and order as the input, with the matching `user`.
"""
import argparse
import csv
import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / os.environ.get("RESULTS", "results/current")
WORK = HERE / ".work"
PG_URL = "postgres://bench:bench@127.0.0.1:55432/bench"
SELECT = "SELECT path, id, name, tier FROM users WHERE path"

VARIANTS = {
    # name: (where clause or None for no lookup, lookup concurrency, route batch size)
    "no_lookup": (None, 16, 512),
    "per_message_c16": ("= ${payload:path}", 16, 512),
    "per_message_c64": ("= ${payload:path}", 64, 512),
    "batched_b1": ("IN (${payload:path})", 16, 1),
    "batched_b16": ("IN (${payload:path})", 16, 16),
    "batched_b128": ("IN (${payload:path})", 16, 128),
    "batched_b512": ("IN (${payload:path})", 16, 512),
    "batched_b1000": ("IN (${payload:path})", 16, 1000),
}


def write_input(rows, seed):
    path = WORK / f"input_{rows}_{seed}.jsonl"
    if not path.exists():
        rng = random.Random(seed)
        with open(path, "w") as f:
            for _ in range(rows):
                f.write(f'{{"path":"/u/{rng.randrange(1_000_000):06d}","event":"view"}}\n')
    return path


def route_yaml(variant, src, dst):
    where, lookup_c, batch = VARIANTS[variant]
    lookup = "" if where is None else f"""
      middlewares:
        - lookup:
            from:
              sqlx:
                url: "{PG_URL}"
                table: users
                max_connections: 64
                lookup_query: "{SELECT} {where}"
            into: user
            concurrency: {lookup_c}"""
    return f"""log_level: warn
routes:
  lookup_file_bench:
    concurrency: 1
    batch_size: {batch}
    exit_on_empty: true
    input:
      file:
        path: "{src}"
        format: raw
    output:
      file:
        path: "{dst}"
        format: raw{lookup}
"""


class LineCounter:
    """Counts newlines in a growing file, reading only the bytes added since the last call."""

    def __init__(self, path):
        self.path, self.f, self.n = path, None, 0

    def __call__(self):
        if self.f is None:
            try:
                self.f = open(self.path, "rb")
            except FileNotFoundError:
                return 0
        while chunk := self.f.read(1 << 20):
            self.n += chunk.count(b"\n")
        return self.n


def measure(variant, src, rows, window, timeout):
    dst = WORK / f"out_{variant}.jsonl"
    dst.unlink(missing_ok=True)
    log = open(WORK / f"file_{variant}.log", "a")
    proc = subprocess.Popen(
        ["mq-bridge-app", "--no-ui", "--no-metrics", "--config-str", route_yaml(variant, src, dst),
         "--color", "never"],
        cwd=WORK, stdout=log, stderr=subprocess.STDOUT,
    )
    lines = LineCounter(dst)
    lo, hi = rows // 100, rows * 9 // 10
    t_lo = t_hi = None
    deadline = time.time() + timeout
    try:
        while time.time() < deadline and proc.poll() is None:
            n = lines()
            now = time.perf_counter()
            if t_lo is None and n >= lo:
                t_lo, n_lo = now, n
            if t_lo is not None and (n >= hi or now - t_lo >= window):
                t_hi, n_hi = now, n
                break
            time.sleep(0.01)
    finally:
        if proc.poll() is None:
            proc.terminate()
        proc.wait(10)
    if t_hi is None:
        raise RuntimeError(f"{variant}: only {lines()} of {rows} lines, see .work/file_{variant}.log")
    if t_hi - t_lo < 0.25:
        raise RuntimeError(f"{variant}: window {t_hi - t_lo:.2f}s is too short, raise --rows")
    return (n_hi - n_lo) / (t_hi - t_lo), dst


def verify(variant, src, dst):
    with open(dst, "rb") as fo:
        written = fo.read().split(b"\n")[:-1]
    with open(src) as fi:
        expected = [json.loads(next(fi))["path"] for _ in written]
    lookup = VARIANTS[variant][0] is not None
    for i, (line, path) in enumerate(zip(written, expected)):
        o = json.loads(line)
        if o["path"] != path:
            raise RuntimeError(f"{variant}: line {i} has key {o['path']}, input has {path}")
        if lookup and (not o.get("user") or o["user"].get("path") != path):
            raise RuntimeError(f"{variant}: line {i} lacks its user: {o}")
    return len(written)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows", type=int, default=1_000_000)
    ap.add_argument("--window", type=float, default=15, help="max seconds measured per run")
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--only", nargs="*", help="limit to these variants")
    args = ap.parse_args()

    WORK.mkdir(exist_ok=True)
    RESULTS.mkdir(parents=True, exist_ok=True)
    src = write_input(args.rows, args.seed)
    variants = args.only or list(VARIANTS)
    rng = random.Random(args.seed)
    out = RESULTS / "file.csv"
    new = not out.exists()
    with open(out, "a", newline="") as f:
        w = csv.writer(f)
        if new:
            w.writerow(["variant", "rows", "rep", "rows_per_s"])
        for rep in range(1, args.reps + 1):
            order = list(variants)
            rng.shuffle(order)
            for variant in order:
                rps, dst = measure(variant, src, args.rows, args.window, args.timeout)
                checked = verify(variant, src, dst)
                w.writerow([variant, checked, rep, f"{rps:.0f}"])
                f.flush()
                print(f"rep {rep} {variant:16} rows/s={rps:.0f}  checked {checked} lines", flush=True)


if __name__ == "__main__":
    sys.exit(main())
