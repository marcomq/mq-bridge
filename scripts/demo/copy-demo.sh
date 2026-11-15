#!/usr/bin/env bash
# Re-shoot docs/assets/copy-demo.png: three real `mqb copy` runs of 1M rows each.
#
#   scripts/demo/copy-demo.sh up      # start and seed the containers
#   scripts/demo/copy-demo.sh shoot   # warm up, capture, check, render
#   scripts/demo/copy-demo.sh down    # remove the containers
#
# Needs docker, python3 and `freeze`. MQB selects the binary (default: mqb on PATH),
# DEMO_CSV an existing orders.csv (default: generated, seed 42).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$REPO/target/copy-demo"
MQB="${MQB:-$(command -v mqb)}"
CONTAINERS="mqb-demo-pg mqb-demo-s3 mqb-demo-nats mqb-demo-clickhouse mqb-demo-kafka"
CHECKPOINT=/tmp/ck.json

export AWS_ENDPOINT_URL=http://127.0.0.1:4566
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_REGION=us-east-1
export AWS_ALLOW_HTTP=true

psql() { docker exec -i mqb-demo-pg psql -U analytics -d shop -q "$@"; }
clickhouse() { docker exec -i mqb-demo-clickhouse clickhouse-client --user demo "$@"; }

wait_for() {
  local name="$1"; shift
  echo "waiting for $name..."
  for _ in $(seq 1 90); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "ERROR: $name did not become ready" >&2; exit 1
}

orders_csv() {
  mkdir -p "$WORK"
  if [ -n "${DEMO_CSV:-}" ]; then
    cp "$DEMO_CSV" "$WORK/orders.csv"
  elif [ ! -f "$WORK/orders.csv" ]; then
    python3 - "$WORK/orders.csv" <<'PY'
import random, sys
random.seed(42)
with open(sys.argv[1], "w") as out:
    out.write("id,customer,amount,status\n")
    for i in range(1, 1_000_001):
        status = random.choice(("pending", "paid", "refunded"))
        out.write(f"{i},cust{i},{random.uniform(1, 2000):.2f},{status}\n")
PY
  fi
}

up() {
  orders_csv
  down
  docker run -d --name mqb-demo-pg -p 5433:5432 \
    -e POSTGRES_USER=analytics -e POSTGRES_PASSWORD=demo -e POSTGRES_DB=shop \
    postgres:16-alpine >/dev/null
  docker run -d --name mqb-demo-s3 -p 4566:4566 -e SERVICES=s3 -e LS_LOG=warn \
    localstack/localstack:4.12.0 >/dev/null
  docker run -d --name mqb-demo-nats -p 4222:4222 nats:2.12.2-alpine3.22 >/dev/null
  docker run -d --name mqb-demo-clickhouse -p 8123:8123 --ulimit nofile=262144:262144 \
    -e CLICKHOUSE_USER=demo -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
    clickhouse/clickhouse-server:24.8 >/dev/null
  docker run -d --name mqb-demo-kafka --hostname kafka -p 9092:9092 \
    -e KAFKA_BROKER_ID=1 -e KAFKA_PROCESS_ROLES=broker,controller \
    -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@kafka:9093 -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
    -e CLUSTER_ID=copy-demo-cluster-id-0001 \
    -e KAFKA_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 \
    -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://127.0.0.1:9092 \
    -e KAFKA_INTER_BROKER_LISTENER_NAME=PLAINTEXT \
    -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 -e KAFKA_NUM_PARTITIONS=4 \
    confluentinc/cp-kafka:7.9.5 >/dev/null

  wait_for postgres docker exec mqb-demo-pg pg_isready -U analytics -d shop
  wait_for s3 docker exec mqb-demo-s3 awslocal s3 ls
  wait_for clickhouse clickhouse --query "SELECT 1"
  wait_for kafka docker exec mqb-demo-kafka kafka-topics --bootstrap-server localhost:9092 --list

  echo "seeding 1M orders into postgres and clickhouse..."
  psql -c "CREATE TABLE orders (id integer PRIMARY KEY, customer text NOT NULL,
           amount numeric(10,2) NOT NULL, status text NOT NULL);"
  psql -c "\copy orders FROM STDIN WITH (FORMAT csv, HEADER true)" < "$WORK/orders.csv"
  psql -c "ANALYZE orders;"
  clickhouse --query "CREATE TABLE orders (id UInt32, customer String, amount Float64,
                      status String) ENGINE = MergeTree ORDER BY id"
  clickhouse --query "INSERT INTO orders FORMAT CSVWithNames" < "$WORK/orders.csv"
  echo "postgres rows:   $(psql -tAc 'SELECT count(*) FROM orders')"
  echo "clickhouse rows: $(clickhouse --query 'SELECT count() FROM orders')"
}

down() {
  # shellcheck disable=SC2086
  docker rm -f $CONTAINERS >/dev/null 2>&1 || true
}

# Each of these silently yields a run that copies nothing and still renders fine:
# a kept SQL cursor, a kept checkpoint file, or an object-store prefix that already
# holds the range.
reset_state() {
  psql -c "DROP TABLE IF EXISTS mqb_cursors_orders;"
  rm -f "$CHECKPOINT"
  docker exec mqb-demo-s3 awslocal s3 rb --force s3://lake >/dev/null 2>&1 || true
  docker exec mqb-demo-s3 awslocal s3 mb s3://lake >/dev/null
  docker exec mqb-demo-kafka kafka-topics --bootstrap-server localhost:9092 \
    --delete --if-exists --topic orders >/dev/null
}

shoot() {
  cd "$WORK"
  # mqb is a launcher for the mq-bridge-app next to it, so put its directory on PATH.
  PATH="$(cd "$(dirname "$MQB")" && pwd):$PATH"
  mqb --version

  local src1="'file://orders.csv?format=csv'"
  local dst1="'s3://lake/orders?format=payload&extension=jsonl.zst&compression=zstd'"
  local opt1="--drain --batch-size 32768"
  local src2="'postgres://analytics:demo@127.0.0.1:5433/shop?table=orders&cursor_column=id'"
  local dst2="'nats://127.0.0.1:4222?subject=orders&no_jetstream=true'"
  local opt2="--drain --resume --batch-size 32768 --filter 'number(amount) > 50'"
  local src3="'clickhouse://127.0.0.1:8123?username=demo&table=orders&cursor_column=id&checkpoint_store=file://$CHECKPOINT'"
  local dst3="'kafka://127.0.0.1:9092?topic=orders'"
  local opt3="--drain --resume --batch-size 32768"

  echo "-- warm-up (discarded)"
  reset_state
  eval "mqb copy $src1 $dst1 $opt1" >/dev/null
  eval "mqb copy $src2 $dst2 $opt2" >/dev/null
  eval "mqb copy $src3 $dst3 $opt3" >/dev/null

  echo "-- capture"
  reset_state
  {
    printf '$ mqb copy %s %s %s\n' "$src1" "$dst1" "$opt1"
    eval "mqb copy $src1 $dst1 $opt1 --color always" 2>&1
    printf '\n$ mqb copy %s\n%s %s\n' "$src2" "$dst2" "$opt2"
    eval "mqb copy $src2 $dst2 $opt2 --color always" 2>&1
    printf '\n$ mqb copy %s\n%s %s\n' "$src3" "$dst3" "$opt3"
    eval "mqb copy $src3 $dst3 $opt3 --color always" 2>&1
  } > transcript.txt
  cat transcript.txt

  local kept
  kept="$(awk -F, 'NR > 1 && $3 > 50' orders.csv | wc -l | python3 -c 'print(f"{int(input()):_}")')"
  [ "$(grep -c 'copied 1_000_000 rows' transcript.txt)" -eq 2 ] || {
    echo "ERROR: expected two full 1,000,000-row runs" >&2; exit 1; }
  grep -q "copied $kept of 1_000_000 rows" transcript.txt || {
    echo "ERROR: filtered run did not report $kept rows" >&2; exit 1; }
  [ "$(grep -c -v -e '^\$ ' -e "^'" -e '^copied ' -e '^$' transcript.txt)" -eq 0 ] || {
    echo "ERROR: transcript holds warnings or errors" >&2; exit 1; }
  echo "-- guards passed"

  freeze transcript.txt -o copy-demo.png --language ansi --theme charm --font.size 15 \
    --padding 30 --margin 20 --border.radius 8 --window
  echo "rendered $WORK/copy-demo.png — review it, then copy it to docs/assets/copy-demo.png"
}

case "${1:-}" in
  up) up ;;
  shoot) shoot ;;
  down) down ;;
  *) echo "usage: $0 up|shoot|down" >&2; exit 2 ;;
esac
