#!/usr/bin/env bash
# Scenario 2 — CDC event-to-sink latency via the zero-code config path.
#
# Starts the app on cdc_latency.yaml, applies the config (POST /config — the same
# action as clicking Save in the UI, which starts the postgres_cdc -> null route),
# inserts rows into the captured table, waits for the engine to process them, then
# reads the `queue_message_processing_duration_seconds` summary quantiles as the
# p50/p95/p99 latency proxy. See METHODOLOGY.md for what this measures.
#
# Prereqs:  ./seed.sh up   and a lean build with CDC:
#           cargo build -p mq-bridge-app --no-default-features --features bench-cdc --release
#           (`bench-cdc`, not `bench` — CDC needs the postgres logical-replication
#           endpoint, which plain `bench` leaves out.)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/seed.sh"   # sources lib.sh

CONFIG="$HERE/cdc_latency.yaml"
UI_ADDR="${UI_ADDR:-127.0.0.1:9091}"
CDC_TABLE="${CDC_TABLE:-cdc_src}"
CDC_PUB="${CDC_PUB:-mqb_pub}"
PAYLOADS="${PAYLOADS:-256 4096}"
RESULTS_DIR="${RESULTS_DIR:-$HERE/results}"
mkdir -p "$RESULTS_DIR"
CSV="$RESULTS_DIR/cdc_latency.csv"
echo "payload_bytes,count,p50_s,p95_s,p99_s" > "$CSV"

require_bin

APP_PID=""
trap 'kill_pids "$APP_PID"' EXIT

boot_app() {
  APP_PID="$(start_app "$CONFIG" "$RESULTS_DIR/cdc_app.log")"
  wait_health "$UI_ADDR" "$RESULTS_DIR/cdc_app.log"
  # cdc_latency.yaml is loaded via --config, but consumers do NOT auto-start
  # headless — this spawns the postgres_cdc -> null route (same as clicking
  # Start in the UI; POST /config only validates+saves, it never starts routes).
  start_consumer "$UI_ADDR" cdc_lat
  sleep 3
}

# Insert `n` rows as `n` SEPARATE single-row transactions (a server-side plpgsql
# loop with per-row COMMIT). This matters for CDC: one multi-row transaction is
# delivered as a *single* change message, so a bulk INSERT would yield ~one
# latency sample. N single-row commits yield N change events, each with its own
# processing-latency sample feeding the summary quantiles.
insert_rows() {
  local n="$1" bytes="$2"
  psql_q <<SQL
CREATE OR REPLACE PROCEDURE mqb_bench_insert(cnt int, bytes int)
LANGUAGE plpgsql AS \$\$
DECLARE g int;
BEGIN
  FOR g IN 1..cnt LOOP
    INSERT INTO ${CDC_TABLE} (payload, ins_ts)
    VALUES (rpad(format('{"id":%s,"pad":"', g), bytes - 2, 'x') || '"}',
            extract(epoch FROM clock_timestamp()));
    COMMIT;
  END LOOP;
END \$\$;
CALL mqb_bench_insert(${n}, ${bytes});
SQL
}

# Sum of the input-side processed counter across the CDC route.
processed_count() {
  curl -s "http://${METRICS_ADDR}/metrics" | awk -F' ' '
    /^queue_messages_processed_total/ && /endpoint="input"/ { s+=$NF } END { print int(s) }'
}

# Wait until the processed counter STOPS increasing (drained), rather than
# reaching an exact row count: a multi-row CDC transaction can be delivered as
# fewer processed-messages than rows, so counting to the row total may never
# converge. Settles when the counter is unchanged for `stable_needed` polls.
wait_drain() {
  local stable_needed=4 poll_s=2 tries=90
  local last=-1 stable=0
  while ((tries--)); do
    local n; n="$(processed_count)"
    if [[ "${n:-0}" -eq "$last" && "${n:-0}" -gt 0 ]]; then
      # Assignment, not `((stable++))`: see kill_pids in lib.sh — a post-increment
      # at 0 exits 1 and would kill this under `set -e`.
      stable=$((stable + 1)); ((stable >= stable_needed)) && return 0
    else
      stable=0; last="${n:-0}"
    fi
    sleep "$poll_s"
  done
  echo "  WARNING: processed counter did not settle (last=${last})" >&2
  return 1
}

run_one() {
  local bytes="$1"
  insert_rows "$WARMUP_COUNT" "$bytes" >/dev/null      # pre-roll (primes the stream)
  wait_drain || echo "  WARNING: ${bytes}B warmup drain did not settle" >&2
  insert_rows "$MSG_COUNT" "$bytes" >/dev/null          # measured
  if ! wait_drain; then
    echo "  SKIP: ${bytes}B measured drain did not settle — not recording quantiles" >&2
    return 0
  fi
  local p50 p95 p99
  p50="$(scrape_quantile queue_message_processing_duration_seconds 0.5)"
  p95="$(scrape_quantile queue_message_processing_duration_seconds 0.95)"
  p99="$(scrape_quantile queue_message_processing_duration_seconds 0.99)"
  printf '%s,%s,%s,%s,%s\n' "$bytes" "$MSG_COUNT" "$p50" "$p95" "$p99" | tee -a "$CSV"
}

wait_for_pg
seed_cdc "$CDC_TABLE" "$CDC_PUB"
boot_app
for bytes in $PAYLOADS; do
  echo "-- cdc latency ${bytes}B"
  run_one "$bytes"
done
echo "done -> $CSV"
