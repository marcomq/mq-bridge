#!/usr/bin/env bash
# Smoke test run by CI: posts orders, restarts the bridge, checks the history continues.
set -euo pipefail
cd "$(dirname "$0")"

sql() { docker compose exec -T postgres psql -U app -d app -tAc "$1"; }
order() { # order <id> <customer> <amount> <time>
  local body="{\"id\": $1, \"customer_id\": $2, \"amount\": $3, \"created_at\": \"$4\"}"
  for _ in $(seq 1 60); do
    curl -sS --fail --max-time 30 -o /dev/null http://localhost:8080/orders \
      -H 'Content-Type: application/json' -d "$body" 2>/dev/null && return
    sleep 2
  done
  echo "FAILED: could not post order $1"; docker compose logs --tail 50 mq-bridge; exit 1
}
expect() { # expect <description> <query> <expected>
  local got; got=$(sql "$2")
  if [ "$got" = "$3" ]; then echo "ok: $1"; return; fi
  echo "FAILED: $1"; echo "  expected: $3"; echo "  got:      $got"
  docker compose logs --tail 50 mq-bridge
  exit 1
}

trap 'docker compose down -v >/dev/null 2>&1' EXIT
docker compose up -d

order 1 1 40 2026-10-01T10:00:00Z
order 2 2 15 2026-10-01T11:00:00Z
order 3 1 60 2026-10-02T09:00:00Z
expect "a first order has no history" \
  "SELECT count(*) FROM orders_enriched WHERE history IS NULL" "2"
expect "the second order sees the first one" \
  "SELECT history->>'orders' || '/' || (history->>'spend')::float8 FROM orders_enriched WHERE id = 3" "1/40"
expect "orders are enriched with the customer" \
  "SELECT customer->>'name' FROM orders_enriched WHERE id = 3" "Ada"

docker compose restart mq-bridge

order 4 1 900 2026-10-03T09:00:00Z
order 5 9 20 2026-10-03T10:00:00Z
expect "the history survives a restart" \
  "SELECT history->>'orders' FROM orders_review WHERE id = 4" "2"
expect "an order far above the usual amount goes to review" \
  "SELECT string_agg(id::text, ',' ORDER BY id) FROM orders_review" "4"
expect "an unknown customer is stored without customer data" \
  "SELECT coalesce(customer::text, 'null') FROM orders_enriched WHERE id = 5" "null"
expect "the states are rows in Postgres" \
  "SELECT count(*) FROM customer_stats" "3"
