#!/usr/bin/env bash
# Smoke test run by CI: starts the stack, changes rows, checks the warehouse change log.
set -euo pipefail
cd "$(dirname "$0")"

src() { docker compose exec -T postgres psql -U app -d app -qc "$1"; }
dw() { docker compose exec -T warehouse psql -U dw -d dw -qAtc "$1"; }
# The change log as the warehouse reads it: one row per (_lsn, id), in arrival order.
log() { dw "SELECT string_agg(id || ':' || _op || ':' || coalesce(amount_eur::text, '-'), ' ' ORDER BY first, id)
  FROM (SELECT id, _op, amount_eur, min(_synced_at) AS first FROM orders_changes GROUP BY _lsn, id, _op, amount_eur) c"; }
count() { dw "SELECT count(*) FROM orders_changes WHERE $1"; }
dlq() { docker compose cp mq-bridge:/app/dlq.jsonl - 2>/dev/null | tar xO 2>/dev/null || true; }
wait_for() { # wait_for <description> <expected change log>
  for _ in $(seq 1 90); do
    if [ "$(log 2>/dev/null)" = "$2" ]; then echo "ok: $1"; return; fi
    sleep 2
  done
  echo "FAILED: $1"; echo "  expected: $2"; echo "  got:      $(log)"
  docker compose logs --tail 50 mq-bridge
  exit 1
}

trap 'docker compose down -v >/dev/null 2>&1' EXIT
docker compose up -d --build

SNAP="1:snapshot:12.5 2:snapshot:99 3:snapshot:3"
wait_for "existing rows are copied, marked as snapshot" "$SNAP"

src "UPDATE orders SET status = 'paid' WHERE customer = 'ada'"
src "DELETE FROM orders WHERE customer = 'linus'"
src "INSERT INTO orders (customer, amount_cents) VALUES ('barbara', 4200)"
CHANGES="$SNAP 1:update:12.5 3:delete:- 4:insert:42"
wait_for "update, delete and insert are appended in commit order" "$CHANGES"
[ "$(dw "SELECT count(*) FROM orders_changes WHERE _op <> 'snapshot' AND _lsn IS NULL")" = 0 ] \
  || { echo "FAILED: a change row has no _lsn"; exit 1; }
echo "ok: every change row carries its commit LSN"

docker compose stop mq-bridge
src "INSERT INTO orders (customer, amount_cents) VALUES ('edsger', 700)"
docker compose start mq-bridge
CHANGES="$CHANGES 5:insert:7"
wait_for "a change made while the bridge was down arrives after a restart" "$CHANGES"
[ "$(count "_op = 'snapshot'")" = 3 ] || { echo "FAILED: the copy ran again"; exit 1; }
[ "$(count "customer = 'edsger'")" = 1 ] || { echo "FAILED: the offline row arrived more than once"; exit 1; }
echo "ok: the copy is not repeated and the offline row arrives once"

src "INSERT INTO orders (customer, amount_cents) VALUES ('kim', 250000)"
src "INSERT INTO orders (customer, amount_cents) VALUES ('ken', 1500)"
CHANGES="$CHANGES 7:insert:15"
wait_for "a row the warehouse rejects does not stop the route" "$CHANGES"
case "$(dlq)" in *'"customer":"kim"'*) echo "ok: the rejected row is in the DLQ" ;;
  *) echo "FAILED: rejected row not in the DLQ: $(dlq)"; exit 1 ;; esac
