#!/usr/bin/env bash
# Smoke test run by CI: posts tickets, checks each lands in its team's stream.
set -euo pipefail
cd "$(dirname "$0")"

ids() { # ids <stream>: the ticket ids in a stream, sorted
  docker compose exec -T redis redis-cli --raw XRANGE "$1" - + |
    grep -oE '"id": ?[0-9]+' | grep -oE '[0-9]+$' | sort -n | paste -sd, - || true
}
post() {
  for _ in $(seq 1 60); do
    curl -sS --fail --max-time 120 -o /dev/null http://localhost:8080/tickets \
      -H 'Content-Type: application/json' -d "$1" 2>/dev/null && return
    sleep 2
  done
  echo "FAILED: could not post $1"; docker compose logs --tail 50 mq-bridge setup; exit 1
}
expect() { # expect <stream> <ids>: each id is in the stream; ticket 6 may land there too
  local got; got=",$(ids "$1"),"
  local missing=""
  for id in ${2//,/ }; do [[ $got == *",$id,"* ]] || missing=1; done
  if [ -z "$missing" ]; then echo "ok: $1 holds $2"; return; fi
  echo "FAILED: $1"; echo "  expected: $2"; echo "  got:      $(ids "$1")"
  docker compose logs --tail 50 mq-bridge
  exit 1
}

trap 'docker compose down -v >/dev/null 2>&1' EXIT
docker compose up -d

# The listener answers once the ticket is in its stream, so no waiting is needed afterwards.
while read -r ticket; do post "$ticket"; done < tickets.jsonl

expect tickets.billing 1
expect tickets.technical 2,4
expect tickets.account 3
expect tickets.triage 5

# A ticket that tries to pick its own queue still lands in one of the four.
total=0
for s in billing technical account triage; do
  total=$((total + $(docker compose exec -T redis redis-cli XLEN "tickets.$s" | tr -d '\r')))
done
[ "$total" = 6 ] && echo "ok: all 6 tickets are routed" || { echo "FAILED: $total of 6 tickets routed"; exit 1; }
