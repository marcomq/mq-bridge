#!/usr/bin/env bash
# CSV -> JSONL: Redpanda Connect, and mq-bridge-app with the mq-bridge-connect
# plugin. Runnable on its own; run_csv_mqb.sh provides the native rows.
#
#   ./run_csv_connect.sh            # every cell below
#   ./run_csv_connect.sh rpc        # Redpanda Connect standalone only
#   ./run_csv_connect.sh plugin     # mq-bridge-app + plugin only
#   ./run_csv_connect.sh parity     # typed outputs vs. mq-bridge-app's (run_csv_mqb.sh)
#
#   redpanda-connect-untyped       connect/csv_untyped.yaml
#   redpanda-connect               connect/csv_typed.yaml (Bloblang mapping)
#   mq-bridge-app-bloblang         native file endpoints + `connect_mapping` middleware
#   mq-bridge-app-connect-untyped  Connect `file` input and output through the plugin
#   mq-bridge-app-connect          same, with the mapping in the input's pipeline
#
# Install (both skipped if absent):
#   curl -sL https://github.com/redpanda-data/connect/releases/download/v4.112.0/redpanda-connect_4.112.0_darwin_arm64.tar.gz \
#     | tar -xz -C benches/etl/bin redpanda-connect
#   brew install marcomq/tap/mq-bridge-connect
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/csv_common.sh"

MAPPING='root = this
root.id = this.id.int64()
root.attributes = this.attributes.parse_json()'

urlenc() { python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$1"; }

# Typed outputs are kept per label for the parity step.
out_for() { echo "${OUT_CONNECT%.jsonl}_$1.jsonl"; }

run_rpc_once() {
  rm -f "$out"
  CSV="$CSV" OUT="$out" "$RPC_BIN" run "$HERE/connect/csv_$1.yaml"
}

rpc_cells() {
  if [[ ! -x "$RPC_BIN" ]]; then
    echo "-- redpanda-connect: not found at $RPC_BIN, skipping" >&2; return 0
  fi
  out="$(out_for redpanda-connect-untyped)"
  bench_tool "redpanda-connect-untyped" "$out" "$CONNECT_TIMEOUT" run_rpc_once untyped
  rm -f "$out"
  out="$(out_for redpanda-connect)"
  bench_tool "redpanda-connect" "$out" "$CONNECT_TIMEOUT" run_rpc_once typed
}

run_mqb_once() {
  rm -f "$out"
  "$BIN" copy --plugin "$CONNECT_PLUGIN" --from "$from" --to "$to" \
    --drain --batch-size 1024 --concurrency 1
}

# Form B of the plugin's config: a Redpanda Connect document minus the other end.
connect_input() {
  printf 'input:\n  file:\n    paths: ["%s"]\n    scanner:\n      csv: {}\n' "$CSV"
  if [[ "$1" == typed ]]; then
    printf 'pipeline:\n  processors:\n    - mapping: |\n'
    sed 's/^/        /' <<<"$MAPPING"
  fi
}
connect_output() { printf 'output:\n  file:\n    path: %s\n    codec: lines\n' "$out"; }

plugin_cells() {
  require_bin
  if [[ ! -f "$CONNECT_PLUGIN" ]]; then
    echo "-- mq-bridge-connect: not found at $CONNECT_PLUGIN, skipping" >&2; return 0
  fi
  out="$(out_for mq-bridge-app-bloblang)"
  from="file://${CSV}?format=csv"
  to="file://${out}?format=raw|connect_mapping?mapping=$(urlenc "$MAPPING")"
  bench_tool "mq-bridge-app-bloblang" "$out" "$COPY_TIMEOUT" run_mqb_once

  out="$(out_for mq-bridge-app-connect-untyped)"
  from="connect://?yaml=$(urlenc "$(connect_input untyped)")"
  to="connect://?yaml=$(urlenc "$(connect_output)")"
  bench_tool "mq-bridge-app-connect-untyped" "$out" "$CONNECT_TIMEOUT" run_mqb_once
  rm -f "$out"

  out="$(out_for mq-bridge-app-connect)"
  from="connect://?yaml=$(urlenc "$(connect_input typed)")"
  to="connect://?yaml=$(urlenc "$(connect_output)")"
  bench_tool "mq-bridge-app-connect" "$out" "$CONNECT_TIMEOUT" run_mqb_once
}

# Connect runs its pipeline on several threads, so its output order is not the
# input's: sort by id before the record-level comparison.
parity() {
  [[ -e "$OUT_MQB" ]] || { echo "missing $OUT_MQB — run run_csv_mqb.sh first" >&2; exit 1; }
  local label f
  for label in redpanda-connect mq-bridge-app-bloblang mq-bridge-app-connect; do
    f="$(out_for "$label")"
    [[ -e "$f" ]] || { echo "-- parity: no output for $label, skipping"; continue; }
    echo "-- parity: mq-bridge-app vs $label"
    python3 - "$f" <<'PY'
import json, sys
src = sys.argv[1]
lines = sorted((json.loads(l)["id"], l) for l in open(src) if l.strip())
with open(src + ".sorted", "w") as f:
    f.writelines(l for _, l in lines)
PY
    python3 "$HERE/compare_jsonl.py" "$OUT_MQB" "$f.sorted" \
      || { echo "  FAILED: outputs diverge — the timings are not comparable" >&2; exit 1; }
    rm -f "$f.sorted"
  done
}

ensure_csv
case "${1:-all}" in
  rpc)    rpc_cells ;;
  plugin) plugin_cells ;;
  parity) parity ;;
  all)    rpc_cells; plugin_cells ;;
  *) echo "usage: $0 [rpc|plugin|parity|all]" >&2; exit 2 ;;
esac
