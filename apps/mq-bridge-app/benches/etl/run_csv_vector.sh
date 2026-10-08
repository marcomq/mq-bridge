#!/usr/bin/env bash
# CSV -> JSONL, Vector only. Runnable on its own; run_csv_mqb.sh provides the
# mq-bridge-app rows.
#
#   ./run_csv_vector.sh            # vector-untyped, then vector (typed)
#   ./run_csv_vector.sh parity     # both outputs vs. mq-bridge-app's (run_csv_mqb.sh)
#
# Vector has no CSV codec and its file source never ends, so the file arrives on
# stdin and a remap parses each line (vector/csv_*.yaml). Defaults otherwise.
#
# Skipped if the binary is absent. Install:
#   curl -sL https://github.com/vectordotdev/vector/releases/download/v0.59.0/vector-0.59.0-arm64-apple-darwin.tar.gz \
#     | tar -xz -C benches/etl/bin --strip-components 3 ./vector-arm64-apple-darwin/bin/vector
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/csv_common.sh"

if [[ ! -x "$VECTOR_BIN" ]]; then
  echo "-- vector: not found at $VECTOR_BIN, skipping" >&2
  exit 0
fi

WORK="$(mktemp -d -t mqb_vector.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

out_for() { echo "${OUT_VECTOR%.jsonl}_$1.jsonl"; }

run_vector_once() {
  rm -f "$out"
  sed -e "s#@DATA_DIR@#$WORK#" -e "s#@OUT@#$out#" "$HERE/vector/csv_$1.yaml" > "$WORK/vector.yaml"
  "$VECTOR_BIN" -q -c "$WORK/vector.yaml" < "$CSV"
}

# Vector's remap runs concurrently, so the output order is not the input's.
parity_one() {
  local label="$1" ref="$2" f; f="$(out_for "$1")"
  for p in "$ref" "$f"; do
    [[ -e "$p" ]] || { echo "missing $p — run run_csv_mqb.sh and this script first" >&2; exit 1; }
  done
  echo "-- parity: mq-bridge-app vs $label"
  python3 - "$f" <<'PY'
import json, sys
src = sys.argv[1]
lines = sorted((int(json.loads(l)["id"]), l) for l in open(src) if l.strip())
with open(src + ".sorted", "w") as f:
    f.writelines(l for _, l in lines)
PY
  python3 "$HERE/compare_jsonl.py" "$ref" "$f.sorted" \
    || { echo "  FAILED: outputs diverge — the timings are not comparable" >&2; exit 1; }
  rm -f "$f.sorted"
}

ensure_csv
case "${1:-run}" in
  run)
    # bench_tool sends stdin to /dev/null, so the redirect lives in the function.
    out="$(out_for vector-untyped)"
    bench_tool "vector-untyped" "$out" "$CONNECT_TIMEOUT" run_vector_once untyped
    out="$(out_for vector)"
    bench_tool "vector" "$out" "$CONNECT_TIMEOUT" run_vector_once typed
    ;;
  parity)
    parity_one vector "$OUT_MQB"
    parity_one vector-untyped "$OUT_MQB_RAW"
    ;;
  *) echo "usage: $0 [run|parity]" >&2; exit 2 ;;
esac
