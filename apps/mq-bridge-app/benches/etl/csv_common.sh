#!/usr/bin/env bash
# Shared setup for the CSV -> JSONL scenario. Sourced by run_csv_*.sh.
#
# Each tool has its own runnable script (run_csv_mqb.sh, run_csv_sling.sh,
# run_csv_meltano.sh) so a single tool can be re-measured on its own; they all
# append to the same results CSV, and run_csv_to_jsonl.sh chains them plus the
# parity check. Splitting them up is why this file exists: the dataset, the
# output paths and the watchdog budgets have to agree across the scripts, or the
# parity check would be comparing outputs from different runs.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/lib.sh"

ROWS="${ROWS:-1000000}"
SEED="${SEED:-42}"
# 2, not 5: on this hardware the median stops moving after the second timed run,
# so the extra three only cost wall-clock. Raise it if a machine looks noisy.
REPEATS="${REPEATS:-2}"
CSV="${CSV:-$HERE/data/bench.csv}"
SCHEMA="${SCHEMA:-$HERE/schemas/bench.json}"

MELTANO_PROJECT="$HERE/meltano_project/bench"
MELTANO_BIN="$HERE/meltano_project/.venv/bin/meltano"

RESULTS_DIR="${RESULTS_DIR:-$HERE/results}"
RESULTS_CSV="${RESULTS_CSV:-$RESULTS_DIR/csv_to_jsonl.csv}"

# Output paths are fixed and shared so the parity check can run as its own step,
# after the tools that produced the files have exited.
OUT_MQB="${OUT_MQB:-/tmp/mqb_csv_out.jsonl}"
OUT_MQB_RAW="${OUT_MQB_RAW:-/tmp/mqb_csv_out_raw.jsonl}"
OUT_SLING="${OUT_SLING:-/tmp/sling_csv_out.jsonl}"
OUT_DUCKDB="${OUT_DUCKDB:-/tmp/duckdb_csv_out.jsonl}"
OUT_MELTANO="$MELTANO_PROJECT/output/bench.jsonl"

# Roughly 20-100x the measured median at 1M rows: a healthy run never trips one,
# a wedged run fails in minutes. See [[bench-harness-timeout-contract]].
COPY_TIMEOUT="${COPY_TIMEOUT:-$(guard_budget 0.0002 60)}"          # ~200s at 1M rows
SLING_TIMEOUT="${SLING_TIMEOUT:-$(guard_budget 0.0004 60)}"        # ~400s at 1M rows
DUCKDB_TIMEOUT="${DUCKDB_TIMEOUT:-$(guard_budget 0.0002 60)}"        # ~200s at 1M rows
MELTANO_TIMEOUT="${MELTANO_TIMEOUT:-$(guard_budget 0.0006 120)}"   # ~600s at 1M rows

# (Re)generate the seeded CSV if missing or the wrong length (rows + header line).
ensure_csv() {
  local want_lines=$((ROWS + 1))
  local have_lines
  have_lines="$( [[ -f "$CSV" ]] && wc -l < "$CSV" | tr -d ' ' || echo 0 )"
  if [[ "$have_lines" != "$want_lines" ]]; then
    echo "generating $CSV ($ROWS rows, seed $SEED)"
    uv run --python 3.12 python3 "$HERE/gen_bench_data.py" --rows "$ROWS" --seed "$SEED" --out "$CSV"
  fi
}
