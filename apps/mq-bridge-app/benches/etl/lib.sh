#!/usr/bin/env bash
# Shared helpers for the ETL/CDC benchmark harness. Source this from the runners.
set -euo pipefail

# --- Fixed connection + workload parameters (match the compose + the brief) ---
export PGHOST="${PGHOST:-localhost}"
export PGPORT="${PGPORT:-5432}"
export PGUSER="${PGUSER:-testuser}"
export PGPASSWORD="${PGPASSWORD:-testpass}"
export PGDATABASE="${PGDATABASE:-testdb}"
PG_URL="postgres://${PGUSER}:${PGPASSWORD}@${PGHOST}:${PGPORT}/${PGDATABASE}"
export PG_URL

# Workload knobs (overridable from the environment).
export MSG_COUNT="${MSG_COUNT:-1000000}"    # rows per run
export WARMUP_COUNT="${WARMUP_COUNT:-5000}" # pre-roll rows, excluded from timing
export METRICS_ADDR="${METRICS_ADDR:-127.0.0.1:9090}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export HERE
REPO_ROOT="$(cd "$HERE/../../../.." && pwd)"
export REPO_ROOT

# Honour CARGO_TARGET_DIR if the caller shares a target dir across repos, or
# points the harness at a second build (see README: default vs. lean). A relative
# value is resolved against REPO_ROOT, not the runner's cwd, so BIN is the same
# path no matter where the script was invoked from.
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$CARGO_TARGET_DIR" in
  /*) ;;
  *) CARGO_TARGET_DIR="$REPO_ROOT/$CARGO_TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR

# Path to the built lean benchmark binary (see README: cargo build --features bench).
export BIN="${BIN:-$CARGO_TARGET_DIR/release/mq-bridge-app}"

# Path to the Sling CLI — a compiled (Go) EL baseline alongside Meltano. Kept
# repo-local so a run never depends on what's on $PATH. Install with:
#   mkdir -p benches/etl/bin && curl -sL \
#     "https://github.com/slingdata-io/sling-cli/releases/latest/download/sling_darwin_arm64.tar.gz" \
#     | tar -xz -C benches/etl/bin sling
export SLING_BIN="${SLING_BIN:-$HERE/bin/sling}"

# Count JSONL rows at a path that may be a single file or a directory of parts
# (Sling splits its output into part files unless file_max_rows is 0).
landed_rows() {
  local p="$1"
  if [[ -d "$p" ]]; then
    find "$p" -type f -exec cat {} + 2>/dev/null | wc -l | tr -d ' '
  elif [[ -f "$p" ]]; then
    wc -l < "$p" | tr -d ' '
  else
    echo 0
  fi
}

# psql shorthand against the benchmark database. Uses a host `psql` if present,
# otherwise runs psql inside the compose container (so no host client is needed).
PG_CONTAINER="${PG_CONTAINER:-postgres-etl-bench-mq-bridge}"
psql_q() {
  if command -v psql >/dev/null 2>&1; then
    psql "$PG_URL" -v ON_ERROR_STOP=1 -qtA "$@"
  else
    docker exec -i "$PG_CONTAINER" psql -U "$PGUSER" -d "$PGDATABASE" -v ON_ERROR_STOP=1 -qtA "$@"
  fi
}

# Wait until Postgres answers.
wait_for_pg() {
  local tries=30
  until psql_q -c 'SELECT 1' >/dev/null 2>&1; do
    ((tries--)) || { echo "postgres never became ready" >&2; return 1; }
    sleep 1
  done
}

# Environment header — CPU model / cores / RAM / OS / mq-bridge(-app) version.
# Printed next to every results block so nothing is published without methodology.
print_env_header() {
  local cpu cores ram os appver enginever
  if [[ "$(uname)" == "Darwin" ]]; then
    cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
    cores="$(sysctl -n hw.ncpu 2>/dev/null || echo '?')"
    ram="$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1024 / 1024 / 1024 )) GiB"
  else
    cpu="$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo unknown)"
    cores="$(nproc 2>/dev/null || echo '?')"
    ram="$(( $(grep -m1 MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}') / 1024 / 1024 )) GiB"
  fi
  os="$(uname -sr)"
  appver="$(cd "$REPO_ROOT" && awk '/^\[workspace.package\]/{f=1; next} f&&/^version =/{gsub(/"/, "", $3); print $3; exit}' Cargo.toml)"
  enginever="$appver"
  cat <<EOF
**Environment:** ${cpu} · ${cores} cores · ${ram} · ${os} · mq-bridge-app ${appver} · engine ${enginever} · postgres:16-alpine (wal_level=logical)
EOF
}

# Scrape a Prometheus summary quantile for a metric from the running app's exporter.
# Usage: scrape_quantile <metric> <quantile> [label=value ...]
scrape_quantile() {
  local metric="$1"; local q="$2"; shift 2
  local url="http://${METRICS_ADDR}/metrics"
  curl -s "$url" | awk -v m="$metric" -v q="$q" '
    $0 ~ "^"m"{" && $0 ~ "quantile=\""q"\"" { print $NF; found=1 }
    END { if (!found) print "NaN" }'
}

# --- Shared app-process helpers (used by the run_*.sh scenario scripts) ---

# Bail with a clear message if the lean bench binary hasn't been built.
require_bin() {
  [[ -x "$BIN" ]] || { echo "binary not found at $BIN — build with --features bench --release" >&2; exit 1; }
}

# Start the app in the background against a --config file; prints its PID.
# Usage: pid="$(start_app config.yaml app.log)"
start_app() {
  local config="$1" log="$2"
  "$BIN" --config "$config" >"$log" 2>&1 &
  echo $!
}

# Poll a UI's /health endpoint until it answers, or fail after ~30s.
wait_health() {
  local addr="$1" log="${2:-}" tries=30
  until curl -fs "http://${addr}/health" >/dev/null 2>&1; do
    ((tries--)) || { echo "UI at ${addr} never came up${log:+ (see $log)}" >&2; return 1; }
    sleep 1
  done
}

# --config loads a route but does NOT start it — this is the zero-code
# "Start" action (POST /consumer-start), the same thing clicking Start in
# the UI does. (POST /config only validates+saves; it never starts routes.)
start_consumer() {
  local addr="$1" id="$2"
  curl -fs -X POST "http://${addr}/consumer-start?consumer_id=${id}" >/dev/null \
    || { echo "POST /consumer-start failed for ${id} on ${addr}" >&2; return 1; }
}

# Kill one or more PIDs, escalating SIGTERM -> SIGKILL. Ignores already-exited
# processes. Usage: trap 'kill_pids "$PID1" "$PID2"' EXIT
#
# The escalation is not optional: mq-bridge-app traps SIGTERM to shut routes down
# gracefully, so a wedged route swallows the TERM and the process survives. A
# TERM-only kill in an EXIT trap then leaks a running app into the next scenario,
# which contends for the same ports and files.
kill_pids() {
  local pid
  for pid in "$@"; do
    [[ -n "$pid" ]] || continue
    kill -TERM "$pid" 2>/dev/null || true
  done
  # Give the graceful path a moment, then make sure they are actually gone.
  local waited=0
  while ((waited < GUARD_GRACE * 10)); do
    local alive=0
    for pid in "$@"; do
      [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && alive=1
    done
    ((alive)) || return 0
    sleep 0.1
    # Assignment, not `((waited++))`: a post-increment evaluates to the *old*
    # value, so at waited=0 the arithmetic result is 0 and `(( ))` exits 1 —
    # which under `set -e` killed the caller on the first iteration.
    waited=$((waited + 1))
  done
  for pid in "$@"; do
    [[ -n "$pid" ]] || continue
    kill -KILL "$pid" 2>/dev/null || true
  done
  true
}

# Seconds a process gets to honour SIGTERM before it is SIGKILLed.
export GUARD_GRACE="${GUARD_GRACE:-5}"
# Exit code reported when the watchdog had to kill the command, matching timeout(1).
export GUARD_TIMEOUT_RC=124

# Run a command with a watchdog timeout so a stuck/misconfigured route can't hang
# a benchmark matrix. Returns GUARD_TIMEOUT_RC (124) if it had to be killed,
# otherwise the command's own exit code.
# Usage: run_guarded 900 "$BIN" copy --from ... --to ...
#
# Two details are what make this actually terminate, and both were missing from
# the earlier version that hung the matrix repeatedly:
#
#   1. SIGTERM is escalated to SIGKILL after GUARD_GRACE. A TERM-only watchdog is
#      useless against mq-bridge-app's graceful-shutdown handler when a route is
#      wedged -- the signal is absorbed, the process lives, and the `wait` below
#      blocks forever. The watchdog fires and accomplishes nothing.
#   2. The command runs in its own process group (`set -m` makes it the leader,
#      so pgid == pid) and signals go to the whole group via `kill -- -$pid`.
#      Signalling only the direct child orphans everything it spawned -- meltano's
#      tap|target pipeline being the case that bit us.
#
# stdin is /dev/null: with job control on, a background job in its own process
# group that reads the terminal gets SIGTTIN and stops, which is another silent hang.
run_guarded() {
  local timeout="$1"; shift
  local marker; marker="$(mktemp -t mqb_guard.XXXXXX)"
  local pid rc=0

  set -m
  "$@" </dev/null >/dev/null 2>&1 &
  pid=$!

  # The watchdog gets its own process group (still under `set -m`) AND its own
  # /dev/null stdio. Both are load-bearing, and their absence was a real hang:
  #
  #   - Killing only the subshell leaves the `sleep` it forked alive. That orphan
  #     inherits whatever stdout the script had, so if the harness output is piped
  #     or captured, the orphan holds the write end of the pipe open and the reader
  #     never sees EOF. A 2-second benchmark then appears to hang for the full
  #     timeout (200s per sample at 1M rows, and every sample leaves another one).
  #     Killing the whole group takes the `sleep` with it.
  #   - Redirecting to /dev/null means that even if a stray descendant does survive,
  #     it is not holding a descriptor the caller is waiting on.
  {
    sleep "$timeout"
    printf timeout > "$marker"
    kill -TERM -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
    sleep "$GUARD_GRACE"
    kill -KILL -"$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null
  } </dev/null >/dev/null 2>&1 &
  local killer=$!
  set +m

  wait "$pid" 2>/dev/null || rc=$?
  kill -KILL -"$killer" 2>/dev/null || kill -KILL "$killer" 2>/dev/null || true
  wait "$killer" 2>/dev/null || true

  # A non-empty marker means the watchdog fired, which is unambiguous; inferring
  # it from a 128+n exit code would confuse a real signal-death for a timeout.
  if [[ -s "$marker" ]]; then rm -f "$marker"; return "$GUARD_TIMEOUT_RC"; fi
  rm -f "$marker"
  return "$rc"
}

# Run one timed sample under the watchdog, aborting the scenario if it times out.
# A timeout must never be recorded as a measurement: the old scripts passed
# `|| true` and then timed the kill, silently publishing the watchdog interval as
# a throughput number.
# Usage: guarded_sample <timeout> <label> <cmd...>
guarded_sample() {
  local timeout="$1" label="$2"; shift 2
  local rc=0
  # Announced before it starts, not after it finishes: tool output is sent to
  # /dev/null, so without this a multi-minute run shows nothing at all and is
  # indistinguishable from a hang.
  printf '  %s (timeout %ss) ... ' "$label" "$timeout"
  run_guarded "$timeout" "$@" || rc=$?
  ((rc == 0)) && printf 'ok\n' || printf 'rc=%s\n' "$rc"
  if ((rc == GUARD_TIMEOUT_RC)); then
    echo "  FAILED: ${label} exceeded ${timeout}s and was killed — no usable timing." >&2
    echo "  (raise the timeout if the machine is just slow; otherwise the route is wedged)" >&2
    return 1
  fi
  return 0
}

# --- Timing helpers, shared by the per-tool scenario scripts ---

now() { python3 -c 'import time; print(time.time())'; }

median_stddev() {
  python3 -c "
import statistics as s
xs=[${1}]
print(f'{s.median(xs):.3f} {s.pstdev(xs):.3f}' if len(xs) > 1 else f'{xs[0]:.3f} 0.000')
"
}

# Watchdog budget scaled to the row count rather than a flat wall-clock number:
# guard_budget <seconds-per-row> <floor>. See [[bench-harness-timeout-contract]].
guard_budget() { python3 -c "print(max($2, int($ROWS * $1)))"; }

# Create the results CSV with a header if it doesn't exist yet. Per-tool scripts
# append to a shared file, so running one alone adds a row instead of truncating
# the other tools' results.
results_init() {
  local f="$1"
  mkdir -p "$(dirname "$f")"
  [[ -f "$f" ]] || echo "tool,rows,repeats,median_elapsed_s,stddev_elapsed_s,median_rows_per_s" > "$f"
}

# Drop any previous row for this tool, so a re-run replaces rather than duplicates.
results_drop_tool() {
  local f="$1" tool="$2" tmp
  [[ -f "$f" ]] || return 0
  tmp="$(mktemp)"
  awk -F, -v t="$tool" 'NR==1 || $1 != t' "$f" > "$tmp" && mv "$tmp" "$f"
}

# One full timed cell: warmup + $REPEATS samples, row-count check on every sample,
# median/stddev, one row appended to $RESULTS_CSV.
#   bench_tool <label> <landed-path> <timeout> <cmd...>
# <cmd...> may be a shell function; it must be idempotent (clear its own output).
#
# A short row count aborts rather than averaging in a truncated run, and a timeout
# aborts via guarded_sample — a killed run must never become a throughput number.
bench_tool() {
  local label="$1" landed_path="$2" timeout="$3"; shift 3
  local samples=() i t0 t1 elapsed landed
  # Output lines, which is $ROWS for a JSONL sink but not for every format — a CSV
  # sink writes a header line too. Set EXPECT_LINES around the call for those.
  local expect="${EXPECT_LINES:-$ROWS}"
  # How to count what landed. The default counts lines, which is meaningless for a
  # compressed or otherwise binary sink; set LANDED_FN to a function that prints the
  # row count for those. The check itself is never skipped — a format we cannot
  # count is a format we cannot publish a rate for.
  local counter="${LANDED_FN:-landed_rows}"

  echo "-- ${label}: warmup"
  guarded_sample "$timeout" "${label} warmup" "$@" || return 1

  for ((i = 1; i <= REPEATS; i++)); do
    t0="$(now)"
    guarded_sample "$timeout" "${label} run $i" "$@" || return 1
    t1="$(now)"
    landed="$("$counter" "$landed_path")"
    [[ "$landed" == "$expect" ]] || {
      echo "  FAILED: ${label} run $i landed ${landed} != expected ${expect}" >&2; return 1; }
    elapsed="$(python3 -c "print(f'{$t1-$t0:.6f}')")"
    echo "  ${label} run $i: ${elapsed}s"
    samples+=("$elapsed")
  done

  local median stddev rate
  read -r median stddev <<<"$(median_stddev "${samples[*]/%/,}")"
  rate="$(python3 -c "print(int($ROWS/$median))")"
  results_init "$RESULTS_CSV"
  results_drop_tool "$RESULTS_CSV" "$label"
  printf '%s,%s,%s,%s,%s,%s\n' "$label" "$ROWS" "$REPEATS" "$median" "$stddev" "$rate" \
    | tee -a "$RESULTS_CSV"
}
