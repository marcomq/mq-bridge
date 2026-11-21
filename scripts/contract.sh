#!/usr/bin/env bash
# Runs the delivery contract per broker, one container set at a time, and removes it afterwards.
#
#   scripts/contract.sh                          every broker
#   scripts/contract.sh nats kafka               only these
#   scripts/contract.sh --prune-images kafka     also delete the images this run downloaded
#   scripts/contract.sh --list                   the broker names
set -u
cd "$(dirname "$0")/.." || exit 1

ALL="dir_spool sqlite sled file nats amqp kafka redis_streams mongodb mqtt mysql mariadb aws postgres postgres_cdc clickhouse object_store ibm_mq"
LIMIT_SECONDS=${MQB_CONTRACT_LIMIT:-600}
prune=false
brokers=""

for arg in "$@"; do
  case "$arg" in
    --prune-images) prune=true ;;
    --list) printf '%s\n' $ALL; exit 0 ;;
    -h | --help) sed -n 2,6p "$0" | cut -c3-; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *)
      case " $ALL " in
        *" $arg "*) brokers="$brokers $arg" ;;
        *) echo "unknown broker: $arg (try --list)" >&2; exit 2 ;;
      esac
      ;;
  esac
done
brokers=${brokers:-$ALL}

compose_file() {
  case "$1" in
    dir_spool | sqlite | sled | file) echo "" ;;
    redis_streams) echo "tests/integration/docker-compose/redis.yml" ;;
    *) echo "tests/integration/docker-compose/$1.yml" ;;
  esac
}

current_file=""
downloaded=""

clean_up() {
  [ -n "$current_file" ] && docker compose -f "$current_file" down -v >/dev/null 2>&1
  if $prune && [ -n "$downloaded" ]; then
    docker rmi $downloaded >/dev/null 2>&1
  fi
  current_file=""
  downloaded=""
}
trap 'clean_up; exit 130' INT TERM

logs=$(mktemp -d "${TMPDIR:-/tmp}/mq-bridge-contract.XXXXXX")
echo "Building the test binary..."
build_log="$logs/build.log"
cargo test --test integration_test --features full,test-utils --no-run >"$build_log" 2>&1 || {
  tail -25 "$build_log"
  exit 1
}
bin=$(sed -n 's/.*Executable .*(\(.*\))/\1/p' "$build_log" | tail -1)
[ -x "$bin" ] || { echo "no test binary, see $build_log" >&2; exit 1; }

summary=""
failed=0
for broker in $brokers; do
  file=$(compose_file "$broker")
  if [ -n "$file" ]; then
    current_file=$file
    for image in $(docker compose -f "$file" config --images 2>/dev/null); do
      docker image inspect "$image" >/dev/null 2>&1 || downloaded="$downloaded $image"
    done
  fi

  log="$logs/$broker.log"
  started=$(date +%s)
  MQB_TEST_BACKEND=$broker perl -e 'alarm shift; exec @ARGV' "$LIMIT_SECONDS" "$bin" \
    --ignored --nocapture --exact "integration::contract::test_${broker}_contract" >"$log" 2>&1
  code=$?
  seconds=$(($(date +%s) - started))
  clean_up

  count() { grep -aE "^$broker  [a-z_]+ +$1" "$log" | sort -u | wc -l | tr -d ' '; }
  passed=$(count ok)
  red=$(count FAILED)
  skipped=$(count skipped)
  if [ "$code" -eq 0 ]; then status=ok; else status=FAILED; failed=$((failed + 1)); fi
  line=$(printf '%-14s %-7s %4ss  ok %2s  failed %2s  skipped %2s' \
    "$broker" "$status" "$seconds" "$passed" "$red" "$skipped")
  echo "$line"
  grep -aE "^$broker  [a-z_]+ +FAILED" "$log" | sort -u | cut -c1-200 | sed 's/^/    /'
  summary="$summary$line"$'\n'
done

echo
printf '%s' "$summary"
echo "Logs: $logs"
[ "$failed" -eq 0 ]
