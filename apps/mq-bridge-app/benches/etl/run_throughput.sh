#!/usr/bin/env bash
# Scenarios 1 & 3 — bulk-insert throughput and batched-vs-unbatched, run through
# the app's zero-code `copy` ETL command (postgres table -> postgres table),
# wall-clocked over an exact MSG_COUNT rows. rows/sec = MSG_COUNT / elapsed.
#
# Prereqs:  ./seed.sh up   (Docker Postgres)   and a lean build:
#           cargo build -p mq-bridge-app --no-default-features --features bench --release
#
# Matrix (override via env): PAYLOADS, BATCHES, CONCURRENCIES.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/seed.sh"   # also sources lib.sh

PAYLOADS="${PAYLOADS:-256 4096}"        # bytes per JSON row
BATCHES="${BATCHES:-1 128}"
CONCURRENCIES="${CONCURRENCIES:-1 4}"
REPEATS="${REPEATS:-2}"                 # timed runs per cell (1 warmup + REPEATS, median/stddev reported)

RESULTS_DIR="${RESULTS_DIR:-$HERE/results}"
mkdir -p "$RESULTS_DIR"
CSV="$RESULTS_DIR/throughput.csv"
echo "payload_bytes,batch,concurrency,rows,repeats,median_elapsed_s,stddev_elapsed_s,median_rows_per_s" > "$CSV"

now() { python3 -c 'import time; print(time.time())'; }
COPY_TIMEOUT="${COPY_TIMEOUT:-900}"   # hard cap per copy run (s), guards a stuck route
copy_guarded() { run_guarded "$COPY_TIMEOUT" "$BIN" copy "$@"; }

require_bin
wait_for_pg

run_one() {
  local bytes="$1" batch="$2" conc="$3"
  local src="src_${bytes}" dst="dst_${bytes}"
  # Non-destructive incremental read paging on the monotonic `id` column (the
  # sqlx cursor reader) — the ETL "read a source table" path, akin to an Airbyte
  # incremental sync. (`delete_after_read` instead expects a queue-shaped table.)
  local from="${PG_URL}?table=${src}&cursor_column=id&sslmode=disable"
  # `columns=auto` writes the row's fields into the table's columns; without it the
  # sink stores the whole message in `payload`, the layout of a queue table.
  local to="${PG_URL}?table=${dst}&columns=auto&sslmode=disable"

  # Warmup pre-roll (discarded, not timed): primes the connection pool + caches.
  reset_dst "$dst" >/dev/null
  seed_source "$src" "$bytes" "$WARMUP_COUNT" >/dev/null
  copy_guarded --from "$from" --to "$to" --drain --batch-size "$batch" --concurrency "$conc" || true

  # REPEATS timed runs (a single sample is noise on a laptop, esp. on battery/
  # thermal throttling) — report median + stddev, the same statistic hyperfine
  # reports, instead of trusting one wall-clock sample.
  local -a elapsed_samples=()
  local i t0 t1 elapsed landed
  for ((i = 1; i <= REPEATS; i++)); do
    reset_dst "$dst" >/dev/null
    seed_source "$src" "$bytes" "$MSG_COUNT" >/dev/null
    t0="$(now)"
    copy_guarded --from "$from" --to "$to" --drain --batch-size "$batch" --concurrency "$conc" || true
    t1="$(now)"
    landed="$(psql_q -c "SELECT count(*) FROM ${dst};")"
    if [[ "$landed" != "$MSG_COUNT" ]]; then
      echo "  WARNING: run $i landed ${landed} != expected ${MSG_COUNT}" >&2
    fi
    elapsed="$(python3 -c "print(f'{$t1-$t0:.6f}')")"
    elapsed_samples+=("$elapsed")
  done

  local median stddev rate
  read -r median stddev <<<"$(python3 -c "
import statistics as s
xs=[${elapsed_samples[*]/%/,}]
print(f'{s.median(xs):.3f} {s.pstdev(xs):.3f}' if len(xs) > 1 else f'{xs[0]:.3f} 0.000')
")"
  rate="$(python3 -c "print(int($MSG_COUNT/$median))")"
  printf '%s,%s,%s,%s,%s,%s,%s,%s\n' "$bytes" "$batch" "$conc" "$MSG_COUNT" "$REPEATS" "$median" "$stddev" "$rate" | tee -a "$CSV"
}

echo "# throughput matrix: payloads=[$PAYLOADS] batches=[$BATCHES] concurrency=[$CONCURRENCIES] rows=$MSG_COUNT"
for bytes in $PAYLOADS; do
  for batch in $BATCHES; do
    for conc in $CONCURRENCIES; do
      echo "-- ${bytes}B batch=${batch} conc=${conc}"
      run_one "$bytes" "$batch" "$conc"
    done
  done
done
echo "done -> $CSV"
