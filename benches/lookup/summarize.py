#!/usr/bin/env python3
"""Summarises $RESULTS (default results/current) into summary.md, plus http_summary.csv.

Reads http/*.json (run.py), file.csv (run_file.py) and pgbench.csv (pgbench.py), whichever
exist. Per cell: median across repetitions, with min-max in brackets.
"""
import csv
import json
import os
import statistics
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / os.environ.get("RESULTS", "results/current")
RAW = RESULTS / "http"


def load_cells():
    cells = defaultdict(list)
    for f in sorted(RAW.glob("*.json")):
        net, variant, point, _rep = f.stem.split("__")
        r = json.loads(f.read_text())
        ok = r["statusCodeDistribution"].get("200", 0)
        errors = sum(r["errorDistribution"].values()) - r["errorDistribution"].get("aborted due to deadline", 0)
        bad = sum(v for k, v in r["statusCodeDistribution"].items() if k != "200") + errors
        pc = r["latencyPercentiles"]
        cells[(net, variant, point)].append({
            "rps": ok / r["summary"]["total"],
            "p50": pc["p50"] * 1e3, "p99": pc["p99"] * 1e3, "p999": pc["p99.9"] * 1e3,
            "err_pct": 100 * bad / max(ok + bad, 1),
        })
    return cells


def point_key(point):
    mode = "closed" if point.startswith("closed") else "open"
    return (mode != "closed", int(point.removeprefix(mode)))


def fmt(values, digits):
    med = statistics.median(values)
    if len(values) == 1:
        return f"{med:.{digits}f}"
    return f"{med:.{digits}f} [{min(values):.{digits}f}–{max(values):.{digits}f}]"


def csv_section(name, title, group, value, sort):
    path = RESULTS / name
    if not path.exists():
        return []
    cells = defaultdict(list)
    for row in csv.DictReader(open(path)):
        cells[tuple(row[k] for k in group)].append(float(row[value]))
    out = [f"## {title}", "", "| " + " | ".join(group) + f" | {value} |",
           "|" + "---|" * (len(group) + 1)]
    for key in sorted(cells, key=sort):
        out.append("| " + " | ".join(key) + f" | {fmt(cells[key], 0)} |")
    return out + [""]


def main():
    out = ["Median across repetitions, [min–max].", ""]
    out += csv_section("pgbench.csv", "pgbench (upper bound)", ["location", "query", "clients"],
                       "keys_per_s", lambda k: (k[0], k[1], int(k[2])))
    out += csv_section("file.csv", "file -> lookup -> file", ["variant"], "rows_per_s",
                       lambda k: list(VARIANT_ORDER).index(k[0]) if k[0] in VARIANT_ORDER else 99)
    out += http_section()
    (RESULTS / "summary.md").write_text("\n".join(out))
    print("\n".join(out))


VARIANT_ORDER = ["no_lookup", "per_message_c16", "per_message_c64", "batched_b1", "batched_b16",
                 "batched_b128", "batched_b512", "batched_b1000"]


def http_section():
    if not RAW.exists():
        return []
    cells = load_cells()
    rows = []
    for (net, variant, point), reps in cells.items():
        rows.append({"net": net, "variant": variant, "point": point, "reps": len(reps),
                     **{k: statistics.median(r[k] for r in reps) for k in reps[0]},
                     "_reps": reps})
    rows.sort(key=lambda r: (r["net"], point_key(r["point"]), r["variant"]))

    with open(RESULTS / "http_summary.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["net", "variant", "point", "reps", "rps", "p50_ms", "p99_ms", "p999_ms", "err_pct"])
        for r in rows:
            w.writerow([r["net"], r["variant"], r["point"], r["reps"], f"{r['rps']:.0f}",
                        f"{r['p50']:.3f}", f"{r['p99']:.3f}", f"{r['p999']:.3f}", f"{r['err_pct']:.2f}"])

    out = ["## HTTP: single requests -> lookup -> response", "",
           "Latency in ms; `open` = fixed offered rate (req/s), `closed` = concurrent clients.", ""]
    for net in sorted({r["net"] for r in rows}):
        out += [f"### {net}", "", "| load | variant | achieved req/s | p50 | p99 | p99.9 | errors % |",
                "|---|---|---|---|---|---|---|"]
        for r in (r for r in rows if r["net"] == net):
            reps = r["_reps"]
            out.append(f"| {r['point']} | {r['variant']} | {fmt([x['rps'] for x in reps], 0)} | "
                       f"{fmt([x['p50'] for x in reps], 2)} | {fmt([x['p99'] for x in reps], 2)} | "
                       f"{fmt([x['p999'] for x in reps], 2)} | {fmt([x['err_pct'] for x in reps], 2)} |")
        out.append("")
    return out


if __name__ == "__main__":
    main()
