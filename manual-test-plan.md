# Manual test plan: Unreleased changes since `474c3da4`

## Context

Four commits (`29b5fedc` core review & audit, `f3790e68` readiness part 2, `acb4e815` and
`96072bb8` coderabbit) changed 69 files. All of it is unit-tested; this plan is a manual pass
through a real `mq-bridge-app` process, one short check per user-visible entry of the
`Unreleased` section in [CHANGELOG.md](CHANGELOG.md). The first version of
this plan covered only the last three fixes; this one covers the whole section.

Depth: one row per change, a single observable result each. No code changes; nothing is
committed or staged.

## Setup

- Build once from the repo root: `cargo build --manifest-path apps/mq-bridge-app/Cargo.toml`
  with the app's default features (needs `http`, `sqlx`, `dedup` at least). Use
  `mq-bridge-app`, not the `mqb` shim; confirm the run flag with `--help` (`run -c file.yaml`).
- Scratch directory `/tmp/mqb-manual/`. Input `in.jsonl`: 1,000 lines `{"id": n, "amount": n}`.
- Unless a row says otherwise: `file` input, `file` or `null` output, run with `--drain`.
- Sections 1–6 need no broker. Section 7 needs Docker (Kafka, Postgres, MongoDB).

## 1. Config validation at start (all: expect a clear error and a non-zero exit)

| # | Config | Expected |
|---|---|---|
| 1.1 | `retry` on an input; then `dlq` on an input | Start fails, message says to move it to the output |
| 1.2 | `batch_size: 100000001`; `concurrency: 1000001`; `reconnect_interval_ms: 0`; `startup_timeout_ms: 0` | Each refused at start |
| 1.3 | `batch_size: 20000`, `concurrency: 10000` | Starts, logs a warning about the product |
| 1.4 | `batch_size: 50000`, concurrency 4, tiny input | Starts, memory stays flat (no full-batch reservation) |
| 1.5 | Top-level `config:` key next to route keys | Refused as ambiguous |
| 1.6 | Run a route name / publisher name that is not in the file | Error lists the names the file has |
| 1.7 | Typo in a field of `transform`, `lookup`, `sequence` or `http_bulk` | Error names the wrong field, not "unknown custom type" |
| 1.8 | Endpoint type that does not exist, with a password in its config | Error names the type only; password not printed |
| 1.9 | `memory` input `url: "ipc://../x"` | Rejected |
| 1.10 | Output `ref` chain that loops back to itself | Reported as a ref cycle, not a fanout recursion |
| 1.11 | `limiter: { messages_per_second: 0.0001 }` | Starts, no panic |
| 1.12 | Endpoint type policy (allowed list) that omits a type used inside a `dlq` or `lookup`; and a `custom` endpoint not on the list | Both refused |

## 2. Secrets and redaction

| # | Step | Expected |
|---|---|---|
| 2.1 | `http` output to `http://127.0.0.1:9/hook?tok%65n=S1&API%5Fkey=S2&id%3D=7&sig=S3&password=S4`, capture all output | URL shown with `***` for the four values, `id%3D=7` untouched; `grep -c 'S[1-4]'` is 0 |
| 2.2 | Save/export a config (UI or CLI) containing a `sequence` input, a `lookup` middleware and a `deduplication`/`aggregate` `store` URL, each with a password | Exported file has no passwords; they are in the secret store |
| 2.3 | Save a config with routes `a-b` and `a_b` | Save fails with "same secret key" |
| 2.4 | `http` output with headers `X-Author`, `X-Keyboard`, `X-Api-Key`, `Authorization`; save | Only the last two are moved out of the config |
| 2.5 | CLI URI `null:|filter?expression=amount%20%3E%20100&on_error=drop` (e.g. via `mqb copy`) | Parsed as the map form; a bare expression still works |

## 3. Route runner

| # | Step | Expected |
|---|---|---|
| 3.1 | `concurrency: 4`, `batch_size: 1`, output `null` with `random_panic: { mode: panic, trigger_on_message: 5 }` | Ends in seconds with "A worker task panicked; its batch was never committed"; no hang |
| 3.2 | 3.1 again with `enabled: false`, `file` output | Completes; no line from the panicked batch onward is missing |
| 3.3 | `concurrency: 4`, `http` output to a closed port, long-running (no `--drain`) | Route fails and reconnects repeatedly; status changes; it is not stuck |
| 3.4 | While 3.3 waits to reconnect (`reconnect_interval_ms: 30000`), stop it (Ctrl-C) | Stops at once, not after 30 s |
| 3.5 | Two routes as in 3.3, stop both | Total stop time is about one timeout, not two; a warning is logged if a route had to be aborted |
| 3.6 | Output `ref` to a publisher that carries `retry` + `dlq` (dlq to a file); make the sink fail with `random_panic: { mode: json_format_error }` | Failed messages land in the dlq file, none silently dropped |
| 3.7 | `static`/`http` input producing a message with `reply_to`, output `null` (acknowledges only) | Message is acked once with a warning naming it; no redelivery loop |

## 4. Filter, deduplication, on_error

| # | Step | Expected |
|---|---|---|
| 4.1 | `file` input, `filter: "amount > 99999"` (drops everything), no `--drain`; append lines, then restart | After restart nothing is re-read: the position moved although every batch was dropped |
| 4.2 | `filter: { expression: "amount.x.y > 1", on_error: drop }` vs. the bare string | Map form logs and drops, route continues; string form stops the route |
| 4.3 | `deduplication` with `key: "${metadata:tenant}-${payload:missing}"` | All 1,000 lines reach the output (keyed on `message_id`), not just the first |
| 4.4 | `deduplication` (sled store) with `delay: 7000 ms` on the output; feed the same id twice within that window | Second copy is not delivered |
| 4.5 | `deduplication` store `sled:///path?durable=true`; run, `kill -9` right after the last output line, rerun same input | No message is delivered twice |
| 4.6 | Inputs with `encryption`, `compression`, `unpack` and one corrupt message each; default, then `on_error` flipped | `encryption` default drops and continues; the other two default to stopping; flipped values do the opposite |
| 4.7 | `compression` or `unpack` on an input without a size limit | One `info` line at start mentions the missing limit |

## 5. Retry, dlq, buffer, pack

| # | Step | Expected |
|---|---|---|
| 5.1 | `retry` + `dlq` on an `http` output against a small local server that accepts some of a batch and fails the rest, then fails the whole retry | Messages accepted on the first attempt are not redelivered or dead-lettered |
| 5.2 | Same server, drop the connection for part of a batch | Connection-failed messages are not in the dlq; they are redelivered after reconnect |
| 5.3 | `buffer` on an output, then `random_panic: { mode: panic, trigger_on_message: 3 }` behind it | That batch fails with a connection error; later sends still go through |
| 5.4 | `pack` on an output to a failing sink | Route reports the failure, never "sent" |

## 6. Memory and Unix IPC

Consumer: `memory` input `url: "ipc:///tmp/mqb-manual/t.sock"` → `file`. Publisher: `file` →
`memory` with the same url.

| # | Step | Expected |
|---|---|---|
| 6.1 | Start a second consumer on the same socket | Fails with "in use by another consumer"; first consumer still receives |
| 6.2 | `kill -9` the consumer (socket file stays), start again | Starts; stale file replaced |
| 6.3 | `kill -STOP` the consumer, start a second one | "in use" in under a second, no block; `kill -CONT` restores the first |
| 6.4 | `touch` a regular file at the socket path, start a consumer | Starts or reports "in use"; no hang. Note which |
| 6.5 | Socket in `/tmp` directly (`ipc:///tmp/t.sock`), then `ls -ld /tmp` | Mode of `/tmp` unchanged |
| 6.6 | Stop a consumer that is idle in a receive (Ctrl-C) | Exits promptly; socket file is gone afterwards |
| 6.7 | `enable_nack: false` on the input, sink that nacks (`random_panic: { mode: nack }`) | Log says the nack was dropped |

## 7. With brokers (Docker; pick these if time allows)

| # | Step | Expected |
|---|---|---|
| 7.1 | Kafka input, `commit_concurrency_limit: 8`, output with `delay: 60000 ms` | Reading pauses after roughly limit + in-flight batches; lag stays visible instead of unbounded reads |
| 7.2 | Kafka input with a `filter` that drops everything | Consumer-group offset advances; lag does not grow |
| 7.3 | Kafka input, 3.1's panic setup | Route errors out; after restart the uncommitted messages are redelivered |
| 7.4 | Postgres or MongoDB source named `public.orders` on a fresh database | Cursor table is `mqb_cursors_public_orders_<hash>`; an existing old-name table is still used |
| 7.5 | File checkpoint store: run, stop, inspect the cursor file, rerun | Resumes from the saved cursor (the fsync itself is not observable by hand) |
| 7.6 | `lookup` on an input against an HTTP server that answers 401 | Messages are retried / nacked, not acked and dropped |
| 7.7 | Memory publisher with `request_reply` and a slow responder in the same process (two routes in one config) | Batch is acked only after the replies arrive |

## Not covered by hand (unit tests only)

Rust-API or race-only changes with no practical manual trigger: `Middleware::Filter` type
change, `send_batch_helper` ordering, concurrent `Route::deploy`, `CanonicalMessage::from`
with a bad `id`, commit adapter disposition count, publisher-registry lock poisoning,
duplicate-`message_id` handling in `compression`/`encryption`/`buffer`, `buffer` input
sub-batch settling, `unpack` empty-envelope ack, `Publisher::request_batch` error text, memory
event-log `Gap`, `weak_join` receive cancellation, `replay_response` after 1024 duplicate
batches, IPC `close()` during a read/write.

For these, run the targeted suites once: `cargo test --lib` and
`cargo test --lib --features mqtt -- redact::`.

## Reporting

Per row: pass / fail / not run, with the log excerpt for any fail. Blockers: a hang in 3.1,
3.3 or 6.3; a secret found in 2.1 or 2.2; a lost message in 3.2, 4.3 or 5.1.

## Results, 2026-10-11

Debug build of `mq-bridge-app` (`full`), run as `--no-ui --no-metrics --config file.yaml`
or `copy --drain`. Scripts and logs: `/tmp/mqb-manual/`.

| Row | Result | Observed |
|---|---|---|
| 1.1 | pass | "`retry` is an output-only middleware … Move it to the route's output endpoint", same for `dlq`; exit 1 |
| 1.2 | pass | All four refused: "above the limit of …", "must be at least 1" |
| 1.3 | pass | Warning "lets up to 200000000 messages be in memory at once", route completes |
| 1.4 | pass | Completes in 0.2 s |
| 1.5 | n/a in the app | The app warns "Unknown top-level config key 'config' is ignored" (decision 1: A). The refusal is the engine's `config_file` loader; unit-tested |
| 1.7 | pass | `transform`: "Failed to deserialize known middleware 'transform': unknown field `mappping`"; `http_bulk`: "unknown field `presett`" |
| 1.8 | pass | "Custom endpoint factory 'nosuchthing' not found"; the password is not in the output |
| 1.9 | pass | "IPC socket name '../x' must not contain '..'" |
| 1.11 | pass | Starts, no panic (killed after 25 s, as the rate allows one message in 10,000 s) |
| 2.1 | pass | `http://127.0.0.1:9/hook?token=***&API_key=***&id%3D=7&sig=***&password=***`; no `S1`..`S4` in the output |
| 2.5 | pass | Map form and bare form both copy 900 of 1,000 |
| 3.1 | pass | "A worker task panicked; its batch was never committed", reconnect, done in 5.3 s |
| 3.2 | pass | 1,000 of 1,000 lines |
| 3.3, 3.4 | pass | "Reconnecting in 30000ms..." repeated; Ctrl-C ends it in under 0.1 s |
| 3.5 | pass | Two such routes stop in under 0.1 s |
| 4.2 | pass | `len(amount) > 1`: map form warns and drops, exit 0; string form "permanent consumer error", exit 1 |
| 4.3 | pass | 1,000 of 1,000, with "`key` template did not fully resolve; keying on message_id" |
| 4.6 | pass (`compression`, `unpack`) | Default stops with a permanent error, exit 1; `on_error: drop` logs each message and exits 0. `encryption` not run |
| 4.7 | pass | "compression: no max_decompressed_bytes set …", "unpack: no max_messages set …" |
| 5.3 | pass | "Buffered send panicked" as a connection error, reconnect, all lines written (the failed batches twice) |
| 5.4 | pass | "Transient error in batch send (512 messages failed)", exit 1 |
| 6.1 | pass | "is in use by another consumer"; first consumer gets 20 of 20. The app reports it after its 5 s start timeout |
| 6.2 | pass | Stale socket replaced, 20 of 20 |
| 6.3 | pass | "in use" while the first is stopped, no block; after `CONT` 20 of 20 |
| 6.4 | noted | Reports "in use" and retries every 5 s; no hang, the file is left alone |
| 6.5 | pass | `/tmp` stays `1777` |
| 6.6 | pass | Exits at once, socket file gone |
| 6.7 | pass | "Nacked IPC messages dropped: `enable_nack` is off … count=3" |

Not run: 1.6, 1.10, 1.12, 2.2–2.4, 3.6, 3.7, 4.1, 4.4, 4.5, 5.1, 5.2 and section 7. 1.10 and
3.6 need a `ref` target, which a headless YAML cannot register (`publishers:` entries are not
endpoints a route can `ref`); 4.1 needs a source with a stored position, which the plain `file`
input does not have; 2.2–2.4 need the UI's save path; 5.1 and 5.2 need a scripted HTTP server.
All of them have unit tests.
