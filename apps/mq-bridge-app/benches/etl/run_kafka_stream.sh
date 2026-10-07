#!/usr/bin/env bash
# Scenario 9: Kafka -> file, stateless, mq-bridge-app vs Arroyo vs Sea Streamer.
#
# The measured job is the same on both sides: read a 4-partition topic of
# 1M JSON records, deserialize, keep 4 of the 7 columns, write to local disk.
# No window, no join, no aggregation, no UDF — Arroyo's
# stateful machinery is deliberately not exercised, because mq-bridge-app has no
# equivalent and a ratio over different work means nothing.
#
# Around that headline cell the mq-bridge-app side is measured as a ladder, so
# each delivery guarantee is priced separately rather than hidden inside one
# number: raw passthrough (the ceiling), the projection, and the projection with
# deduplication on top. Arroyo's exactly-once is not decomposable that way, so it
# contributes a single row.
#
# Prereqs:
#   docker compose -f docker-compose.kafka.yml up -d --wait     (Kafka + Arroyo)
#   ./seed.sh up && ./seed.sh bench 1000000                     (Postgres source rows)
#   ./run_kafka_stream.sh seed                                  (fill the topic)
#   cargo build -p mq-bridge-app --no-default-features --features bench,kafka,mimalloc --release
#   # Build the pinned Sea Streamer helper into the existing target directory.
#   cargo build --manifest-path benches/etl/sea_streamer/Cargo.toml --release \
#     --target-dir target
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# seed.sh dispatches on its own "$1", and a sourced script sees the caller's
# positional args — so sourcing it with a subcommand still in $1 makes it print
# its usage and exit 2. Stash the args, source with an empty $@, restore.
ARGV=("$@"); set --
source "$HERE/seed.sh"   # also sources lib.sh
set -- "${ARGV[@]+"${ARGV[@]}"}"

TOPIC="${TOPIC:-bench_src}"
PARTITIONS="${PARTITIONS:-4}"
KAFKA_CONTAINER="${KAFKA_CONTAINER:-kafka-etl-bench-mq-bridge}"
ARROYO_CONTAINER="${ARROYO_CONTAINER:-arroyo-etl-bench}"
KAFKA_HOST_BROKERS="${KAFKA_HOST_BROKERS:-localhost:9092}"
KAFKA_DOCKER_BROKERS="${KAFKA_DOCKER_BROKERS:-kafka:29092}"
ROWS="${ROWS:-1000000}"
REPEATS="${REPEATS:-3}"
CONCURRENCY="${CONCURRENCY:-4}"          # matched to PARTITIONS and Arroyo's parallelism
BATCH="${BATCH:-1024}"
CHECKPOINT_INTERVAL="${CHECKPOINT_INTERVAL:-10}"   # Arroyo's own default, in seconds

RESULTS_DIR="${RESULTS_DIR:-$HERE/results}"
RESULTS_CSV="${RESULTS_CSV:-$RESULTS_DIR/kafka_stream.csv}"
OUT_FILE="${OUT_FILE:-/tmp/mqb_kafka_out.jsonl}"
DEDUP_STORE="${DEDUP_STORE:-/tmp/mqb_kafka_dedup}"
SEA_STREAMER_RELAY="${SEA_STREAMER_RELAY:-$HERE/../../target/release/sea-streamer-relay}"
SEA_STREAMER_COUNT="${SEA_STREAMER_COUNT:-$HERE/../../target/release/sea-streamer-count}"
SEA_STREAMER_LABEL="${SEA_STREAMER_LABEL:-sea-streamer-file}"
RPC_BIN="${RPC_BIN:-$HERE/bin/redpanda-connect}"
VECTOR_BIN="${VECTOR_BIN:-$HERE/bin/vector}"
CONNECT_PLUGIN="${CONNECT_PLUGIN:-/opt/homebrew/lib/mq-bridge/libmq_bridge_connect.dylib}"
export RESULTS_CSV ROWS REPEATS

# --- Topic seeding -----------------------------------------------------------

seed_topic() {
  echo "recreating topic ${TOPIC} with ${PARTITIONS} partitions"
  docker exec "$KAFKA_CONTAINER" kafka-topics --bootstrap-server localhost:9092 \
    --delete --topic "$TOPIC" >/dev/null 2>&1 || true
  docker exec "$KAFKA_CONTAINER" kafka-topics --bootstrap-server localhost:9092 \
    --create --topic "$TOPIC" --partitions "$PARTITIONS" --replication-factor 1 \
    --config retention.ms=-1 >/dev/null
  # linger.ms=1 (the default from 0.4.20 on), URL-encoded [["linger.ms","1"]].
  # Concurrency 1: on 0.4.19 this linger at concurrency 4 left the topic short.
  local LINGER_OPTS='%5B%5B%22linger.ms%22%2C%221%22%5D%5D'
  # Filled from the same `bench` table every other scenario reads, so the row
  # shape here is identical to the Postgres and CSV scenarios.
  "$BIN" copy \
    --from "${PG_URL}?table=bench&cursor_column=id&sslmode=disable" \
    --to "kafka://${KAFKA_HOST_BROKERS}?topic=${TOPIC}&producer_options=${LINGER_OPTS}" \
    --drain --batch-size 32768 --concurrency 1
  docker exec "$KAFKA_CONTAINER" kafka-get-offsets --bootstrap-server localhost:9092 --topic "$TOPIC"
}

# --- Cells -------------------------------------------------------------------
#
# Both tools go through stream_bench.py so they share one stopwatch: start the
# job, poll the sink until the whole dataset has landed, stop. bench_tool from
# lib.sh is deliberately not used here — it times a command to completion, which
# a stream processor's pipeline never reaches.

cell() {
  local tool="$1" variant="$2" label="$3"
  "${PYTHON[@]:-python3}" "$HERE/stream_bench.py" \
    --tool "$tool" --variant "$variant" --label "$label" \
    --topic "$TOPIC" --brokers "$KAFKA_DOCKER_BROKERS" \
    --brokers-host "$KAFKA_HOST_BROKERS" --bin "$BIN" \
    --rpc-bin "$RPC_BIN" --vector-bin "$VECTOR_BIN" --connect-plugin "$CONNECT_PLUGIN" \
    --sea-relay "$SEA_STREAMER_RELAY" --sea-count "$SEA_STREAMER_COUNT" \
    --out-file "$OUT_FILE" --dedup-store "$DEDUP_STORE" \
    --batch-size "$BATCH" --rows "$ROWS" --repeats "$REPEATS" \
    --parallelism "$CONCURRENCY" --checkpoint-interval "$CHECKPOINT_INTERVAL" \
    --results "$RESULTS_CSV" "${@:4}"
}

# --- Entry points ------------------------------------------------------------

case "${1:-all}" in
  seed)
    require_bin
    seed_topic
    ;;
  mqb)
    require_bin
    mkdir -p "$RESULTS_DIR"
    # Ceiling: record bytes straight through, no deserialization at all.
    cell mqb passthrough mqb-passthrough
    # Headline: the same projection Arroyo's SELECT does.
    cell mqb projection mqb-projection
    # No dedup cell, and no `checkpoint_store` cell either. The two tools reach
    # idempotency by different mechanisms, so neither is a column of the other:
    # Arroyo checkpoints operator state for exactly-once within the pipeline,
    # while mq-bridge-app relies on the source's own resumable progress — for a
    # Kafka source that is the committed group offset, so every row here already
    # runs resumable, the same as Arroyo (`cursor_id`/`checkpoint_store` are
    # fields of the sqlx, clickhouse, mongodb, object_store and postgres_cdc
    # sources, which a Kafka source does not have). Making redelivery idempotent
    # on top of that is the `deduplication` middleware's job — a store lookup and
    # insert per message, which is a different question from throughput and is
    # priced separately elsewhere. `stream_bench.py --variant projection-dedup`
    # still runs it on demand.
    ;;
  arroyo)
    mkdir -p "$RESULTS_DIR"
    cell arroyo projection arroyo-projection
    ;;
  sea)
    require_bin
    [[ -x "$SEA_STREAMER_RELAY" ]] || { echo "missing Sea Streamer relay: $SEA_STREAMER_RELAY" >&2; exit 1; }
    [[ -x "$SEA_STREAMER_COUNT" ]] || { echo "missing Sea Streamer counter: $SEA_STREAMER_COUNT" >&2; exit 1; }
    mkdir -p "$RESULTS_DIR"
    cell mqb passthrough mqb-normal-passthrough --mqb-file-format normal
    cell sea-streamer passthrough "$SEA_STREAMER_LABEL"
    ;;
  connect)
    # Redpanda Connect standalone, then the same Bloblang projection as an
    # mq-bridge-app middleware (mq-bridge-connect plugin) on the native source.
    require_bin
    [[ -x "$RPC_BIN" ]] || { echo "missing Redpanda Connect: $RPC_BIN" >&2; exit 1; }
    [[ -f "$CONNECT_PLUGIN" ]] || { echo "missing mq-bridge-connect plugin: $CONNECT_PLUGIN" >&2; exit 1; }
    mkdir -p "$RESULTS_DIR"
    cell redpanda-connect passthrough redpanda-connect-passthrough
    cell redpanda-connect projection redpanda-connect-projection
    cell mqb projection-bloblang mqb-projection-bloblang
    ;;
  vector)
    [[ -x "$VECTOR_BIN" ]] || { echo "missing Vector: $VECTOR_BIN" >&2; exit 1; }
    mkdir -p "$RESULTS_DIR"
    cell vector passthrough vector-passthrough
    cell vector projection vector-projection
    ;;
  kafka-sink)
    # Topic to topic, mq-bridge-app and Vector. Completion is the destination
    # topic's end offsets, which needs a Kafka client in the harness.
    require_bin
    [[ -x "$VECTOR_BIN" ]] || { echo "missing Vector: $VECTOR_BIN" >&2; exit 1; }
    mkdir -p "$RESULTS_DIR"
    PYTHON=(uv run --quiet --with kafka-python python3)
    for variant in passthrough projection; do
      cell mqb "$variant" "mqb-kafka-$variant" --sink kafka --partitions "$PARTITIONS"
      cell vector "$variant" "vector-kafka-$variant" --sink kafka --partitions "$PARTITIONS"
    done
    ;;
  parity)
    # Prove the two sides produced the same records before any ratio is quoted.
    # Both outputs are sorted by id first: the partitions are consumed
    # concurrently on both sides, so line order is not meaningful, but the
    # multiset of rows is.
    require_bin
    mkdir -p "$RESULTS_DIR"
    RESULTS_CSV="$RESULTS_DIR/kafka_stream_parity.csv"
    docker exec "$ARROYO_CONTAINER" sh -c 'rm -rf /arroyo-out/*'
    export REPEATS=1
    cell mqb projection parity-mqb
    # The last KEPT_OUTPUT_DIR line is the timed run's directory; the warmup's is
    # the first, and concatenating both would double the row count.
    arroyo_dir="$(cell arroyo projection parity-arroyo --keep-output \
      | tee /dev/stderr | grep '^KEPT_OUTPUT_DIR=' | tail -1 | cut -d= -f2)"
    [[ -n "$arroyo_dir" ]] || { echo "no Arroyo output dir reported" >&2; exit 1; }
    local_dir="$(mktemp -d)"
    docker exec "$ARROYO_CONTAINER" sh -c \
      "find '$arroyo_dir' -type f -exec cat {} +" > "$local_dir/arroyo.jsonl"
    python3 - "$OUT_FILE" "$local_dir/arroyo.jsonl" <<'PY'
import json, sys
for path in sys.argv[1:]:
    rows = sorted((json.loads(l) for l in open(path) if l.strip()), key=lambda r: r["id"])
    with open(path + ".sorted", "w") as f:
        for r in rows:
            f.write(json.dumps(r, sort_keys=True) + "\n")
PY
    python3 "$HERE/compare_jsonl.py" "$OUT_FILE.sorted" "$local_dir/arroyo.jsonl.sorted"
    rm -rf "$local_dir"
    docker exec "$ARROYO_CONTAINER" sh -c 'rm -rf /arroyo-out/*'
    ;;
  all)
    "$0" mqb
    "$0" arroyo
    "$0" sea
    ;;
  *)
    echo "usage: $0 seed|mqb|arroyo|sea|connect|vector|kafka-sink|parity|all" >&2; exit 2 ;;
esac
