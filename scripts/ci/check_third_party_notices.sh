#!/usr/bin/env bash
# Fail when a crate that ships in a distribution is missing from that
# distribution's THIRD_PARTY_LICENSES.txt. Compares crate names only: a version
# bump does not fail, a new dependency does. See docs/THIRD_PARTY_LICENSES.md.
#
# Usage: check_third_party_notices.sh
set -euo pipefail
export LC_ALL=C

cd "$(dirname "$0")/../.."
root="$PWD"
status=0

# check <workspace dir> <notice file> <cargo tree package selection...>
check() {
  local workspace="$1" notices="$2" missing
  shift 2
  missing=$(comm -23 \
    <(cd "$workspace" && cargo tree "$@" --all-features -e normal --target all \
        --prefix none --locked -f '{p}' | awk 'NF && $3 !~ /^\(\// { print $1 }' | sort -u) \
    <(grep -oE '^[A-Za-z0-9_.-]+@[0-9]' "$root/$notices" | sed 's/@.$//' | sort -u))
  if [ -n "$missing" ]; then
    echo "::error file=$notices::crates missing from $notices: $(echo "$missing" | tr '\n' ' ')"
    status=1
  else
    echo "ok: $notices"
  fi
}

check . python/mq-bridge-py/THIRD_PARTY_LICENSES.txt -p mq-bridge-py
check . node/mq-bridge-node/THIRD_PARTY_LICENSES.txt -p mq-bridge-node
check . c/mq-bridge-c/THIRD_PARTY_LICENSES.txt -p mq-bridge-c
check apps/mq-bridge-app apps/mq-bridge-app/crates/core/THIRD_PARTY_LICENSES.txt --workspace

exit "$status"
