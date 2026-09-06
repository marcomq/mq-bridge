#!/usr/bin/env bash
# CSV -> JSONL, DuckDB only. Runnable on its own: ./run_csv_duckdb.sh
#
# DuckDB is the reference for "how fast can this machine turn this CSV into
# JSONL at all". It is not an ETL tool and offers none of the delivery semantics
# mq-bridge does — no at-least-once, no checkpointing, no arbitrary sinks — so
# this is a throughput ceiling, not a like-for-like product comparison.
#
# `all_varchar=true` matches the *untyped* mq-bridge column: every field stays
# the string the CSV reader produced. Without it DuckDB infers types, which is
# different work and belongs against the typed column instead.
#
# THREADS is the parameter to watch. DuckDB parallelises its CSV scan across
# cores while `run_csv_mqb.sh` runs at --concurrency 1, so the two are only
# comparable when this is stated. Both numbers are worth having:
#
#   THREADS=1 ./run_csv_duckdb.sh   # like-for-like against mq-bridge
#   ./run_csv_duckdb.sh             # this machine's ceiling (all cores)
#
# Skipped (not fatal) when uv is absent; duckdb is fetched ephemerally so the
# repo takes on no permanent dependency.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/csv_common.sh"

if ! command -v uvx >/dev/null 2>&1; then
  echo "-- duckdb: uvx not found, skipping (install uv to enable this baseline)" >&2
  exit 0
fi

ensure_csv

THREADS="${THREADS:-0}"   # 0 = DuckDB's own default (one thread per core)

run_duckdb_once() {
  rm -f "$OUT_DUCKDB"
  CSV="$CSV" OUT_DUCKDB="$OUT_DUCKDB" THREADS="$THREADS" uvx --from duckdb python - <<'PY'
import duckdb, os
con = duckdb.connect()
if int(os.environ["THREADS"]):
    con.execute(f"SET threads={os.environ['THREADS']}")
con.execute(
    "COPY (SELECT * FROM read_csv(?, header=true, all_varchar=true)) "
    "TO '" + os.environ["OUT_DUCKDB"] + "' (FORMAT JSON)",
    [os.environ["CSV"]],
)
PY
}

label="duckdb"
[[ "$THREADS" != "0" ]] && label="duckdb-t${THREADS}"
bench_tool "$label" "$OUT_DUCKDB" "$DUCKDB_TIMEOUT" run_duckdb_once

echo "output kept at $OUT_DUCKDB"
