#!/usr/bin/env bash
# Usage: ./search.sh "hedphones"   (typos are fine; Meilisearch tolerates them)
set -euo pipefail
curl -sS http://localhost:7700/indexes/products/search \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg q "$1" '{q: $q, limit: 3}')" | jq -r '.hits[] | "\(.name)  \(.price)"'
