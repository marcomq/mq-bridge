#!/usr/bin/env python3
"""Upper bound: the same lookups straight from pgbench, without mq-bridge.

Same table and keys as run.py / run_file.py. Two client locations:
- `port`:  a second container connects to host.docker.internal:55432, crossing Docker's port
           forwarding like the bridge on the Mac does.
- `in_pg`: pgbench inside the pg container over 127.0.0.1, without the forwarding.
Keys/s = transactions/s × keys per query. Results append to $RESULTS/pgbench.csv.
"""
import argparse
import csv
import os
import random
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / os.environ.get("RESULTS", "results/current")
SELECT = "SELECT path, id, name, tier FROM users WHERE path"
KEY = "'/u/' || lpad(:k{i}::text, 6, '0')"

# name: (keys per query, pgbench protocol). `prepared` matches a cached statement, `extended`
# an unnamed one prepared on every call. pgbench caps bound variables at 255, so the larger
# IN lists use the simple protocol (literals in the query text).
QUERIES = {
    "single_prepared": (1, "prepared"),
    "single_extended": (1, "extended"),
    "in1_extended": (1, "extended"),
    "in16_extended": (16, "extended"),
    "in128_extended": (128, "extended"),
    "in512_simple": (512, "simple"),
    "in1000_simple": (1000, "simple"),
}
LOCATIONS = {
    "port": ("pgclient", "host.docker.internal", "55432"),
    "in_pg": ("pg", "127.0.0.1", "5432"),
}
CLIENTS = [1, 4, 16, 64]


def script(name):
    n, _ = QUERIES[name]
    sets = "".join(f"\\set k{i} random(0, 999999)\n" for i in range(n))
    if name.startswith("single"):
        return sets + f"{SELECT} = {KEY.format(i=0)};\n"
    return sets + f"{SELECT} IN ({', '.join(KEY.format(i=i) for i in range(n))});\n"


def compose(*args, **kw):
    return subprocess.run(["docker", "compose", *args], cwd=HERE, check=True,
                          capture_output=True, text=True, **kw)


def pgbench(location, name, clients, duration):
    service, host, port = LOCATIONS[location]
    _, protocol = QUERIES[name]
    out = compose("exec", "-T", "-e", "PGPASSWORD=bench", service,
                  "pgbench", "-n", "-h", host, "-p", port, "-U", "bench", "-M", protocol,
                  "-c", str(clients), "-j", str(min(clients, 4)), "-T", str(duration),
                  "-f", f"/tmp/{name}.sql", "bench").stdout
    if m := re.search(r"failed transactions: (\d+)", out):
        if int(m.group(1)):
            raise RuntimeError(f"{location} {name} c{clients}: {m.group(1)} failed transactions")
    return float(re.search(r"tps = ([\d.]+) \(without initial connection time\)", out).group(1))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=int, default=10, help="seconds per cell")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    compose("--profile", "pgbench", "up", "-d", "--wait", "--no-deps", "pgclient")
    for service in {s for s, _, _ in LOCATIONS.values()}:
        for name in QUERIES:
            compose("exec", "-T", service, "sh", "-c", f"cat > /tmp/{name}.sql", input=script(name))

    RESULTS.mkdir(parents=True, exist_ok=True)
    out = RESULTS / "pgbench.csv"
    new = not out.exists()
    rng = random.Random(args.seed)
    with open(out, "a", newline="") as f:
        w = csv.writer(f)
        if new:
            w.writerow(["location", "query", "clients", "rep", "tps", "keys_per_s"])
        for rep in range(1, args.reps + 1):
            cells = [(loc, q, c) for loc in LOCATIONS for q in QUERIES for c in CLIENTS]
            rng.shuffle(cells)
            for loc, q, c in cells:
                tps = pgbench(loc, q, c, args.duration)
                keys = tps * QUERIES[q][0]
                w.writerow([loc, q, c, rep, f"{tps:.0f}", f"{keys:.0f}"])
                f.flush()
                print(f"rep {rep} {loc:6} {q:16} c{c:<3} tps={tps:.0f} keys/s={keys:.0f}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
