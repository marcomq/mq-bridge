# $12 server challenge: what does mq-bridge cost per request?

**Question:** how close does a config-only mq-bridge route come to the
[$12 server challenge](https://github.com/arjaythedev/twelve-dollar-server-challenge)
(a small feed API on SQLite, 1 vCPU), and where does its CPU go?

`route.yaml` serves the challenge's API with no code: an `http` input, a `when` switch on
method and path, a sqlx `lookup` per endpoint, and a `static` reply. SQLite builds each
response body itself with `json_object`, so key order and bytes match the spec, and the
`static` output sends it with `${payload:j | raw}`.

| Path | Query | Reply |
|---|---|---|
| `GET /feed` | 20 newest posts with like counts | 200 |
| `GET /posts/:id` | one post; `lookup.found` false → 404 | 200 / 404 |
| `POST /posts` | `INSERT … RETURNING json_object(…)` | 201 |
| `POST /posts/:id/like` | `INSERT … ON CONFLICT DO NOTHING RETURNING` | 201 / 200 |
| other | | 404 |

## Run

```sh
mkdir -p .work && cd .work && git init -q challenge && cd challenge \
  && git remote add origin https://github.com/arjaythedev/twelve-dollar-server-challenge \
  && git fetch -q --depth 1 origin c03d64cb2edb932fb59efc274b32ea7eb0a7c992 && git checkout -q FETCH_HEAD \
  && bash seed/make-seed.sh && cd ../..          # ~1 min, needs node and sqlite3
caffeinate -dims python3 run.py --duration 6s --warmup 2s
caffeinate -dims python3 decompose.py
```

Needs `mq-bridge-app` (`MQB_APP` overrides the binary) and `oha`. Every variant starts from a
fresh copy of the seed database. Announce the run and check the machine is idle first.

**Measure:** the bridge process's CPU time (user + sys; `ps cputime`, `/proc/<pid>/stat` on Linux) over a 6 s closed load
of 64 connections, divided by the requests answered. This is the leaderboard's µs/req column
and does not depend on core count. `mix` weights the endpoints like one k6 user loop: feed +
post + 0.15 like + 0.02 create. A variant with an endpoint under 99 % 2xx gets no `mix`.
`post` reads the seed feed's 20 posts, as the load test does; the results below predate that
and used random ids from 10,000–99,999.

## Results (2026-10-31, Apple M1, mq-bridge-app 1.2.0)

`run.py`, µs CPU per request:

| Variant | feed | post | like | create | mix |
|---|---|---|---|---|---|
| `rc1_pool1` (route concurrency 1, 1 connection) | 91 | 53 | 50 | 46 | **70** |
| `rc1_pool4` | 181 | 73 | 52 | 47 | 121 |
| `rc4_pool4` | 183 | 87 | 63 | ¹ | n/a |

¹ 85 % of creates failed: concurrent writers on 4 connections got `database is locked`, the
route went into its 5 s reconnect, and every request in that window failed.

Where the CPU of a feed request goes (`rc1_pool1`, 91 µs):

| Part | µs | Source |
|---|---|---|
| HTTP in/out, inline fast path (hyper + tokio) | 19 | `decompose.py` `static_inline` |
| Route pipeline + `lookup` + `switch` | ~5 | `lookup_static`, `switch3_static` |
| SQLite executing the feed query | 33 | Python `sqlite3`, same SQL, cached statement |
| sqlx (`Any` driver, worker-thread hop, row → JSON → template) | ~35 | remainder |

A lookup on the input costs the same CPU as on the output (`feed_lookup_input` 90 µs,
`feed_lookup_output` 91 µs) with ~10 % more throughput (14.0k vs 12.8k req/s). route.yaml
keeps it on the output because the `switch` picks the query.

Raw SQLite, for comparison: post by id 10 µs; create 51 µs CPU / 86 µs wall with
`synchronous=FULL` (sqlx's default), 30 / 34 µs with `NORMAL` (allowed by the challenge).

## Reading

- The leaders spend 25–31 µs per request in total, on a 2.4 GHz Xeon. mq-bridge's HTTP
  floor alone is 19 µs on an M1, so a top place is out of reach.
- 70 µs on an M1 is roughly 90–125 µs on the challenge box, ~8–11k req/s, about
  85–115k users by the leaderboard's users-per-req/s ratio: around the Node/PHP entries,
  below Go. A rough conversion, not measured on the box.
- Group commit works without a buffer: route concurrency 1 hands the switch every request
  that is waiting, and a writing sqlx lookup on SQLite runs that batch in one
  `BEGIN IMMEDIATE` transaction. `buffer` only adds something at route concurrency > 1,
  and that is where SQLite's writer lock breaks the route today.

## Not covered by route.yaml

- JWT: there is no middleware to verify HS256 tokens. route.yaml takes the user from
  `x-user-id` / `x-username` headers.
- Validation: an invalid id gives 404 instead of 400; a too-long or empty body fails as a
  lookup error instead of 400; liking a missing post answers 200 instead of 404; `/health`.

## Rust handler prototype (`server/`)

The same API as one mq-bridge `Handler` on an `http` input and a `response` output, built
with only the `http` and `sqlx` features plus mimalloc. It verifies the HS256 JWT on every
write and passes the challenge's `test/test.sh` (42/42). `handle_many` gets every waiting
request as one batch and runs it in a single transaction (`BEGIN IMMEDIATE` when it holds a
write), so writes are group-committed and every reply waits for the commit. It uses native
`sqlx-sqlite` on one connection with WAL + `synchronous=NORMAL`.

```sh
cargo build --release --manifest-path server/Cargo.toml   # line tables, not stripped
caffeinate -dims python3 server_bench.py                  # INLINE=0 routed, INLINE=1 forces inline
cp .work/challenge/seed/feed.db .work/t.db && SQLITE_PATH=.work/t.db PORT=13002 \
  ../../target/release/twelve-dollar-server &             # then: bash .work/challenge/test/test.sh http://127.0.0.1:13002
```

| Variant | feed | post | like | create | mix |
|---|---|---|---|---|---|
| route.yaml `rc1_pool1` | 91 | 53 | 50 | 46 | 70 |
| handler, inline, one request at a time (before `prefers_batches`) | 89 | 61 | 90 | 92 | 76 |
| handler, routed (`INLINE=0`) | 47 | 21 | 38 | 31 | 34 |
| handler, inline + `prefers_batches` | 47 | 20 | 36 | 30 | 34 |
| + leaders' query tweaks (current) | **41** | **20** | **35** | **30** | **31** |

- **The HTTP inline fast path used to skip batching.** An `http` → `response` route answered
  each request through `Handler::handle` on its own, so `handle_many` never saw more than
  one message, and each request paid BEGIN, query and COMMIT as three hops to the sqlx
  worker thread. A `buffer` middleware did not help: it sits inside the handler and batches
  what the handler emits, not what it receives (inline + buffer measured 78 µs).
  `Handler::prefers_batches` now batches on the inline path itself, as fast as the routed
  pipeline.
- On one connection there is nothing to batch: `prefers_batches` then costs ~4 µs CPU per
  request (post 62 vs 58 µs, `NOBATCH=1` turns it off), p99 latency unchanged.
- 31 µs on an M1 against the leaders' 25–31 µs on a 2.4 GHz Xeon: the same league, not
  measured on the challenge box.
- What is left in the batched feed profile: SQLite executing the query, socket
  `send`/`recv`, and building JSON in SQLite. Aggregating `json_object` directly instead of
  re-parsing it with `json(x)` saved ~3 µs.
- Query tweaks taken from the leaders: `json_group_array` without its own `ORDER BY` (the
  subquery is already sorted; the extra sort cost ~6 µs per feed), and a like is one
  `INSERT … SELECT … WHERE EXISTS (post)` that checks existence only when nothing was
  inserted. Their feed trick (newest ids, then posts and like counts as one id range) is
  slower as a single SQL statement here (41 vs 30 µs in raw SQLite): it pays off only when
  the join happens in app code.

The k6 load test was not run.

## Submission (`submission/rust-mqbridge-marcomq/`)

The prototype without the bench switches (`INLINE`, `NOBATCH`) and with tokio's default
worker count, plus `install.sh` / `build.sh` / `start.sh` and a README, laid out as the
challenge expects. `/health` answers 503 `degraded` when SQLite fails, as the spec asks.
It needs mq-bridge 1.3.0 from crates.io (for `prefers_batches`), so before opening the PR:

1. Release mq-bridge 1.3.0.
2. In the folder, `cargo generate-lockfile` and keep `Cargo.lock` (`build.sh` uses `--locked`).
3. Copy the folder to `submissions/` in a fork of the challenge and run
   `bash test/run.sh submissions/rust-mqbridge-marcomq` (42/42).

To test before the release, copy the folder elsewhere and add
`.cargo/config.toml` with `[patch.crates-io] mq-bridge = { path = "<this repo>" }`.
