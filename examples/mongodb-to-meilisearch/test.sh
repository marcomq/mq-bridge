#!/usr/bin/env bash
# Smoke test run by CI: starts the stack, changes documents, checks the index follows.
set -euo pipefail
cd "$(dirname "$0")"

mongo() { docker compose exec -T mongo mongosh --quiet shop --eval "$1" >/dev/null; }
indexed() {
  curl -sS "http://localhost:7700/indexes/products/documents?limit=100&fields=name,price" |
    jq -r '[.results[] | "\(.name)=\(.price)"] | sort | join("|")'
}
wait_for() { # wait_for <description> <expected documents>
  for _ in $(seq 1 90); do
    if [ "$(indexed 2>/dev/null)" = "$2" ]; then echo "ok: $1"; return; fi
    sleep 2
  done
  echo "FAILED: $1"; echo "  expected: $2"; echo "  got:      $(indexed)"
  docker compose logs --tail 50 mq-bridge setup
  exit 1
}

trap 'docker compose down -v >/dev/null 2>&1' EXIT
docker compose up -d

wait_for "existing documents are copied" \
  "Mechanical keyboard=99|USB-C charger=35|Wireless headphones=79"

mongo 'db.products.insertOne({name: "Webcam", description: "1080p webcam with a privacy shutter.", price: 45.0, tags: ["video"]})'
mongo 'db.products.updateOne({name: "Wireless headphones"}, {$set: {price: 69.0}})'
mongo 'db.products.deleteOne({name: "USB-C charger"})'
wait_for "insert, update and delete are applied" \
  "Mechanical keyboard=99|Webcam=45|Wireless headphones=69"

docker compose stop mq-bridge
mongo 'db.products.insertOne({name: "Desk lamp", description: "LED lamp with a dimmer.", price: 29.0, tags: ["desk"]})'
docker compose start mq-bridge
wait_for "changes made while the bridge was down arrive after a restart" \
  "Desk lamp=29|Mechanical keyboard=99|Webcam=45|Wireless headphones=69"

top=$(./search.sh "hedphones" | head -1)
case "$top" in "Wireless headphones"*) echo "ok: a misspelled search finds the headphones" ;; *) echo "FAILED: search returned: $top"; exit 1 ;; esac
