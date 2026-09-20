# Middleware & Structural Endpoint Reference

Complete listing of every **middleware** and every **structural endpoint** mq-bridge ships.

Structural endpoints are the ones that do not talk to a broker or store: they compose other
endpoints, shape routing, or terminate a request. Data endpoints (`kafka`, `nats`, `mqtt`,
`sqlx`, …) are covered in [README.md](../README.md#backend-features--configuration) and
[CONFIGURATION.md](CONFIGURATION.md).

- [Middleware](#middleware)
- [Structural endpoints](#structural-endpoints)

---

## Middleware

Middleware attaches to an endpoint via a `middlewares:` list, on the **input**, the
**output**, or both:

```yaml route
my_route:
  input:
    middlewares:
      - deduplication: { sled_path: "/var/lib/mqb/dedup", ttl_seconds: 3600 }
    kafka: { topic: "orders", url: "localhost:9092" }
  output:
    middlewares:
      - retry: { max_attempts: 5 }
      - dlq: { endpoint: { file: { path: "failed.jsonl" } } }
    nats: { subject: "orders.processed", url: "nats://localhost:4222" }
```

### Ordering — read this before combining middleware

**Output (publisher) middlewares wrap in list order, so the *last* entry is the outermost
layer and sees the failures of the ones before it.** Put `dlq` last.

**Input (consumer) middlewares are applied in reverse, so the *first* entry is outermost**
and runs first on an incoming message.

**Consequence: a route that reads back what another route wrote needs the *reversed* list.**
Writing with `[compression, encryption]` produces `compress(encrypt(payload))`; a reader
given that same list would try to decrypt first and fail. The reading route must say
`[encryption, compression]`. The lists mirror — they are not copied.

**A route handler sits outside every output middleware**, so it runs **once** per message
and the middlewares act on what it returned. In particular `retry` re-attempts only the
publish, never the handler — a handler with a side effect fires once however many times the
sink is retried. The trade: a `dlq` cannot capture a handler failure, only a send failure;
a handler error propagates to the route and is reported there.

> This is asserted by `route::tests::test_retryable_handler_error_is_not_retried_by_output_middleware`
> (the handler runs once and `retry` does not re-run it),
> `route::tests::test_dlq_and_retry_batch_integration`,
> `middleware::transform::tests::test_rejected_message_reaches_the_dlq_through_the_config_wiring`,
> and `reference_docs_test::publisher_middleware_wraps_last_entry_outermost`, and is
> documented on `apply_middlewares_to_publisher` in `src/middleware/mod.rs`.

```yaml
# Correct: transform rejects -> retry gives up -> dlq captures.
middlewares:
  - transform: { schema_file: "user.json" }
  - retry: { max_attempts: 3 }
  - dlq: { endpoint: { file: { path: "rejected.jsonl" } } }
```

### What exists

| Name | Input | Output | Feature | Purpose |
|---|:---:|:---:|---|---|
| [`retry`](#retry) | – | ✅ | – | Exponential-backoff retry of failed sends |
| [`dlq`](#dlq) | – | ✅ | – | Route permanently-failed messages to another endpoint |
| [`transform`](#transform) | ✅ | ✅ | – | Declarative JSON mapping, coercion, validation |
| [`id`](#id) | ✅ | – | – | Derive a replay-stable business identity into `mqb.id` |
| [`filter`](#filter) | ✅ | ✅ | `filter` | Keep only the messages matching an expression |
| [`deduplication`](#deduplication) | ✅ | – | `dedup` | Drop repeated keys within a TTL |
| [`weak_join`](#weak_join) | ✅ | – | – | Correlate and join related messages |
| [`buffer`](#buffer) | ✅ | ✅ | – | Coalesce single sends into batches |
| [`limiter`](#limiter) | ✅ | ✅ | – | Cap throughput to a message rate |
| [`delay`](#delay) | ✅ | ✅ | – | Fixed delay per receive/send |
| [`cookie_jar`](#cookie_jar) | ✅ | ✅ | – | Persist HTTP cookies / session values across messages |
| [`encryption`](#encryption) | ✅ | ✅ | `encryption` | AEAD-encrypt payloads on send, decrypt on receive |
| [`compression`](#compression) | ✅ | ✅ | `compression` | Compress payloads on send, decompress on receive |
| [`pack`](#pack) | – | ✅ | – | Combine a batch into one physical transport message |
| [`unpack`](#unpack) | ✅ | – | – | Split a packed physical message back into messages |
| [`metrics`](#metrics) | ✅ | ✅ | `metrics` | Emit throughput/latency/error metrics |
| [`random_panic`](#random_panic) | ✅ | ✅ | – | Fault injection for testing |
| [`custom`](#custom-middleware) | ✅ | ✅ | – | Your own middleware via a registered factory |

> **Two kinds of compression.** The [`compression`](#compression) *middleware* compresses
> each message **payload** on any transport and decompresses it on the far side. Separately,
> the batch `compression` *field* on the `file` and `object_store` endpoints (`none` / `gzip`
> / `lz4` / `zstd`, same `compression` feature) compresses whole write batches so the file
> stays decodable with `zcat` / `lz4 -d`. Use the field for CLI-readable data at rest, the
> middleware for over-the-wire payloads. Stacking compression with the
> [`encryption`](#encryption) middleware only works in one order — the codec must run
> **before** the cipher, since ciphertext does not compress. On an output that means
> listing `encryption` *before* `compression`, so compression ends up outermost of the
> two: see [compress-then-encrypt over the wire](#compress-then-encrypt-over-the-wire).
> For compressed-and-encrypted data at rest, prefer the endpoints' own
> `compression`/`encryption` fields, which do this per write batch.

**Putting a middleware on the wrong side behaves in two different ways**, so check the table
above rather than assuming:

- `dlq` / `retry` on an input log a warning and are skipped. The route still starts.
- `deduplication`, `weak_join` and `id` on an output are **hard startup errors**. Deduplication
  cannot work on the publish side, and silently starting an un-deduplicated route is worse
  than refusing to start. `pack` on an input and `unpack` on an output are hard errors too —
  the pair is directional.

A middleware whose feature is not compiled in (`deduplication` without `dedup`, `metrics`
without `metrics`) is likewise a startup error, not a silent no-op.

---

### `retry`

Retries failed sends with exponential backoff. Output only.

| Field | Type | Default |
|---|---|---|
| `max_attempts` | integer | `3` |
| `initial_interval_ms` | integer | `100` |
| `max_interval_ms` | integer | `5000` |
| `multiplier` | float | `2.0` |

```yaml middleware
- retry: { max_attempts: 5, initial_interval_ms: 200, max_interval_ms: 10000, multiplier: 2.0 }
```

Only `Retryable` and connection errors are retried; `NonRetryable` failures pass straight
through. Once attempts are exhausted the error is marked so a following `dlq` treats it as
permanent. Pair the two.

### `dlq`

Sends permanently-failed messages to a separate endpoint instead of failing the batch. Output only.

| Field | Type | Required |
|---|---|---|
| `endpoint` | Endpoint | yes |

```yaml middleware
- dlq:
    endpoint:
      file: { path: "dead-letters.jsonl" }
```

Captures `NonRetryable` failures and `Retryable` ones whose retries are exhausted. Connection
errors are **not** dead-lettered — they propagate so the route can reconnect. Nor are handler
failures: the handler runs outside the middlewares (see [Ordering](#ordering--read-this-before-combining-middleware)),
so a `dlq` only ever sees what failed on the way to the sink. The DLQ endpoint
is a full endpoint, so it can itself have middleware. If the DLQ send fails with a connection
error that error propagates rather than silently dropping the message.

**Without a `dlq` middleware**, a message that fails permanently — a data/type
error the sink rejects — is logged at `error` level and
**dropped**, and the route keeps processing the rest of the batch. `dlq` is the only retention
mechanism: `retry` alone does not retain a permanently-failed message nor prevent it from being
dropped — it only re-attempts `Retryable` errors (a connection error is passed straight through
for the route to reconnect on, not retried), then hands a still-failing message on to be dropped
(or to a following `dlq`). This is why a sink that fails with a *connection* error never reaches
its `dlq`, whether it is a route's sole output or one leg of a `fanout`. This tolerate-and-continue
policy keeps one bad message from halting the whole stream, but it means a *systematic* failure
(e.g. every row hitting a column-type mismatch) drains the input while committing nothing and
still ends `completed`. Add a `dlq` to capture the failures for inspection/replay, or watch the
route's logs — a burst of `Dropping message … due to non-retryable error` is the signal. Note
that transient errors are handled separately: several endpoints retry connection/timeout errors
internally, and the `retry` middleware adds backoff on top, so only genuinely permanent errors
reach this drop path.

### `transform`

JSON reshaping with field mapping, Zen Expression, and schema processing, in that order.

| Field | Type | Default |
|---|---|---|
| `mapping` | map of output field → rule | `{}` |
| `expression` | Zen Expression returning the output document | – |
| `schema` | inline JSON Schema subset | – |
| `schema_file` | path to a schema file | – |
| `coerce` | bool | `true` |
| `apply_defaults` | bool | `true` |
| `coerce_empty_as_null` | bool | `false` |
| `on_error` | `reject` \| `pass_through` | `reject` |

`schema` and `schema_file` are mutually exclusive. A mapping rule is a bare path string or
`{ path, default, required }`.

`schema` must be a JSON *object*, not a string containing one. A flat `key=value` middleware
syntax (such as the `|transform?schema=…` form in a connection URI) can only pass strings, so
it cannot express `schema` or any mapping rule beyond a bare path — use `schema_file`, or move
the route into a config file.

```yaml middleware
- transform:
    mapping:
      firstName: "$.first_name"
      id: "$.user_id"
      "address.city": { path: "$.city", default: "unknown" }
    schema_file: "schemas/user.json"
```

For calculated output, use `expression` (available with the `zen` Cargo feature):

```yaml middleware
- transform:
    mapping:
      first: "$.first_name"
      last: "$.last_name"
    expression: >-
      { fullName: first + ' ' + last, source: meta.source }
```

Paths accept `$.field`, `$.a.b`, and `$.items[0]`; the `$.` prefix is optional. Dots in the
*output* key nest the result. An absent optional source field is omitted rather than emitted
as null.

Schema keywords honoured: `type`, `properties`, `required`, `default`, `items`, `nullable`
(also `"type": ["string","null"]`), `enum`, `contentMediaType`, `contentSchema`. Everything
else is ignored, so an existing fuller schema can be used as-is. Coercions are limited to the
lossless ones: `string → integer`, `string → number`, `string → boolean` (`true`/`false`/`1`/`0`),
`number → string`.

#### Empty strings

CSV and many SQL exports spell "no value" as an empty string. `coerce_empty_as_null: true`
reads every `""` the schema visits as `null`, which is then handled like any other null —
a `nullable` field keeps it, a `default` replaces it:

```yaml middleware
- transform:
    coerce_empty_as_null: true
    schema:
      type: object
      properties:
        note: { type: string, nullable: true }
        tier: { type: string, default: standard }
```

`note: ""` arrives as `null` and `tier: ""` as `"standard"`. A field that is neither
nullable nor defaulted is rejected, naming the coercion. Only fields the schema declares are
affected; `" "` is not empty.

#### Embedded JSON

A field carrying a JSON document as a string is decoded by `contentMediaType`, following
JSON Schema 2020-12:

```yaml middleware
- transform:
    schema:
      type: object
      properties:
        payload:
          type: string
          contentMediaType: application/json
          contentSchema:
            type: object
            properties:
              qty: { type: integer }
```

The string is replaced by the parsed document, and `contentSchema` — if given — is applied to
it with the same coercion, defaults and validation as anywhere else, so the inner `qty: "7"`
arrives as `7`. Without `contentSchema` the value is parsed but not validated. A root-level
schema of this shape decodes a double-encoded message body.

This is **not** a coercion, and `coerce: true` never performs it: widening `"42"` to `42` is
lossless, whereas evaluating a string as a document is a parse that can succeed on input never
meant as JSON. It is opt-in per field, as the JSON Schema spec requires. Note that the spec
treats `contentSchema` as annotation-only; applying it is the opt-in behaviour it carves out.

Media types ending in `+json` (and `text/json`) are decoded too; parameters like
`; charset=utf-8` are ignored. A media type we cannot decode, or one paired with a
`contentEncoding`, leaves the string untouched rather than failing. A string that does not
parse fails with kind `content`:
`transform failed at $.payload [content]: contentMediaType is JSON but the string does not parse: ...`.

Failures are always non-retryable and name the field, e.g.
`transform failed at $.items[1].qty [coercion]: cannot coerce string "oops" to integer`.
On an **output** endpoint the message is failed so a following `dlq` captures it; on an
**input** endpoint it is dropped from the batch and acknowledged, keeping invalid data out of
the route. `on_error: pass_through` instead forwards the original payload with the reason in
the `mqb.transform_error` metadata key, which a [`switch`](#switch) can route on.

Schemas and paths compile once at startup; `schema_file` is read a single time. A `transform`
with neither stage configured leaves the payload untouched without parsing it.

### `id`

Renders a template into the `mqb.id` metadata key, giving a message a **business identity**
that survives a re-read. Input only.

The value is a bare interpolation template string (see `${namespace:selector}`).

```yaml middleware
- id: "${payload:order_id}"
```

Most sources mint a fresh `message_id` on every read, so it identifies the *delivery*, not the
record — see [DELIVERY.md](DELIVERY.md#sources-what-identity-you-get) for which ones do carry a
stable id. `mqb.id` fills that gap: derived from the message itself, it is the same on every
re-read. Unlike `message_id` (a `u128`) it keeps the key as a string, so a sink can use it
verbatim, and unlike `mqb.src.*` it is **not** stripped on publish — an identity describes the
record, not the hop, so it propagates downstream.

**Order matters, and not the way it reads.** Consumer middlewares wrap in reverse, so the entry
*closest to the end of the list* touches an incoming message *first*. Anything that consumes
`mqb.id` must therefore be listed **before** the `id` that produces it:

```yaml middleware
- deduplication: { store: "sled:///var/lib/mq-bridge/dedup", ttl_seconds: 3600, key: "${metadata:mqb.id}" }
- id: "${payload:order_id}"
```

Reversing those two leaves `mqb.id` unset when `deduplication` reads it, which falls back to
`message_id` with only a warning. Pinned by
`middleware::id::tests::the_last_listed_consumer_middleware_runs_first`.

**A partial identity is no identity.** The key is set only when *every* selector in the template
resolves; if any one is missing the message passes through with `mqb.id` unset (warned once per
route, then at `debug`). This matters for multi-part templates: `"${payload:tenant}-${payload:order_id}"`
would otherwise render `"acme-"` for every message missing `order_id` and hand them all the same
identity — which, used as a `deduplication` key, drops all but the first.

Malformed templates fail at startup, and so does a template with no `${...}` token at all, since a
constant would give every message one identity.

### `filter`

Keeps only the messages for which an expression is true; the rest are dropped. Input and
output. Requires the `filter` feature (pulls the `zen-expression` engine), which is part of
`middleware`/`full` but **not** of `portable`.

The value is a bare expression string:

```yaml middleware
- filter: "amount > 100"
```

**Put it on the input whenever you can.** A filter on the input drops the message before the
rest of the pipeline touches it, and acknowledges it at the source; on the output the
message has already paid for the whole route.

When filtering splits a full input batch, the consumer reads additional full source batches
until it refills the requested batch size. A naturally short source batch remains a flush
boundary, so live routes do not wait indefinitely merely to fill a batch. This lets sinks such
as MongoDB continue using bulk writes after filtering without adding input buffer middleware.

**What an expression can read:**

- **Payload fields by bare name**, including nested paths — `amount`, `order.status`. The
  payload must be a JSON object; anything else produces a per-message error and fails the batch.
  Indexed paths such as `items[0].qty` are unsupported: an expression that uses one is
  rejected at startup rather than silently dropping every message.
- **Metadata under the reserved `meta.` prefix** — `meta.http_status_code`,
  `meta.kind`. Metadata is **always text**, and so are all CSV fields and a SQL source's
  `numeric` and timestamp columns. Comparing one against a **numeric literal** reads it as a
  number, so `meta.http_status_code >= 400` needs no cast. The literal is what decides:
  `zip == "01234"` stays a string comparison, and `number()` is still required where no
  literal names the intent (`meta.a > meta.b`, `amount > 100 * 2`) or where the text is not a
  number at all.

If an expression names no payload field at all, the payload is never parsed — a
metadata-only filter costs no JSON decode.

```yaml middleware
- filter: "order.status == \"open\" and meta.retry_count < 3"
```

`&&` and `||` are rewritten to `and` / `or` for you, so both spellings work.

A field that is absent is supplied to the expression as `null`, so an `or` branch or negation
can still match. A `null` or non-scalar field (an array or object where the expression expects
a scalar) logs a warning. A payload that is not a JSON object, or an
expression that does not evaluate to a boolean, is an **error** and fails the batch; those
are configuration mistakes, and dropping every message would hide them.

To send the non-matching messages somewhere instead of discarding them, use
[`switch`](#switch)'s `when` mode rather than a filter.

**With an `object_store` sink on `name_by: auto`, the route switches to `write_time` names.**
A source-range name covers one *contiguous* run of source positions, so a batch with holes
punched in it would be written as one object per surviving run — a filter keeping 80% of rows
turns one upload into roughly a hundred. The route logs one line at startup saying it made the
switch. The same applies to every other middleware that removes messages from a batch
(`deduplication`, `weak_join`, `transform` with `on_error: reject`) and to a `switch` in `when`
mode with no `default`. Set `name_by: source_position` explicitly to keep replay-safe names and
accept the fragmentation.

---

### `deduplication`

Drops messages whose key was already seen within the TTL. Input only. Requires the `dedup`
feature (pulls `sled`).

| Field | Type | Required |
|---|---|---|
| `store` | string | one of `store`/`sled_path` |
| `sled_path` | string | one of `store`/`sled_path` |
| `ttl_seconds` | integer | yes |
| `key` | string | no (defaults to `message_id`) |

`key` is an interpolation template (see `${namespace:selector}`), typically
`"${payload:order_id}"`. Without it the key is the `message_id`, which most sources
regenerate on every read — so re-reading the same source deduplicates nothing and only
in-flight redeliveries are suppressed. Set `key` to a business key whenever you need
dedup to survive a re-read.

`store` selects the backend by URL scheme:

- `sled:///path` (or a bare path) — a local sled database; per-process, not cluster-wide.
- `mongodb://host/db[/collection]` — a shared collection, so multiple instances of a route
  deduplicate against one another. Requires the `mongodb` feature. Expiry is judged on read,
  so a `ttl_seconds` boundary is honoured exactly; the TTL index only reclaims space
  afterwards (MongoDB's sweep can lag by up to a minute). The collection defaults to
  `mqb_dedup_<route>`. Point it at the same
  deployment your sink already uses to avoid running extra infrastructure.
- `postgres|mysql|mariadb|sqlite://…[/table]` — a shared SQL table (`dedup_key` PK,
  `expire_at`), so multiple instances deduplicate against one another. Requires the `sqlx`
  feature. SQL has no native TTL, so expired rows are swept periodically; the table defaults
  to `mqb_dedup_<route>`.

`sled_path` is the legacy spelling of a local sled store and is equivalent to `store: "sled://<path>"`.

```yaml middleware
- deduplication: { store: "sled:///var/lib/mq-bridge/dedup", ttl_seconds: 3600 }
```

```yaml middleware
- deduplication: { store: "mongodb://localhost:27017/etl", ttl_seconds: 3600 }
```

```yaml middleware
- deduplication: { store: "postgres://user:pass@localhost/etl", ttl_seconds: 3600 }
```

```yaml middleware
- deduplication: { store: "sled:///var/lib/mq-bridge/dedup", ttl_seconds: 3600, key: "${payload:order_id}" }
```

When MongoDB is your sink and messages carry a business key, prefer the sink's own unique
index (`id_field`, which also accepts templates, on the mongodb output) over this middleware — the target collection then
*is* the deduplication authority, with no second write. See the idempotency notes in README.

### `weak_join`

Correlates messages by a metadata key and emits them as one joined message. Input only.

| Field | Type | Default |
|---|---|---|
| `group_by` | string (metadata key) | required |
| `expected_count` | integer | required |
| `timeout_ms` | integer | required |
| `branch_by` | string (metadata key) | – |
| `required` | list of branch names | `[]` |
| `on_timeout` | `fire` \| `discard` | `fire` |

```yaml middleware
# Count mode: wait for any 3 messages sharing a correlation_id, emit a JSON array.
- weak_join: { group_by: "correlation_id", expected_count: 3, timeout_ms: 5000 }

# Branch mode: wait for named branches, emit a branch-keyed JSON object.
- weak_join:
    group_by: "correlation_id"
    expected_count: 2
    timeout_ms: 5000
    branch_by: "source"
    required: ["inventory", "pricing"]
    on_timeout: discard
```

`group_by` reads message **metadata** only — never the payload. A message that lacks the key
falls into a shared `"default"` group, so a mistyped key or a source that never sets it joins
unrelated messages instead of failing. If the value lives in the payload, lift it into metadata
first (a `transform` mapping, or the source's own metadata options).

Setting `branch_by` switches to branch mode, where `required` overrides `expected_count`.
On timeout an incomplete group is either emitted partially (`fire`) or dropped (`discard`).
Messages are acknowledged on receipt, so a crash before the group completes loses the
buffered members.

### `buffer`

Accumulates single sends and forwards them as one batch. Input and output.

| Field | Type | Required |
|---|---|---|
| `max_messages` | integer | yes |
| `max_delay_ms` | integer | yes |

```yaml middleware
- buffer: { max_messages: 500, max_delay_ms: 20 }
```

Flushes when either bound is hit. Useful in front of an endpoint whose per-call overhead
dominates. Adds up to `max_delay_ms` of latency.

With route `concurrency` greater than 1, buffering preserves order inside each batch but
does not guarantee source order across concurrent destination writes. Use `concurrency: 1`
when destination order matters; route validation emits a warning for this combination.

### `limiter`

Paces throughput to a target rate. Input and output.

| Field | Type | Required |
|---|---|---|
| `messages_per_second` | float (> 0) | yes |

```yaml middleware
- limiter: { messages_per_second: 250 }
```

Best-effort pacing that accounts for batch size, not just call count.

### `delay`

Sleeps a fixed duration before each receive or send. Input and output.

| Field | Type | Required |
|---|---|---|
| `delay_ms` | integer | yes |

```yaml middleware
- delay: { delay_ms: 100 }
```

Mainly for testing and for crude pacing of a downstream system; prefer
[`limiter`](#limiter) for real rate control.

### `cookie_jar`

Persists HTTP cookies and arbitrary session values across messages. Input and output.

| Field | Type | Default |
|---|---|---|
| `shared_scope` | string | – (per-instance store) |
| `cookie_metadata_key` | string | `cookie` |
| `set_cookie_metadata_key` | string | `set-cookie` |
| `capture_metadata_keys` | list of strings | `[]` |
| `export_metadata_prefix` | string | – |
| `inject_metadata` | map string→string | `{}` |
| `max_cookies` | integer — cap per session store | `256` |

```yaml middleware
- cookie_jar:
    shared_scope: "login-session"
    capture_metadata_keys: ["x-csrf-token"]
    export_metadata_prefix: "session."
```

Reads `set-cookie` from responses and injects `cookie` into later requests. With
`shared_scope`, instances using the same name share one store across endpoints and routes in
the process — that is how a login route and a data route reuse one session.

Cookie names are chosen by the server, so the jar is bounded: `Max-Age=0` (or negative)
deletes a cookie, and once `max_cookies` is exceeded the least recently set entries are
dropped. `Expires` is **not** parsed — use `Max-Age`, which every modern server also sends.

### `encryption`

Encrypts each message **payload** into a self-describing AEAD envelope on the output side
and decrypts it on the input side. Metadata and routing keys stay in the clear. Input and
output. Requires the `encryption` feature.

| Field | Type | Default |
|---|---|---|
| `cipher` | `xchacha20poly1305` \| `aes256gcm` | `xchacha20poly1305` |
| `key_id` | string | `default` |
| `key` | string — base64-encoded 32-byte key; `${env:VAR}` reads it from the environment | required |
| `decrypt_keys` | map key_id → key | `{}` |
| `authenticate_metadata` | list of metadata keys bound into the AEAD tag (middleware only) | `[]` |

```yaml middleware
- encryption: { key: "${env:MQB_ENC_KEY}" }
```

The envelope records the cipher and `key_id`, so key rotation works by sealing with a new
`key_id`/`key` while listing the old key under `decrypt_keys` on the consuming side. Each
payload is authenticated independently: any bit-level tampering, a torn frame, or a
missing/wrong key is a hard consumer error, not a silent drop.

By default the AEAD binds only the payload (empty associated data): metadata and routing
keys are *not* authenticated against the ciphertext, since they are not guaranteed to
survive transport round-trips (many endpoints regenerate the `message_id` or drop `kind`).
A sealed payload can therefore be replayed under different metadata.

`authenticate_metadata` closes that gap for keys you know survive your transport. Listed
keys stay in the clear but are bound into the tag, so altering, removing or adding one in
transit fails decryption exactly like a tampered payload — no extra field or second key is
needed, since the AEAD tag already is the checksum:

```yaml middleware
- encryption:
    key: "${env:MQB_ENC_KEY}"
    authenticate_metadata: [tenant, mqb.id]
```

Both sides must configure the *same* list; a mismatch surfaces as a decrypt failure. Keys
are bound in the order listed, and an absent key is distinguished from an empty one. These
envelopes are tagged version 2, so adding the field is a one-way switch for data already at
rest or in flight — drain the route first. The field applies to this middleware only; the
`file` / `object_store` at-rest `encryption` fields reject it, since there is no per-message
metadata at that layer. Note that this authenticates
each payload, not the file as a whole — like any append-structured file, an at-rest file
that loses whole trailing frames (truncation at a frame boundary) reads back as a shorter
stream with no error, so rely on the consumer's checkpoint/cursor for completeness rather
than on the encryption layer.

Do **not** combine this middleware with a sink's batch `compression` on the same route:
ciphertext does not compress. For compressed *and* encrypted data at rest, use the `file` /
`object_store` endpoints' own fields instead, which apply compress-then-encrypt per batch:

```yaml endpoint
output:
  file:
    path: "data.enc"
    format: raw
    compression: lz4          # none | gzip | lz4 | zstd  (`compression` feature)
    encryption: { key: "${env:MQB_ENC_KEY}" }
```

Both endpoints accept the same `compression` and `encryption` fields (`object_store`
derives its default object extension from them, e.g. `.jsonl.gz` / `.jsonl.lz4`, and adds a
trailing `.enc` when encryption is on since the object is ciphertext, not a directly
decompressible `.gz`). An
encrypted **file** is written as length-prefixed sealed frames (one per batch) and is only
readable through a matching consumer; a compressed-only file stays a standard `.gz`/`.lz4`
stream. File compression/encryption supports only the default `consume` mode. `csv` works
too: the header row is written into the first member, so the decoded stream is a normal CSV
file.

A file **source** must declare the same `compression`/`encryption` the data was written with.
A mismatch (wrong key, wrong codec, or a missing field) is a permanent decode failure: the
route ends `failed` with the error in its status, rather than completing as if the file were
empty. Reading a compressed file with no `compression` set is likewise rejected up front by
sniffing the leading magic bytes, so raw compressed bytes are never emitted as messages.

> **f64 precision.** Numbers move through payloads as JSON. serde_json's default parser shifts
> ~1 ULP on ~19% of 17-significant-digit doubles, so a `postgres → file → postgres` hop of a
> `double precision` column can change the last bit. Build with the `float-roundtrip` feature
> for bit-exact float parsing across every endpoint (it trades a little parse speed for it).

### `compression`

Compresses each message **payload** on the output side and decompresses it on the input
side. Metadata and routing keys are untouched. Input and output. Requires the `compression`
feature.

| Field | Type | Default |
|---|---|---|
| `algorithm` | `none` \| `gzip` \| `lz4` \| `zstd` | `zstd` |
| `max_decompressed_bytes` | integer — reject a payload that decompresses larger than this (bomb guard); consumer side only | unset (no limit) |

```yaml middleware
- compression: { algorithm: zstd }
```

Each payload is compressed independently into a single self-contained member, so this works
over any transport, not just files. `algorithm: none` is a passthrough. A truncated or
corrupt frame is a **permanent** consumer error (the poison message is not re-read
indefinitely), as is a payload that exceeds `max_decompressed_bytes`. Put the same
`algorithm` on both the input and output side of a route.

Unlike the `file` / `object_store` batch `compression` field — which keeps whole write
batches decodable with `zcat` / `lz4 -d` — this middleware frames per message and is only
readable through a matching consumer. Do not combine it with the [`encryption`](#encryption)
middleware (ciphertext does not compress); for compressed-and-encrypted data at rest, use the
endpoints' own `compression`/`encryption` fields instead.

### `pack`

Combines the messages of one publish batch into a **single physical transport message**, so a
thousand rows cost one transport operation instead of a thousand. **Output only** — pair it
with [`unpack`](#unpack) on the reading route's input.

| Field | Type | Default |
|---|---|---|
| `format` | `mqb` \| `benthos_binary` | `mqb` |
| `max_messages` | integer — logical messages per physical message | `1000` |
| `max_bytes` | integer — body-size threshold that closes a physical message | `4194304` (4 MiB) |
| `drop_message_id` | boolean — leave each `message_id` out, saving 16 bytes per record | `false` |

```yaml middleware
- pack: { max_messages: 1000 }
```

A batch larger than either bound is split into several physical messages, and the final
partial batch is sent like any other. `max_bytes` is a threshold, not a hard cap: a single
message larger than it is sent on its own rather than being dropped.

**Compression is separate.** `pack` has no codec of its own — stack the
[`compression`](#compression) middleware around it and the whole physical message is
compressed, which is where the ratio is best anyway. Order matters
(see [Ordering](#ordering--read-this-before-combining-middleware)); on the output,
`pack` must be outermost of the two so it frames before the codec compresses:

```yaml middleware
- compression: { algorithm: zstd }
- pack: { max_messages: 1000, max_bytes: 4194304 }
- buffer: { max_messages: 1000, max_delay_ms: 20 }
```

`buffer` goes last so it ends up outermost and hands `pack` a full batch. A route already
reads its input in `batch_size` chunks, so `buffer` is only needed when `batch_size` is
small or messages arrive one at a time.

The physical message carries no per-message metadata of its own — only `mqb.pack.count` —
so do not combine `pack` with a sink that interpolates `${metadata:…}` into a topic,
subject or path. It is likewise incompatible with request/reply: one physical message has
one acknowledgement and cannot carry N replies.

To save 16 bytes per record, set `drop_message_id: true` — the unpacked messages then get
fresh ids. To drop metadata too, use `format: benthos_binary`.

### `unpack`

Splits a physical message written by [`pack`](#pack) back into its logical messages.
**Input only.**

| Field | Type | Default |
|---|---|---|
| `format` | `mqb` \| `benthos_binary` — must match the sender | `mqb` |
| `max_messages` | integer — reject a batch declaring more messages than this | unset (no limit) |

```yaml middleware
- unpack: {}
```

The reading list is the **mirror** of the writing one, so `compression` comes *after*
`unpack` — the codec then decompresses the physical message before `unpack` sees it:

```yaml middleware
- unpack: {}
- compression: { algorithm: zstd }
```

With `format: mqb`, unpacked messages behave exactly like any other: payload, metadata,
`message_id` and order are all as they were before packing. `format: benthos_binary` carries
payloads only — an input reading it gets the payloads and their order back, but no metadata
and no `message_id`. Either way `receive_batch` still honours the route's `batch_size` —
anything over it is held and handed out on the next read.

Payloads are sliced out of the physical message rather than copied, so unpacking costs
almost nothing per record — *except* for metadata, which has to be rebuilt into an owned
map (one allocation for the map plus two per key/value pair, per message). On the
`benches/pack_bench.rs` corpus that is the difference between ~64M and ~4.5M records/s.
It is the same price any consumer pays for metadata, and still cheaper than decoding a
per-message metadata frame; but if a pipeline does not need per-record metadata,
`format: benthos_binary` skips it entirely.

A malformed, truncated or foreign payload is a **permanent** consumer error, not a
reconnectable one, so a poison message is not re-read forever. So is an envelope written by
a newer format version.

**Acknowledgement.** The physical message is what the transport acks, so the messages inside
one share its fate. `unpack` holds the source's commit until every message it produced has
been dispositioned, then collapses the group: any `Nack` nacks the whole physical message and
all N are redelivered. Nothing is lost; some may be seen twice. That is the same at-least-once
widening a `batch_size` above 1 already has — pair with [`deduplication`](#deduplication) or
an idempotent sink if duplicates matter.

**Interoperability.** `format: benthos_binary` reads and writes the layout Redpanda Connect's
`archive: binary` / `unarchive: binary` processors use (`u32` big-endian count, then a `u32`
big-endian length before each payload). It carries payloads only — metadata and `message_id`
have nowhere to go:

```yaml middleware
- unpack: { format: benthos_binary }
```

#### Compress-then-encrypt over the wire

`pack`, `compression` and `encryption` compose into the full wire stack, and the
ordering rules decide which way round they have to go. On an **output**, the last entry is
outermost and runs first, so listing them in this order gives
`encrypt(compress(pack(messages)))`:

```yaml middleware
- encryption: { key: "${env:MQB_KEY}" }
- compression: { algorithm: zstd }
- pack: { max_messages: 1000 }
- buffer: { max_messages: 1000, max_delay_ms: 20 }
```

Read the list bottom-up to follow a message: `buffer` collects, `pack` frames the batch
into one physical message, `compression` compresses that whole message, `encryption` seals
the result. Packing first is what makes the compression worth having — the codec sees a
thousand similar records instead of one payload at a time.

The **input** list is the plain mirror, with `unpack` in place of `pack`:

```yaml middleware
- unpack: {}
- compression: { algorithm: zstd }
- encryption: { key: "${env:MQB_KEY}" }
```

Getting either order wrong fails loudly rather than silently: compressing ciphertext
merely wastes time, but decrypting something that was never encrypted, or unpacking
something still compressed, is a permanent error on the first message.

> Asserted by `middleware::pack::tests::pack_compression_and_encryption_stack_in_the_documented_order`.

### `metrics`

Emits throughput, latency and error metrics for the endpoint. Input and output. Requires the
`metrics` feature. Takes no options; its presence enables collection.

```yaml middleware
- metrics: {}
```

Input and output are labelled separately, so attaching it to both sides is meaningful.

### `random_panic`

Deliberate fault injection for testing recovery paths. Input and output.

| Field | Type | Default |
|---|---|---|
| `mode` | `panic` \| `disconnect` \| `timeout` \| `json_format_error` \| `nack` | `panic` |
| `trigger_on_message` | integer (1-indexed) | – (every message) |
| `enabled` | bool | `true` |

```yaml middleware
- random_panic: { mode: disconnect, trigger_on_message: 500 }
```

`disconnect` and `timeout` produce retryable errors; `json_format_error` produces a
non-retryable one — useful for exercising a `dlq`. Keep `enabled: false` in committed configs
rather than deleting the block.

On the **input** side, `json_format_error`/`nack` never call the real consumer at all — they
substitute a synthetic message (or error) on every triggered `receive`. Leaving
`trigger_on_message` unset means *every* poll is faulted, so the real source is never read and
`exit_on_empty`/`--drain` never sees the empty batch it waits for — the route runs forever,
manufacturing synthetic messages. Always set `trigger_on_message` to a specific count when
testing a drain-mode route with input-side fault injection. And since `dlq`/`retry` on an input
are no-ops (see above), pair an input-side fault with a real assertion on the *consumer's*
recovery, not a `dlq`.

The middleware block alone is **not** enough: fault injection is gated per route by
`allow_fault_injection`, which defaults to `false`. Copying only the snippet above leaves the
middleware inert (the route logs that it is disabled). A complete, working configuration:

```yaml route
flaky_test_route:
  allow_fault_injection: true
  input:
    memory: { topic: "in" }
    middlewares:
      - random_panic: { mode: disconnect, trigger_on_message: 500 }
  output:
    memory: { topic: "out" }
```

`allow_fault_injection: true` is intended for test configurations only. Do not enable it — or
the `random_panic` middleware — in production configs.

### `custom` (middleware)

Delegates to a factory you registered programmatically.

| Field | Type | Required |
|---|---|---|
| `name` | string | yes |
| `config` | any JSON | yes |

```yaml middleware
- custom:
    name: "my_enricher"
    config: { lookup_url: "http://enrich.internal" }
```

Implement `CustomMiddlewareFactory` (`apply_consumer` and/or `apply_publisher`, each
defaulting to pass-through) and register it before starting routes. It can also be written
in Python (`register_middleware`, hooks `on_receive` / `on_send`) or JavaScript
(`registerMiddleware`, hooks `onReceive` / `onSend`). See **[EXTENDING.md](EXTENDING.md)**
for the full guide.

---

## Structural endpoints

These appear wherever an endpoint is expected — as a route `input`/`output`, or nested inside
another structural endpoint.

| Name | Input | Output | Purpose |
|---|:---:|:---:|---|
| [`ref`](#ref) | ✅ | ✅ | Reuse an endpoint defined elsewhere by name |
| [`fanout`](#fanout) | – | ✅ | Send every message to all listed endpoints; one may reply |
| [`switch`](#switch) | – | ✅ | Content-based routing on a metadata value or an expression |
| [`request`](#request) | – | ✅ | Call a request/reply endpoint, forward the response onward |
| [`response`](#response) | – | ✅ | Reply to the origin of the current request |
| [`reader`](#reader) | – | ✅ | Use an incoming message as a trigger to pull from a consumer |
| [`sequence`](#sequence) | ✅ | – | Read several inputs in order: drain each, then stream the last |
| [`static`](#static) | ✅ | ✅ | Fixed, pre-rendered message |
| [`stream_buffer`](#stream_buffer) | ✅ | ✅ | Correlation-partitioned in-memory stream |
| [`null`](#null) | – | ✅ | Discard everything |
| [`custom`](#custom-endpoint) | ✅ | ✅ | Your own endpoint via a registered factory |

They live under `src/endpoints/structural/`, and each of the variants above carries
`"format": "structural_endpoint"` in the generated JSON schema (`mq-bridge.schema.json`),
so external tooling can tell them apart from the transport endpoints.

### `ref`

Reuses an endpoint registered under a name, instead of repeating its configuration.

The name is a **registry key, not a topic name**. Register it from Rust before starting the
routes:

```rust
use mq_bridge::models::Endpoint;
use mq_bridge::route::register_endpoint;

register_endpoint("common_queue", Endpoint::new_memory("shared_memory_topic", 100));
```

```yaml route
enrich:
  input: { ref: "common_queue" }
  output: { nats: { subject: "enriched", url: "nats://localhost:4222" } }
```

A route can also publish its own output under a name with
`Route::register_output_endpoint(Some("name"))`, which is how one route's output becomes
another's input.

The value is a bare string. Resolution looks in the endpoint registry first, then in
registered publishers. Middleware on the `ref` itself is applied **outside** the referenced
endpoint's own middleware. Circular references are detected and rejected at startup, and
nesting depth is bounded.

### `fanout`

Publishes each message to every listed endpoint. Output only.

```yaml endpoint
output:
  fanout:
    - kafka: { topic: "audit", url: "localhost:9092" }
    - file: { path: "audit.jsonl" }
    - nats: { subject: "audit", url: "nats://localhost:4222" }
```

The value is a plain list of endpoints, each of which may have its own middleware and may
itself be structural. All branches receive the same message.

A fan-out can also reply. If one of the branches produces a response — a
[`response`](#response) or [`static`](#static) leg, or a [`request`](#request) whose
`forward_to` replies — that response is returned to the caller, so a request/reply input can
fan its message out and still answer. Branches that must not answer need a `forward_to` that
does not reply (`{}` is the `null` endpoint, which discards).

```yaml route
# Mirror every call to staging, but answer the caller from production only.
proxy:
  input: { http: { url: "0.0.0.0:8443", path: "test" } }
  output:
    fanout:
      - request:
          to: { http: { url: "http://127.0.0.1:1444/" } }
          forward_to: {}                 # discard staging's response
      - request:
          to: { http: { url: "http://127.0.0.1:1445/" } }
          forward_to: { response: {} }   # only this one replies to the caller
```

A caller has one reply channel, so only one branch may answer a given message: if several do,
the **first in list order** wins and the others are dropped. That is a configuration mistake
which would otherwise repeat on every message, so the route warns **once** and logs later
drops at debug level.

A branch that **fails** nacks the whole fan-out: every branch is delivered at-least-once, so
the answering branch's response is discarded and the caller gets a `500` rather than an answer
that hides a lost message. That is stricter than nginx's `mirror`, which ignores failed mirror
subrequests entirely.

The mirror pattern above is unaffected, because a `request` branch with a non-replying
`forward_to` absorbs its own failure — that is where nginx's "ignore the mirror" semantics
live, opted into per branch. For a plain branch (`- kafka: { … }` directly in the list) the
equivalent is a [`dlq`](#dlq) on that branch: the failure is parked, the branch acks, and the
answering branch still replies.

```yaml route
proxy_with_parked_mirror:
  input: { http: { url: "0.0.0.0:8443", path: "test" } }
  output:
    fanout:
      - kafka: { topic: "audit", url: "localhost:9092" }
        middlewares:
          - dlq: { endpoint: { file: { path: "audit-failures.jsonl" } } }
      - request:
          to: { http: { url: "http://127.0.0.1:1445/" } }
          forward_to: { response: {} }
```

### `switch`

Content-based routing: picks one destination per message. Two modes, and a `switch` uses
exactly one of them — naming both, or neither, is a startup error.

| Field | Type | Required |
|---|---|---|
| `metadata_key` | string | value-lookup mode |
| `cases` | map value → Endpoint | value-lookup mode |
| `when` | list of `{ if, to }` | predicate mode |
| `default` | Endpoint | no |

**Value lookup** matches a **metadata** value exactly. It is a HashMap get and never reads
the payload, so prefer it when the routing key already is metadata.

```yaml endpoint
output:
  switch:
    metadata_key: "http_status_code"
    cases:
      "200": { nats: { subject: "ok", url: "nats://localhost:4222" } }
      "404": { file: { path: "not-found.jsonl" } }
    default: { file: { path: "other.jsonl" } }
```

**Predicate mode** routes on an expression, so it can branch on payload content directly.
Cases are evaluated **in order and the first match wins**, which is what makes overlapping
thresholds safe to write:

```yaml endpoint
output:
  switch:
    when:
      - if: "amount > 100"
        to: { kafka: { topic: "large-orders", url: "localhost:9092" } }
      - if: "amount <= 100"
        to: { nats: { subject: "small-orders", url: "nats://localhost:4222" } }
    default: { file: { path: "unrouted.jsonl" } }
```

`if` takes the same expression language as the [`filter`](#filter) middleware — payload
fields by bare name (`amount`, `order.status`), metadata under `meta.` as text that a numeric
literal reads as a number (`meta.http_status_code >= 400`), `and`/`or` or `&&`/`||`. Predicate mode therefore
needs the `filter` feature; a `when` list in a build without it is a startup error, not a
silent fallback. A payload the expression cannot read fails the send rather than dropping the
message silently. As with `filter`, indexed payload paths such as `items[0].qty` are unsupported
and are rejected at startup.

In either mode, a message that matches nothing goes to `default`; without a `default` it is
dropped with a warning. Value lookup is the cheaper mode and stays the right choice when the
key is already in metadata — for payload-derived keys you can either promote the value into
metadata first (for example with [`transform`](#transform)'s `on_error: pass_through`, which
sets `mqb.transform_error`) or just use `when`.

### `request`

Sends each message to a request-capable endpoint and forwards the **response** somewhere else,
turning a request/reply exchange into a one-way flow.

| Field | Type | Required |
|---|---|---|
| `to` | Endpoint (request-capable) | yes |
| `forward_to` | Endpoint | yes |

```yaml endpoint
output:
  request:
    to: { http: { url: "https://api.internal/score" } }
    forward_to: { ibmmq: { queue: "RESULTS", url: "mq(1414)", queue_manager: "QM1", channel: "APP.SVRCONN" } }
```

`to` must support request/reply: `http`, or a `nats`/`mongodb`/`memory` endpoint with
`request_reply: true`. On error or timeout the **original** message is forwarded instead of a
response, so nothing is lost — distinguish the two downstream with a [`switch`](#switch) on a
status key such as `http_status_code`.

For batch input, requests still run individually because each needs its own reply. They run
concurrently unless `to` requires ordered publishing, in which case they are issued one at a
time in source order. Their responses and error fallbacks are restored to input order and
passed to `forward_to` in one `send_batch` call. Batch-capable sinks such as MongoDB can
therefore use their native bulk write for the forwarding leg.

Whatever `forward_to` returns is passed back up. A plain sink acks, `forward_to: {}` (the
`null` endpoint, also spelled `null`) discards, and `forward_to: { response: {} }` replies to
the origin of the current request — which is how a [`fanout`](#fanout) branch answers the
caller.

The error fallback never becomes that reply: when `forward_to` would answer the caller, a
failed request surfaces the error instead of echoing the original back as a success. The route
then nacks (HTTP `500`), and a [`retry`](#retry) or [`dlq`](#dlq) middleware on the endpoint
sees the failure as usual. Forwarding-to-a-sink still acks, so the `switch` pattern above is
unchanged.

### `response`

Replies to the origin of the current request. Output only, and the recommended way to build
request/reply routes.

```yaml route
http_echo:
  input: { http: { url: "0.0.0.0:8080" } }
  output: { response: {} }
```

Takes no options. Requires an input that carries a reply channel (`http`, `websocket`, `grpc`,
or a request/reply `nats`/`mongodb`/`memory`). With an `http` or `websocket` input and no
middleware, `response` (and `static`) enables an inline fast path that skips the normal route
pipeline. See [README.md](../README.md#patterns-request-response).

### `reader`

An output endpoint that **ignores the incoming payload** and instead reads one message from
the wrapped consumer, returning it as the response. The inbound message is purely a trigger.

```yaml route
# HTTP GET pulls the next message off a Kafka topic.
poll_api:
  input: { http: { url: "0.0.0.0:8080", method: "GET" } }
  output:
    reader:
      kafka: { topic: "queue", url: "localhost:9092" }
```

The value is a single nested endpoint, which must be valid as a **consumer**. The message read
is acknowledged immediately, before the caller has necessarily received it — so a crash in
between loses it. Use it for polling APIs, not for guaranteed delivery.

### `sequence`

Reads several inputs one after another. Each is drained before the next begins, and the last
one streams until the route stops. Input only.

The motivating case is a change-data-capture backfill — snapshot a table, then tail its
replication stream — but the shape fits any history-then-live pair.

#### For a CDC backfill, reach for the endpoint's own `consume` first

**You usually do not need to write a `sequence` by hand.** Both CDC endpoints spell
"backfill, then follow changes" the same way, as one field:

```yaml endpoint
input:
  postgres_cdc:
    url: "postgres://user@localhost/shop"
    publication: "orders_pub"
    consume: capture_all          # page the tables by primary key, then stream changes

input:
  mongodb:
    url: "mongodb://localhost:27017"
    database: "shop"
    collection: "orders"
    consume: capture_all          # page the collection by _id, then stream changes
```

`postgres_cdc: consume: capture_all` **expands to exactly the `sequence` below** — it reads the
publication's tables, takes each table's primary key as the cursor, and appends the CDC phase. Use
the explicit form only when that expansion cannot serve you:

| Situation | Spelling |
|---|---|
| Back fill a Postgres or MongoDB CDC source | `consume: capture_all` on the endpoint |
| A table with a composite or non-integer primary key | `sequence`, with your own `cursor_column` |
| Back fill from somewhere else entirely — a file, an S3 prefix, a different database | `sequence` |
| A custom snapshot query, or only some of the publication's tables | `sequence` |

#### Fields

| Field | Type | Default |
|---|---|---|
| `endpoints` | list of endpoints | required |
| `cursor_id` | string | none |
| `checkpoint_store` | string | none |

```yaml route
# The explicit form. This is what `postgres_cdc: consume: capture_all` becomes.
orders_to_search:
  input:
    sequence:
      endpoints:
        - sqlx: { url: "postgres://user@localhost/shop", table: "orders", cursor_column: "id" }
        - postgres_cdc: { url: "postgres://user@localhost/shop", publication: "orders_pub", slot_name: "orders_slot" }
      cursor_id: "orders_backfill"
      checkpoint_store: "/var/lib/mqb/orders-phase.json"
  output:
    meilisearch: { url: "http://localhost:7700", index: "orders", primary_key: "id" }
```

**The order of operations is what makes this gapless.** Before the first phase reads anything,
every later phase is asked to pin the position it will resume from. For `postgres_cdc` that
creates the replication slot, so Postgres retains WAL from that moment while the snapshot runs,
and the stream later resumes from the slot's own consistent point. Endpoints with nothing to
pin — a table scan, a file, a queue — are left alone.

Consequences worth knowing:

* **Delivery is at-least-once across the handoff.** A row changed while the snapshot was running
  is read twice: once by the snapshot, once from the stream. The sink must be idempotent — an
  upsert keyed on the primary key, which is what a search index or a `ON CONFLICT` sink already
  does. There is no exported-snapshot mode that would deduplicate this.
* **A phase must be able to drain.** An intermediate phase is always run in drain mode, and its
  first empty batch is the handoff signal; only the last phase inherits the route's own
  `exit_on_empty`. An endpoint that never reports empty would never hand off.
* **`postgres_cdc` needs `temporary_slot: false`** when it follows another phase. A temporary
  slot is dropped when the route stops, so a restart would resume with no retained WAL and
  silently skip every change made while the earlier phase ran. This is rejected at startup, for
  `consume: capture_all` as well as for a hand-written `sequence`.
* **Without `cursor_id` + `checkpoint_store` every restart re-enters the first phase.** Whether
  that re-reads anything is then up to that phase's own cursor — a `sqlx` phase with its own
  `cursor_id` picks up where it left off. The marker records which phase was reached, so a
  restart skips the earlier ones outright.
* Phases connect lazily — a later phase opens no connection while an earlier one is still
  running, so a long snapshot does not hold a replication stream open.

**Which endpoints can be a phase: all of them.** Any endpoint valid as an input works in any
position — there is no per-endpoint support list. An intermediate phase is run in drain mode, and every consumer either returns an empty
batch when idle (the poll-style sources: `sqlx`, `clickhouse`, `object_store`, `aws`, `ibm_mq`)
or surfaces one after a short idle timeout — the same mechanism `--drain` uses. That timeout
defaults to 1s and is set process-wide by `MQ_BRIDGE_DRAIN_IDLE_TIMEOUT_MS`, so a handover costs
about a second on a blocking source.

**What differs per endpoint is the pin** — whether the last phase can guarantee it resumes from
before the backfill rather than from "now":

| Last phase | Pinned by | Gapless |
|---|---|:---:|
| `postgres_cdc`, or `sqlx` with a `publication` | Creating the replication slot, so the server retains WAL for the whole backfill | ✅ |
| `mongodb` with `consume: capture_all` | Nothing — it already captures a resume token, snapshots, then streams from that token, inside the consumer | ✅ |
| `kafka`, `nats` JetStream, `redis_streams` | Not pinned. These retain history themselves, so start the consumer at the earliest position rather than relying on a pin | n/a |
| Everything else | Nothing to pin | ⚠️ |

⚠️ means the handover is only as gapless as the source's own retention: a change made *during*
the backfill is missed if the source does not keep it. For a queue or a log that is not a
concern — nothing is dropped while you are not reading. It matters for sources that only expose
"what is true now".


### `static`

A fixed, pre-rendered message. Usable as an output (a constant reply) or an input (a constant
source).

| Field | Type | Default |
|---|---|---|
| `body` | string | required |
| `raw` | bool | `false` |
| `metadata` | map string→string | `{}` |

Accepts either a bare string or the full map form:

```yaml endpoint
output: { static: "OK" }                       # shorthand, body JSON-encoded

output:
  static:
    body: '{"status":"ok"}'
    raw: true                                  # send verbatim, do not JSON-encode
    metadata: { content-type: "application/json" }
```

`raw: true` sends `body` byte-for-byte; the default JSON-encodes it as a string. Like
`response`, a `static` output enables the HTTP inline fast path.

#### Placeholders

`body` is a template compiled **once at startup**; rendering a message never re-parses it.
Tokens use the `${namespace:selector}` form:

| Token | Resolves to |
|---|---|
| `${payload:a.b.c}` | a field of the incoming JSON payload (dotted path; array indices allowed) |
| `${metadata:key}` | a metadata value |
| `${message:id}` | the message id (UUID string) |
| `${gen:uuid}` | a fresh UUID v7 |
| `${gen:now}` / `${gen:timestamp}` | current time (RFC3339 UTC / Unix epoch ms) |
| `${gen:counter}` | a per-endpoint counter, starting at 0 |
| `${gen:random(1,100)}` | a random integer in `[min, max]` |
| `${env:VAR}` | an environment variable, resolved once at startup |

`payload`/`metadata`/`message` read the request, so they are the useful ones on an **output**
(e.g. an error reply that echoes the request); on an **input** (load-test source) only
`gen`/`env` produce values. When the body's `content-type` metadata is a JSON type,
interpolated request values are **JSON-escaped by default** so external data cannot break the
structure — append `| raw` to a token to splice it verbatim. To emit a literal, un-interpolated
`${…}`, write `$${…}` (a bare `$$` is left as-is); any `${…}` with an unknown namespace is also
left untouched.

```yaml endpoint
output:
  static:
    body: '{"error":"not found","id":"${message:id}","at":"${gen:now}"}'
    raw: true
    metadata: { content-type: "application/json" }
```

### `stream_buffer`

An in-memory stream partitioned by correlation ID, used to carry streaming request/response
bodies between routes.

| Field | Type | Notes |
|---|---|---|
| `topic` | string | required; shared by publisher and consumers |
| `correlation_id` | string | **required on consumers, must be unset on publishers** |
| `capacity` | integer | default `100`, per partition |
| `idle_ttl_secs` | integer | default `3600`; `0` disables |

```yaml endpoint
output:
  stream_buffer: { topic: "responses" }        # publisher: no correlation_id

input:
  stream_buffer: { topic: "responses", correlation_id: "req-123" }   # consumer
```

A consumer without `correlation_id` is a startup error; a publisher *with* one logs a warning
and ignores it. Primarily wired up via `HttpConfig::stream_response_to`.

The publisher creates a partition on demand, but only a consumer's shutdown removes one, so
a correlation id whose reader never arrives would hold its buffered batches until the
process exits. `idle_ttl_secs` discards such a partition once nothing has published to it
for that long. A partition with a consumer attached is never discarded, however idle.

### `null`

Discards every message. Output only. This is the **default output** when a route omits one.

```yaml route
drain:
  input: { kafka: { topic: "noisy", url: "localhost:9092" } }
  output: null          # a bare YAML null
```

> Spelling trap: it is a bare YAML `null` (or `~`, or the explicit `null: null`).
> **`null: {}` does not parse.** Omitting `output:` entirely gives the same result.

Useful for consume-and-handle routes where a handler does the work and there is nothing to
forward, and for benchmarking an input in isolation.

### `custom` (endpoint)

Delegates to a factory you registered programmatically.

| Field | Type | Required |
|---|---|---|
| `name` | string | yes |
| `config` | any JSON | yes |

```yaml endpoint
output:
  custom:
    name: "my_sink"
    config: { target: "internal://thing" }
```

Implement `CustomEndpointFactory` and register it before starting routes. Once registered,
the name also works as a bare endpoint key — `input: { my_sink: {...} }` — since any
unrecognised key is looked up in the custom-endpoint registry. Use the explicit `custom:`
form above if you validate configs against `mq-bridge.schema.json`, which cannot know your
key. Endpoints can also be written in Python (`register_endpoint`) or JavaScript
(`registerEndpoint`). See **[EXTENDING.md](EXTENDING.md)** for the full guide.

---

## See also

- [README.md](../README.md) — overview, data endpoints, request/response and CQRS patterns
- [CONFIGURATION.md](CONFIGURATION.md) — full YAML examples, env vars, TLS, IDE schema validation
- [DELIVERY.md](DELIVERY.md) — delivery guarantees, per-source identity, per-sink idempotency
- [ARCHITECTURE.md](ARCHITECTURE.md) — internals, batching/concurrency, extension traits
- [EXTENDING.md](EXTENDING.md) — writing your own endpoint or middleware, in Rust, Python or Node
