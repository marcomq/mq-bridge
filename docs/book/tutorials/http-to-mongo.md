# HTTP webhook → MongoDB

Accept incoming HTTP webhooks on a local port and persist each request body as a
document in MongoDB, with automatic retries on transient write failures. This is a
config-driven route rather than a one-liner, because it wires middleware into the
pipeline.

## The route

`mq-bridge.yaml`:

```yaml
webhook_to_mongo:
  input:
    http:
      url: "127.0.0.1:8080"
      # Force the normal route pipeline instead of the inline HTTP response fast path.
      inline_response_fast_path: false
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "webhooks"
      format: "json"   # a bit slower, but stores readable documents
    middlewares:
      - retry:
          max_attempts: 3
          initial_interval_ms: 500
```

Run it in config mode:

```bash
mqb --config mq-bridge.yaml
```

Then POST to it:

```bash
curl -X POST http://localhost:8080 \
  -H 'content-type: application/json' \
  -d '{"event":"signup","user":"alice"}'
```

The document lands in `app_db.webhooks`.

## What each piece does

- **`http` input** listens on `127.0.0.1:8080`, i.e. localhost only. The endpoint is
  unauthenticated, so bind it to `0.0.0.0` only behind an authenticating proxy or a
  network you control. Setting `inline_response_fast_path: false` forces requests
  through the full route pipeline (input → middleware → output) instead of the fast
  inline-response path, so the `retry` middleware and the MongoDB write both run
  before the HTTP response is sent.
- **`retry` middleware** re-attempts a failed write up to `max_attempts` times,
  starting at `initial_interval_ms` and backing off. See the
  [Retries & backoff](../cookbook/retries.md) recipe for the full knob set and its
  interaction with a [dead-letter queue](../cookbook/dlq.md).

> [!NOTE]
> Delivery here is **at-least-once**: a write that actually succeeded but whose
> acknowledgement was lost gets retried and inserts a second document. For an
> idempotent sink, set `id_field` on the `mongodb` output to a stable business key so
> the value becomes the document `_id`: a retry of an already-written message is then
> rejected by the unique `_id` instead of appending a new document — see
> [Upserts](../cookbook/upserts.md).
- **`mongodb` output** with `format: "json"` stores each payload as a readable JSON
  document rather than the default full-message serialization.

## Variations

- **Want a synchronous reply to the caller?** Attach a handler and use a `response`
  output — see the [Request / reply](request-reply.md) tutorial.
- **High request rates?** The `http` input scales with connection concurrency; see
  [Performance tuning](../operations/tuning.md).

## See also

- [Configuration grammar](../engine/configuration.md) — this is Route 2 of the annotated example.
- [MongoDB connector](../connectors/mongodb.md) and [parameter reference](../reference/mongodb.md).
- [HTTP connector](../connectors/http.md) and [parameter reference](../reference/http.md).
