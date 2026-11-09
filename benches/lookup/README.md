# Lookup benchmark: per-message vs batched

**Question:** does a batched, reading `lookup` (`IN (${…})`, one query per batch) pay off even
when clients send single messages, compared with one query per message (`= ${…}`)?

Three measurements against the same Postgres table (1M rows, text primary key):

| Script | Path | Measures |
|---|---|---|
| `pgbench.py` | pgbench → Postgres | upper bound: keys/s the database answers without mq-bridge |
| `run_file.py` | file → mq-bridge (lookup on output) → file | throughput, rows/s |
| `run.py` | oha → mq-bridge (http input → lookup → response) | latency of single requests, from 1 client to overload |

## Run

`run.py` and `run_file.py` read the database URL from `POSTGRES_URL`, set to the compose
database below. `caller/` connects to `postgres://localhost:55432/bench` and takes user and
password from `PGUSER` / `PGPASSWORD`.

```sh
export POSTGRES_URL=postgres://bench:bench@localhost:55432/bench   # run.py, run_file.py
export PGUSER=bench PGPASSWORD=bench                               # caller/
export RESULTS=results/$(date +%Y%m%d-%H%M%S)   # a fresh directory per run; CSVs append
docker compose up -d --wait
mkdir -p "$RESULTS" && { date; uptime; mq-bridge-app --version; oha --version; docker --version;
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize; } > "$RESULTS/meta.txt"
caffeinate -dims python3 pgbench.py              # ~30 min; --duration 2 --reps 1 for a smoke run
caffeinate -dims python3 run_file.py             # 1M-row input, ≤15 s per cell × 8 variants × 5 reps
caffeinate -dims python3 run.py                  # ~40 min; --reps 1 --duration 2s --warmup 1s for a smoke run
python3 summarize.py                             # $RESULTS/summary.md
docker compose --profile pgbench down --volumes
```

**Quick run (~4 min)**: file throughput plus a few HTTP points on loopback; skips pgbench,
whose upper bound only changes with Postgres or the machine.

```sh
caffeinate -dims python3 run_file.py --reps 2 --window 4
caffeinate -dims python3 run.py --reps 1 --duration 3s --warmup 1s --nets loopback \
  --closed 1 64 --rates 5000 20000
```

**Caller vs a query per request (~3 min)**: a Rust program in `caller/` that fetches one user
per request either straight with sqlx or through `Caller::call` into `caller/routes.yaml`, from
1 to 4,096 clients, plus `call_many` vs a hand-written `= ANY($1)`. Prints a markdown table.

```sh
docker compose up -d --wait
cd caller && caffeinate -dims cargo run --release   # optional argument: requests per cell, default 50000
```

Needs `mq-bridge-app`, `oha` (`brew install oha`) and Docker. pgbench runs inside containers
from the `postgres:17.6` image, nothing is installed on the host. Results go to
`$RESULTS` (default `results/current/`, git-ignored). The pgbench and file CSVs append, so
reusing a directory mixes runs.

## Protocol

- Announce the run, check the machine is idle (1-minute load < 2.5, no `rustc`), keep it awake
  with `caffeinate`, stop the containers afterwards.
- Record versions, hardware and load in `$RESULTS/meta.txt` before the run.
- Each repetition shuffles the cells (`--seed`), spreading slow drift (thermals, background
  load) across variants. Reported: median over repetitions with [min–max].

## Design

**pgbench** (`pgbench.py`): the bridge's queries, keys drawn uniformly at random.
- `single_prepared` / `single_extended`: `= key` with a cached statement vs an unnamed one
  prepared on every call. `inN`: `IN (k1…kN)`; N ≤ 128 extended protocol, larger lists simple
  protocol (pgbench binds at most 255 variables).
- Clients 1, 4, 16, 64; reported as keys/s = transactions/s × N.
- `port`: a second container connects through `host.docker.internal:55432`, crossing Docker's
  port forwarding as the bridge on the Mac does. `in_pg`: pgbench inside the pg container.

**file** (`run_file.py`): JSONL input with random keys (`--seed`), route concurrency 1,
`exit_on_empty`. Throughput is taken from 1 % of the input written until 90 % or `--window`
(15 s), so startup and the last partial batch drop out and slow variants need not finish.
Every written line is checked: same key and order as the input, with its matching `user`.
- `no_lookup`: the same route without lookup, the ceiling of the file path. It finishes in
  ~0.3 s, so with 10 ms polling its figure is coarse (±4 %); it only shows the headroom.
- `per_message_c16` / `c64`: `= ${payload:path}`, lookup `concurrency` 16 / 64, batch 512.
- `batched_bN`: `IN (${payload:path})`, route `batch_size` N = 1, 16, 128, 512, 1000
  (1000 is the per-query key limit). `b1` shows the batched path without any batching.

**HTTP** (`run.py`):
- Variants: `per_message_c16` / `per_message_c64` (lookup `concurrency` 16 / 64),
  `batched_rc1` / `batched_rc4` (route `concurrency` 1 / 4). All use `batch_size: 512` and a
  pool of 64 connections. The batch size is a ceiling: a batch holds the requests that are
  waiting, so a single client always produces batches of one.
- **Closed load** (`closedN`): N clients, each sends its next request when the last answer
  arrives. Shows latency at a given parallelism.
- **Open load** (`openR`): a fixed rate R req/s over 256 connections with
  `--latency-correction`, so a stalled server is charged for the requests it delayed
  (no coordinated omission). Shows where a variant saturates.
- **Network**: `loopback`, plus `rtt+0.5ms` via `tc netem` in a sidecar sharing Postgres'
  network namespace. netem adds timer overhead, so `300us` was chosen by calibration: it
  measured +0.49 ms on a pgbench `SELECT 1` (0.047 → 0.533 ms).
- **Keys**: uniformly random `/u/000000`–`/u/999999`. A 512-key batch holds ~0.13 expected
  duplicates, so batching gains nothing from de-duplication.

## Limitations

- Client, bridge and Postgres share one machine and compete for CPU; Postgres runs in Docker
  Desktop's VM, reached through its port forwarding.
- pgbench `port` runs inside the VM and only approximates the bridge's path from the Mac.
- One workload shape: single-row point lookups by primary key, small rows.
- At low open rates p99.9 rests on few samples; read the min–max ranges.
