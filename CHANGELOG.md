# Changelog

All notable changes to `mq-bridge`. Newest first.

## 0.4.12 — unreleased

### Changed

- **`mq-bridge` is now dual-licensed `MIT OR Apache-2.0`.** Every crate, the npm package,
  and the Python wheels carry the new SPDX expression; `LICENSE-MIT` and `LICENSE-APACHE`
  hold the two texts and `LICENSE` points at both. Recipients pick either arm, so nothing
  changes for existing MIT users — the Apache arm adds an express patent grant.

### Added

- **`Route::run_without_resume` runs a route while skipping optional cursor/checkpoint state.**
  Resume setup, warnings, and errors are suppressed for cursor-based ClickHouse, SQL, MongoDB
  change-stream, and object-store sources. `Route::run` is unchanged, and native broker offsets
  and CDC slots are unaffected.
- **`mqb copy --no-resume` explicitly opts out of optional checkpoint state.** Cursor-based
  ClickHouse, SQL, MongoDB CDC, and object-store sources skip resume configuration, warnings,
  and errors when a full copy is intentional. It conflicts with `--resume` and does not alter
  native queue offsets.

### Fixed

- **ClickHouse URL credentials now behave as advertised.** HTTP clients read percent-decoded
  `user:password@` credentials from the endpoint URL, while explicit `username` / `password`
  fields still take precedence, and strip userinfo from the request URL.

## 0.4.11

### Changed

- **The Python and Node packages ship third-party licence notices.** Each wheel and npm
  package now carries a `THIRD_PARTY_LICENSES.txt` covering its bundled Rust dependencies,
  and the Node package gained the `LICENSE` file it was missing. Nothing about the code
  changes; this is the attribution the binary distributions always owed.

- **PyPI publishing is its own release job.** It previously rode along with another
  publish step, so a skip upstream could strand the wheels while the rest of a release went
  out. Splitting it lets the PyPI upload succeed or fail on its own.

## 0.4.10

### Added

- **`dir_spool` endpoint — a crash-safe directory FIFO queue.** Each message becomes a
  payload file plus a JSON metadata sidecar, written to a `.tmp` name and renamed into place
  so a reader never observes a partial chunk (`atomic`, on by default). `naming_pattern`
  templates the chunk name from `{seq}` / `{seq:06}` / `{timestamp}` / `{message_id}`, and
  `fsync` chooses how hard a write works to survive power loss. It is the endpoint to reach
  for when the queue has to be a plain directory that another process — or another vendor's
  tool — can read.

  A flat directory is the wrong shape past a few hundred thousand chunks, so `shard_depth`
  and `shard_width` spread them over subdirectories keyed on the leading digits of the
  sequence number: depth 2 / width 3 writes chunk 1 as `000/000/001.bin`, capping every
  directory at 1000 entries. **Both ends must agree on these** — a consumer descends only as
  far as its own depth, so a mismatch reads the spool as permanently empty, and it warns when
  a scan finds subdirectories it is not configured to enter.

  Producer and consumer take their own lock file (`producer_file` / `consumer_file`), and the
  producer's lock doubles as the "producer finished" signal. `emit_done` is three-valued —
  `never`, `success`, `end` — controlling when the `done_file` is written, and `stop_on_done`
  lets a reader end its stream when it sees one. `dir_spool` is **not idempotent**: every
  write consumes a fresh `{seq}`, so a replay creates a new chunk even when the pattern
  contains `{message_id}`. Pair it with downstream deduplication or an idempotent sink.

- **`object_store` reads and writes local directories.** The `fs` backend is now enabled, so
  the same endpoint that talks to S3, GCS and Azure also works against a plain path —
  `file:///var/lib/mqb/incoming` in YAML, or `local-store://` on the CLI, which distinguishes
  it from the single-file `file` connector.

- **Conda packaging for `mq-bridge-app`.** A recipe and a release workflow publish the app
  through conda alongside the existing Homebrew and cargo-binstall paths.

### Security

- **AEAD nonces are now counter-based, not random.** A random 96-bit nonce caps `aes256gcm`
  at the ~2^32 messages of NIST SP 800-38D — under an hour on a busy route, after which a
  repeat leaks the XOR of two plaintexts and can expose the authentication key. Each `Crypto`
  now draws a random prefix once and appends a counter, so nonces are unique for its lifetime
  and the budget is gone. The nonce travels in the envelope verbatim, so the wire format,
  existing at-rest files, and key rotation are all unaffected.

- **`authenticate_metadata` binds metadata to the ciphertext.** Listed metadata keys stay in
  the clear but are folded into the AEAD tag, so altering, removing or adding one in transit
  fails decryption exactly like a tampered payload — no extra field and no second key, since
  the tag already is the checksum. Such envelopes also cover the cleartext header, and are
  tagged version 2 so version 1 envelopes keep opening unchanged. The default is an empty
  list, which is byte-identical to previous releases. Both sides must configure the same
  list; the at-rest `encryption` fields of `file` / `object_store` reject it, since there is
  no per-message metadata at that layer.

### Fixed

- **A Kafka drain no longer loses its tail.** `enable.partition.eof` is now set, and the
  consumer tracks the end-of-partition offset each partition reports. A drain finishes when
  every partition has actually delivered up to its high watermark, rather than when a batch
  happens to come back empty — a buffer still in flight inside librdkafka or the prefetch
  task used to read as "nothing left" and silently truncate the run.

- **A route that fails to start reports the real cause.** The recorded cause is now read
  *before* the task is aborted. Aborting drops the `OutcomeGuard`, which overwrites the cause
  with its panicked-or-aborted fallback as soon as the task is next polled; under load that
  landed first, so an actionable error — an unusable filter expression, say — was replaced by
  a generic one.

- **Five pieces of process-wide state are now reclaimed.** The event store dropped timed-out
  subscribers from its ack math but kept them in the map, so ephemeral subscribers — which
  use a fresh id per instance — grew it with churn. A `stream_buffer` partition created by
  the publish path for a correlation id whose reader never arrived was only ever removed by a
  consumer's shutdown, holding its buffered batches until the process exited; the new
  `idle_ttl_secs` discards those, never one with a consumer attached. The cookie jar had no
  expiry at all, so a server rotating cookie names grew it without bound; `Max-Age=0` now
  deletes, and `max_cookies` (default 256) caps it. The checkpoint path-lock map is pruned
  like the file one already was.

- **A cancelled memory request-reply no longer wedges its correlation id.** The waiter was
  removed on every error path but not when the `send()` future itself was dropped, as route
  shutdown or an outer timeout does. Since duplicate registrations are rejected, a
  caller-supplied correlation id was then unusable for the life of the process. Cleanup is
  now an RAII guard, so it covers cancellation too.

### Changed

- **Filter and `switch` expressions infer the numeric cast.** Comparing a text-typed field
  against a numeric literal now reads it as a number, so `meta.retry_count > 3` and
  `amount >= 50` work on metadata, CSV, and a SQL source's `numeric`/timestamp columns without
  `number()`. The literal is what decides, so `zip == "01234"` still compares as text. `number()`
  is unchanged and stays required where no literal names the intent (`meta.a > meta.b`,
  `amount > 100 * 2`); text that is not a number still fails with the hint that names the field.
  A side effect is that `meta.retries == 3` now matches at all — it previously compared a string
  against a number on the fast path and could never be true.

- **`zen-expression` upgraded from 0.55 to 2.0.** This is the engine behind `filter` and
  `switch` expressions. Expressions that worked before are expected to keep working; the
  numeric-cast inference above is the behaviour change worth re-reading a config for.

- **Less allocation on hot paths.** The metrics middleware resolved its counter and histogram
  from the registry — allocating both label strings — on every message; the handles are now
  built once per wrapped endpoint. (Recorders must therefore be installed before routes
  start.) The encryption middleware keeps failure-path plaintexts in a `Vec` rather than
  building a `HashMap` per batch, `retry` moves the batch on its final attempt instead of
  deep-copying metadata for a re-send that cannot happen, and `weak_join` expires groups in
  one pass without cloning keys.

## 0.4.9

### Fixed

- **Completed drain routes no longer retain stale connection failures.** A finite source that
  finishes before its ready signal or recovery timer is processed now reports a healthy,
  successful terminal status. Errors for messages dropped without a DLQ remain visible.

## 0.4.8

### Added

- **Dynamic gRPC sinks.** The descriptor keys now work on a route's `output` too: a unary method
  makes one call per message and returns the reply as its response, a client-streaming method
  streams a batch into one call. Server-streaming and bidirectional methods, and `request`, are
  rejected there. Status codes that mean "never going to succeed" are non-retryable, so such a
  message is dead-lettered instead of replayed.

- **gRPC server reflection, on both sides.** `reflection: true` discovers descriptors from the
  remote server instead of a descriptor file, and server mode hosts reflection v1 and v1alpha,
  so `grpcurl` can introspect an embedded server.

- **Metadata and credentials for dynamic gRPC calls.** `metadata`, `binary_metadata`,
  `bearer_token`, and `api_key`/`api_key_name`, sent on the reflection call as well. They never
  appear in errors or logs, and are startup errors on Bridge and server mode rather than a silent
  unauthenticated connection — authenticate that with TLS client certificates.

- **Separate gRPC deadlines.** `connect_timeout_ms`, `request_timeout_ms`,
  `idle_stream_timeout_ms` (retryable: the route reconnects), and `overall_timeout_ms` (terminal:
  reconnecting would reset the cap). All default to disabled.

- **`descriptor_set_bytes`** passes a compiled gRPC `FileDescriptorSet` straight from embedded
  Rust callers, with no temporary file.

- **Provenance on dynamic gRPC responses.** A deterministic id per response, plus `grpc.service`,
  `grpc.method`, `grpc.response_index`, and `grpc.ack_guarantee=none` metadata.

- **[docs/GRPC.md](docs/GRPC.md).** The full gRPC guide, including why a generic
  descriptor-driven server is intentionally not implemented. The Bridge contract moved to
  `src/endpoints/grpc/proto/mqbridge/bridge.proto`, unchanged on the wire, and CI now checks a
  generated Python client against server mode.

### Changed

- **Adding the dedicated gRPC deadline fields is a breaking Rust API change for exhaustive
  `GrpcConfig` struct literals.** Downstream Rust callers must specify the new fields or use
  `..Default::default()` / `GrpcConfig::new`. A downstream compile fixture now locks the complete
  literal shape so future additions cannot make this break silently.

- **gRPC `bearer_token` and `api_key` now require an `https://` endpoint.** A credential sent over
  plaintext h2c is rejected rather than put on the wire in the clear. Deployments that terminated
  TLS at a sidecar and spoke plaintext gRPC to it must point the endpoint at `https://`.

- **The gRPC endpoint's `timeout_ms` and `server_streaming` are deprecated but still accepted.**
  `timeout_ms` stays the fallback for connection and request setup; it no longer bounds a dynamic
  stream, which needs the dedicated keys. RPC shape comes from the descriptor, so a disagreeing
  `server_streaming` warns instead of failing startup.

- **gRPC descriptor keys on an `output` select the dynamic publisher.** They were previously
  ignored there and the Bridge publisher was used.

- **gRPC failures preserve code, message, and trailing metadata** in a `GrpcStatusError`. Its
  `Display`/`Debug` omit trailer values so a peer's credentials cannot leak into logs.

- **Filtering and structural forwarding retain bulk writes where possible.** An input `filter`
  now reads additional full source batches after dropping rows until it refills the requested
  batch size; a naturally short source batch remains a flush boundary so live routes do not
  wait indefinitely. A `request` still performs its request/reply calls individually and
  concurrently, unless its `to` endpoint requires ordered publishing; ordered requests run one
  at a time. Results are restored to input order and responses plus error fallbacks are sent to
  `forward_to` in one `send_batch` call. Batch-capable destinations such as MongoDB can
  therefore continue using `insert_many` through both paths.

- **Buffered concurrent routes warn that destination order is not guaranteed.** Buffering
  preserves order within each batch, but with route `concurrency > 1` separate destination
  writes may complete out of source order. Route validation now recommends `concurrency: 1`
  when destination order matters.

## 0.4.7

### Changed

- **The startup failure message distinguishes its two causes.** A route that never became ready
  reports `failed to start: did not become ready within {n}ms`; a route whose task ended before
  signalling ready reports `failed to start: ended before it signalled ready (outcome: ...)`.
  The second case returns immediately and never involved the timeout, so reporting it as one
  sent the reader looking for a stall that never happened. Both now carry the cause the
  reconnect loop recorded, which the old message dropped. Code matching on the old
  `failed to start within {n}ms or encountered an error` text needs updating; the substring
  `failed to start` is unchanged.

### Fixed

- **A route that ran to completion is no longer reported as one that failed to start.** The run
  task signals ready before its consume loop, so a drain over a small source can reach its
  terminal while that signal is still buffered — leaving the reconnect loop's `select!` with two
  ready branches and free to take the terminal one, dropping the startup channel unread.
  `Route::run()` then returned an error for a drain that had already written its output, and
  `mqb copy --drain` exited non-zero on a successful copy. Both terminal arms now forward a
  ready signal that was already emitted.

- **A completed route reports the messages it dropped even after an earlier reconnect failure.**
  The drop report was suppressed whenever `EndpointStatus::error` was already set, including by a
  transient failure the route recovered from. Only a `Failed` route's cause now outranks it.

- **A plugin endpoint that rejects its config no longer reconnects forever.** `create_consumer`
  and `create_publisher` flattened the plugin's status into a bare message, and the reconnect
  loop decides by downcasting to `ConsumerError`/`PublisherError` — so every failure to open
  looked like a connection fault worth retrying. A plugin answering `MQB_ERR_INVALID_CONFIG`,
  `MQB_ERR_UNSUPPORTED`, `MQB_ERR_PERMANENT` or `MQB_ERR_PANIC` now stops the route with that
  cause; `MQB_ERR_CONNECTION` and `MQB_ERR_RETRYABLE` still get the reconnect loop.

## 0.4.6

### Added

- **`filter` middleware.** Keeps only the messages matching an expression and drops the rest,
  on the input or the output: `- filter: "amount > 100"`. Payload fields are read by bare name
  (`amount`, `order.status`), metadata under `meta.` and always as text, so a numeric comparison
  needs a cast (`number(meta.http_status_code) >= 400`); `&&`/`||` and `and`/`or` both work. A
  metadata-only expression never parses the payload. **Prefer the input** — a filter there drops
  the message before the rest of the route touches it and acks it at the source. Requires the
  new `filter` feature (it pulls the `zen-expression` engine), which is part of `middleware` and
  `full` but **not** of `portable`.

- **Predicate mode for `switch`.** `when: [{ if, to }, ...]` routes on the same expression
  language, first match wins, so overlapping thresholds are safe to write and a payload-derived
  key no longer has to be promoted into metadata first. A `switch` uses exactly one of `when` or
  `metadata_key`/`cases`; naming both or neither is a startup error, as is a `when` list in a
  build without the `filter` feature. Value lookup stays the cheaper mode when the key is
  already in metadata.

- **`expression` on `transform`.** A Zen expression producing the output document, applied after
  `mapping` and before `schema` — for values that have to be calculated rather than copied.
  Needs the `zen` feature.

- **A `fanout` can reply.** If one branch produces a response — a `response` or `static` leg, or
  a `request` whose `forward_to` replies — that response goes back to the caller, so a
  request/reply input can fan a message out and still answer. Only one branch may answer: if
  several do, the first in list order wins, and the route warns once rather than on every
  message. A branch that *fails* nacks the whole fan-out, so the caller gets a `500` rather than
  an answer that hides a lost message; opt out per branch with a `request` whose `forward_to`
  does not reply, or a `dlq` on a plain branch.

- **`pass_through_status` on the `http` endpoint.** On a **publisher**, a non-2xx response
  becomes response data instead of a publisher error, so a route can branch on
  `http_status_code` with a `switch` rather than treating every 404 as a failed send. Transport
  failures and unreadable responses are still errors. On a **consumer**, once every output sink
  opts in, a transient sink failure answers the request with `502` instead of stopping and
  reconnecting a non-streaming request/reply route. Composite outputs such as `fanout` need the
  flag on every leaf sink — a mixed output keeps the normal stop-and-reconnect policy — and
  streamable HTTP inputs keep their protocol-specific error frames.

### Changed

- **A middleware that empties a batch no longer acknowledges ahead of the route.** `filter` and
  `deduplication` can filter a batch down to nothing, and acking that straight away jumps ahead
  of batches the route is still writing on a source with cumulative acks — the ordered sequencer
  only sees the commits `receive_batch` returns, so a crash in that window lost them. On those
  sources the emptied batch's commit is now held and runs from inside the next retained batch's
  commit, which the sequencer does order.

- **An `object_store` sink on `name_by: auto` switches to `write_time` names when a row-dropping
  middleware is on the route.** A source-range name covers one *contiguous* run of positions, so
  a batch with holes punched in it would be written as one object per surviving run — a filter
  keeping 80% of rows turns one upload into roughly a hundred. The route logs one line at
  startup saying it made the switch. This covers `filter`, `deduplication`, `weak_join`,
  `transform` with `on_error: reject`, and a `switch` in `when` mode with no `default`. Set
  `name_by: source_position` explicitly to keep replay-safe names and accept the fragmentation.

- **`filter` expressions and a `switch`'s `when` cases are compiled when the config is loaded.**
  An invalid expression is now a startup error naming the expression, instead of a per-message
  failure that only surfaces once traffic arrives.

### Fixed

- **`switch` forwards connect and disconnect hooks to its destinations**, `cases` and `default`
  alike, and one failing teardown no longer skips the rest.

- **A failed `request` whose `forward_to` answers the caller surfaces the error** instead of
  echoing the original message back as a success. The route then nacks (HTTP `500`) and a
  `retry` or `dlq` on the endpoint sees the failure as usual. Forwarding to a plain sink still
  acks, so routing a status with a `switch` is unchanged.

- **Concurrent MongoDB consumers no longer claim documents out from under each other.** Two
  consumers polling within the same second write the same `locked_until`, which is all the claim
  query had to work with, so one poll could read back documents another had just taken. Each
  claim now carries a unique `claim_token`, and the read-back, unlock, ack and delete are all
  scoped to it — an expired lease that was re-claimed elsewhere is no longer deleted from under
  its new owner.

## 0.4.5

No library changes: CI workflow timeouts and a version bump.

## 0.4.4

### Breaking

- **`idempotency` on the `file` and `object_store` sinks is replaced by `name_by`.** The old
  field described a consequence rather than the thing it set. What it actually chooses is how
  written objects and parts are *named*, and everything else — sortability, replay dedup,
  whether `date_partition` applies, how errors are reported — follows from that name:

  | `name_by` | Name | Consequence |
  | --- | --- | --- |
  | `write_time` | `<uuidv7>.<ext>`, optionally under `YYYY/MM/DD/` | unique per write, sorts by write order |
  | `source_position` | `part-<topic>-<partition>-<start>-<end>.<ext>`, zero-padded, flat | repeats exactly on a replay, sorts by source order |
  | `auto` (new default) | `source_position` where the input carries a replay position, else `write_time` | see below |

  `idempotency: true|false` is still read and still means
  `name_by: source_position|write_time`, so v0.4.x configs keep loading. An explicit `name_by`
  wins over it.

- **An `object_store` sink now defaults to `source_position` naming over a replayable input**
  (Kafka, Postgres CDC, a SQL cursor, a MongoDB change stream, or a file). Key order is the
  only order a bucket has, so this is what makes `mqb copy orders.csv s3://…` come back in
  source order at any `concurrency` without configuring anything — and it is what removed the
  `concurrency > 1` warning from that route. Three consequences to know about:
  - **A re-run into the same prefix writes nothing — where the source repeats its
    positions.** The names repeat, and a repeat is a no-op. That covers a re-read of the same
    file, Kafka offsets, a SQL cursor and a CDC stream. A file source in `subscribe` or
    `group_subscribe` mode is the exception: it stamps a per-run epoch, so its names stay
    distinct across runs and a re-run writes a second copy. Where the no-op does apply it is a
    change of behaviour — the same command used to append a second copy under fresh uuid
    names. Use a fresh prefix, or `name_by: write_time`, if you want the old behaviour. The
    no-op also needs the prefix listing to succeed: a replay whose batches fall on different
    boundaries names different objects, so where the listing is denied the sink can only catch
    a repeat that lands on the exact same name and the rest is written twice.
  - **`date_partition` does not apply** under `source_position`; parts are written flat.
  - **Do not mix the two families in one prefix.** uuid names and `part-…` names do not sort
    correctly against each other. A prefix written by an older version keeps working on its
    own; point the upgraded route at a new prefix if a reader depends on listing order.

- **The `file` sink stays on `write_time` under `auto`** and must be told
  `name_by: source_position` explicitly. There, positional naming turns `path` from a file
  into a *directory* of part files — a change of structure, not of name, and not something to
  derive on the operator's behalf.

- `object_store` `date_partition` is now `Option<bool>` (unset = on, and only for
  `write_time`). Side effect: the "`date_partition` is a publisher-only option" warning used
  to fire for *every* `object_store` consumer, because the field defaulted to `true` and the
  check could not tell that from an explicit setting. It now fires only when the field was
  actually set.

### Changed

- **The `object_store` sink lists its prefix only on the `source_position` write path**, and
  only once per publisher, immediately before its first write rather than when it opens. A
  `write_time` sink never lists at all; a `source_position` restart into a populated prefix
  pays for exactly one `LIST` and then skips the rest of the replay without re-encoding or
  re-uploading. The listing cannot be deferred any further than this: `PutMode::Create` only
  catches a repeat that lands on the same object name, so a replay whose batches fall on
  different boundaries collides with nothing and would otherwise write every record a second
  time under overlapping names. A listing that fails is retried on the next batch, unless it
  failed for a reason retrying cannot clear (a denied `ListObjects`), in which case the sink
  warns once and falls back to same-name-only deduplication.
- **The `concurrency > 1` warning now fires only for a `write_time` name**, which is the only
  case where write order is not source order — under `auto` over a replayable input that is
  now nobody. It names `name_by: source_position` where the input carries a replay position
  and `concurrency: 1` otherwise, rather than pointing at a setting that would fail on an
  input with no position.
- The `source_position` write path reports `SentBatch::Partial` like the ordinary path does.
  A record that cannot be encoded splits the run instead of failing the whole batch: the
  offsets around it are written under names covering exactly what went in, and the bad record
  goes to the DLQ. (Encode failures are unreachable in practice today; this removes an
  asymmetry rather than fixing an observed bug.)

### Fixed

- A failed prefix listing no longer fails an `object_store` batch. The error was propagated as
  non-retryable, which dropped a replay's batch on a bucket that denies `ListObjects`, or on a
  transient listing hiccup. It is now logged and the sink degrades to same-name-only
  deduplication: a transient failure is retried on the next batch, while a denied `ListObjects`
  is latched after one warning and not retried.

- A route rejected for `name_by: source_position` over an input that carries no replay position
  no longer leaves a directory behind at a `file` sink's `path`. The publisher opens before the
  consumer, and the part-file sink creates its directory as it opens, so the rejected route used
  to turn the operator's output *file* into an empty directory — after which a corrected run
  failed on the leftover. The requirement is now checked before any sink opens.

## 0.4.0

### Breaking

- **Changed defaults.** Four defaults were chosen for safety or speed rather than history.
  Set the field explicitly to keep the old behaviour:
  - `batch_size` is **512** instead of 1. Batches fill opportunistically — the consumer waits
    for one message and takes whatever else is already there — so this raises throughput
    without adding idle latency. It does widen the blast radius of a failed batch, and
    nothing caps a batch by bytes, so lower it on routes carrying large payloads.
  - MongoDB `consume` defaults to **`capture_all`** instead of `consumer`. The old default
    claimed, re-fetched and **deleted** each document, so pointing a route at a collection to
    read it destroyed it — and it only ever worked on collections written by the mq-bridge
    MongoDB publisher. `capture_all` is non-destructive, reads arbitrary collections, and is
    ~5x faster. On a replica set it snapshots, then follows the change stream; a one-shot
    `exit_on_empty` / `--drain` job finishes once that stream goes quiet.
    **`capture_all` and `capture_new` now require a replica set** (a single-node one is enough)
    and refuse to start without one. `capture_all` used to fall back to paging `_id` forward on
    a standalone `mongod`; that reader only ever matches ids above its high-water mark, so any
    document a concurrent writer commits below it was dropped — silently, with no error and no
    gap in the delivery count. On a standalone server use the new `consume: snapshot`, or
    `consume: consumer` for a work queue.
  - MongoDB `consume: subscriber` and the `MongoDbSubscriber` type are **removed**. It polled
    `seq > last_seq` and advanced the watermark to the highest seq it saw, so a batch whose seq
    block was reserved first but committed second was skipped for good — the same silent loss as
    above, and present on a replica set too. Once a replica set is required it is also redundant:
    `consume: capture_new` without a `cursor_id` is ephemeral fan-out from now on, and reads
    arbitrary collections rather than only the bridge's own wrapped documents. The deprecated
    `change_stream: true` boolean now resolves to `capture_new` instead of `subscriber`.
  - ZeroMQ `format` defaults to **`raw_framed`** instead of `json`: binary-safe payloads with
    a JSON metadata frame in front, so headers still travel. This is a wire-format change —
    a 0.4 peer and a 0.3 peer no longer interoperate on the same socket unless one sets
    `format: json`.
  - ZeroMQ `backend` defaults to **`try_omq`**, which uses the faster omq backend when the
    `zeromq-omq` feature is compiled in and falls back to `zmq` otherwise. Naming `omq` or
    `zmq` explicitly still makes that backend a hard requirement.
- `extensions::register_endpoint_factory` and `register_middleware_factory` return
  `anyhow::Result<()>` instead of `()`, and registering a name that is already taken is now an
  error instead of silently replacing the previous factory. A duplicate name meant one of the
  two registrations was quietly ignored, which is impossible to diagnose from a route that then
  behaves like the wrong endpoint. Existing callers add `?` or `.unwrap()`; anything that relied
  on re-registering the same name must pick distinct names.

### Added

- MongoDB `consume: snapshot` — a one-shot, non-destructive read that pages the collection by
  `_id` and ends the route on drain. It needs no replica set, reads arbitrary collections, and is
  the supported way to read a standalone `mongod` without deleting anything. Its contract is
  deliberately narrow: it delivers what exists when the run starts, and it is **not** a tail.
  `cursor_id` is rejected at startup — resuming above a stored `_id` would skip whatever a
  concurrent writer commits below that mark, and `_id` is assigned client-side before the insert,
  so it does not follow commit order. Incremental reads need commit order, i.e. the oplog, i.e. a
  replica set.
- gRPC consumers can call arbitrary unary and server-streaming services without generated
  Rust clients. Point an endpoint at a compiled protobuf `FileDescriptorSet`, name the
  service and method, and provide the request as JSON; responses are decoded dynamically
  with `prost-reflect` and emitted using protobuf's canonical JSON representation. The
  existing generated `mqbridge.Bridge` protocol remains the default.
- The omq ZeroMQ backend covers **REQ/REP** as well as PUSH/PULL and PUB/SUB, so the whole
  `zeromq` endpoint surface — including request-reply — works on either backend. REQ exchanges
  are serialised and bounded by `request_timeout_ms`, and a timed-out socket is rebuilt rather
  than reused, because ZMTP requires strict send/recv alternation.
- Custom endpoints and middleware can be written in Python and Node.js, not just Rust —
  `register_endpoint` / `register_middleware` in both bindings, with the same batch, ack and
  request-reply semantics as a Rust `CustomEndpointFactory`. See [EXTENDING.md](docs/EXTENDING.md).
- Native endpoint plugins. An endpoint can live in its own crate and package and be loaded
  into any mq-bridge process at runtime — `mq_bridge::plugin::load_endpoint_plugin(path)` in
  Rust, `mq_bridge.load_endpoint_plugin(path)` in Python, `loadEndpointPlugin(path)` in
  Node.js — so a broker's dependencies stay out of curated mq-bridge builds while every
  language runs the same implementation and delivery semantics. The `plugin` feature (part of
  `full` and `portable`) provides the loader; `plugin-sdk` provides the authoring side:
  `export_endpoint_plugin!`, which exports an ordinary `CustomEndpointFactory` through the
  stable C ABI with no handwritten `unsafe`, and a conformance suite to run against the
  endpoint both linked directly and loaded as a plugin. A plugin can provide a
  middleware too (`export_middleware_plugin!`, or `middleware:` alongside an
  endpoint): it returns one entry per message — `None` drops it — while the
  wrapper around the endpoint stays on the host side, so nothing calls back
  across the boundary. See [PLUGINS.md](docs/PLUGINS.md).

### Fixed

- The built-in `mqbridge.Bridge` gRPC transport now commits messages with real ACK/NACK
  RPCs instead of a no-op. Embedded publishers wait for downstream processing to commit
  before receiving an ACK, and unacknowledged subscription messages are retained in memory
  and redelivered to the same `consumer_id`. This provides at-least-once delivery while the
  server process is running; durable restart recovery and exactly-once processing still
  require persistent state or downstream deduplication by message ID. Arbitrary dynamic
  services retain the delivery semantics of their own API because protobuf descriptors do
  not define a generic acknowledgement operation.
  Retention is capped (1024 messages per subscriber, 64 subscribers per route, oldest
  evicted first) so a consumer that never acknowledges cannot grow the server without bound.
  `consumer_id` defaults to a fresh id per consumer rather than to the topic, so competing
  consumers on one topic no longer share a retention set; set it explicitly to be
  redelivered unacknowledged messages after a reconnect.
- A **non-retryable handler failure no longer discards the rest of its batch**. The default
  `Handler::handle_many` aborted the remaining messages after any failure; for retryable and
  connection errors that is right (the batch is nacked and redelivered together), but a
  non-retryable message is dropped and never redelivered, so every healthy message behind it
  was silently lost. How many depended purely on `batch_size` — at the old default of 1 the
  collateral was zero, which is why this went unnoticed. The behaviour now matches what
  `CommandPublisher::send_batch` already documented and tested.
- ZeroMQ REQ/REP with `format: raw` or `raw_framed` decoded the reply using that format, but the
  REP side always answers with a JSON array of canonical messages, so the caller got one message
  whose payload was the JSON text instead of the decoded replies. Both backends now decode replies
  as JSON. This was invisible while `json` was the default format.

## 0.3.10


### Changed

- DLQ and output middleware now run around the publish step, not inside the handler, so a
  handler failure is no longer retried by the output chain. The tradeoff: a handler failure
  now skips the output middleware entirely, so `dlq` on the output endpoint cannot capture
  it — only publish failures reach the DLQ. Put a `dlq` on the input endpoint to catch
  handler failures.
- Errors carry their full cause chain instead of only the outermost context.
- `deduplication` on an **output** endpoint is now a startup error instead of a warning and a
  silent no-op. Move it to the route's input endpoint.
- `limiter` paces a single `send_batch` by its own message count, so one large batch is no
  longer a free burst. Sustained throughput is unchanged.
- A `message_id` that is neither a UUID nor a `u128` is hashed to a stable id rather than
  making the whole JSON envelope unparseable.

### Performance

- **Kafka consumer**: a long-lived prefetch task reads librdkafka continuously into a bounded
  channel, instead of rebuilding the stream inside every `receive_batch`. librdkafka only keeps
  requesting records while its queue is drained, so every pause the pipeline took — a transform,
  a slow sink — was also a pause in fetching, and the fetch rate collapsed to well under what the
  broker could serve. Batch offsets are also recorded once per partition rather than once per
  message, which was O(n²) in the batch and allocated two `CString`s per record.
  Kafka → transform → file: **192,854 → 824,983 rows/s** on the 1M-row, four-partition
  benchmark, from 0.35x to 1.51x Arroyo on identical output. A 16,384-message passthrough
  batch went from 11.5s to 2.2s, and from 10.4s to 0.9s of CPU.
- **Postgres / sqlx**: `test_before_acquire(false)` on the pool, zero-copy row encoding via
  a prebuilt `JsonRowSchema`, and prebuilt first/next page queries on the cursor path.
- **Command handler**: `send_batch` no longer loops per-message `send()` (≈8x on batched routes).
- **File**: single-pass byte-array decode, faster CSV writes, compression sniffing.
- **Deduplication**: the two-phase reserve/commit no longer writes to the store twice per
  message. Reservations are held in memory — sled takes an exclusive file lock on its directory,
  so a claim only ever has to be visible to this process — leaving one write per message, on
  commit.

### Fixed

- `insert_query` batch inserts dropped anything but a bare token from the `VALUES` tuple, so
  `decode(${payload:x}, 'base64')` and casts were silently discarded — a binary (`bytea`)
  column could not be written. The batch path now keeps the user's SQL and falls back to
  iterative inserts when the tuple contains an expression.
- A payload string containing an embedded NUL byte was dropped by the driver and stored as
  SQL `NULL` while the route reported success. It is now rejected as non-retryable.
- Database errors were rendered twice, because `sqlx` errors already display their own
  source. Cause chains now skip a link an earlier one already contains.
- `cookie_jar`'s `inject_metadata` resolved only a bare stored name, so the namespaced
  `cookie.<name>` / `value.<name>` spelling that `export_metadata_prefix` reports back
  injected nothing. Both spellings now work.
- MQTT publishes are confirmed by PUBACK/PUBCOMP and guarded against session resets, which
  took chaos-test message loss to zero.
- Postgres CDC advances its replication slot durably on shutdown instead of leaving the
  feedback unflushed.
- `deduplication` could silently drop a message after a crash. The reservation was written to
  the store *before* the message was processed, so a redelivery arriving within the 5s pending
  TTL was classified as a duplicate and acked — without ever having been written to the sink.
  Reservations are now in-memory and die with the process, so the redelivery is reprocessed.
- A Kafka record with no value — a tombstone, which is ordinary traffic on a compacted topic —
  failed the whole batch. Its offset was never committed, so the route reconnected onto the same
  record forever: one tombstone wedged the consumer permanently, and a compacted topic could not
  be consumed at all. Tombstones now arrive as an empty payload flagged `mqb.kafka.tombstone`.
- `--drain` on a Kafka source could report success having copied only part of a topic, or none
  of it. An idle wait was taken to mean "source exhausted", but an idle channel means nothing on
  its own: before the first fetch lands, or in an ordinary gap between fetches, it looks exactly
  the same as the end of the data. The shorter the idle timeout the more went missing — at 1ms a
  1,000,000-row topic landed 0 rows, and the copy still exited successfully. A drain now
  completes only once every assigned partition has reached the offset it held when the drain
  began, and lands all 1,000,000 rows at every idle timeout including 0.
- Drain no longer hangs on an empty source, and reconnect attempts are bounded.
- CSV reader handles quoted newlines; file endpoints fail fast on an unopenable path.
