# Enrichment & lookups

The [`lookup`](../engine/reference.md#lookup) middleware asks another endpoint for each message
and writes the answer into the payload. Use it to join an event with master data, or to add
features such as a previous value or a moving average (for counters and averages that need
no database, see [running aggregates](aggregate.md)). The endpoint asked must answer:
`http`, `static`, `mongodb` with `find`, `sqlx` or `clickhouse` with `lookup_query`, `nats` /
`memory` with `request_reply: true`, or `grpc` to an mq-bridge `grpc` input whose route replies.
For a model's answer or an embedding, see [LLM calls in a pipeline](llm.md).

## Enrich, then route on the result

On an output, the enriched message goes on to the next endpoint. `lookup.found` tells a
`switch` whether the record exists:

```yaml
orders_enrich:
  input: { kafka: { topic: "orders", url: "localhost:9092" } }
  output:
    middlewares:
      - lookup:
          from:
            sqlx:
              url: "postgres://localhost/crm"
              table: "customers"
              lookup_query: "SELECT id, name, tier FROM customers WHERE id IN (${payload:customer_id}::int)"
          into: customer
    switch:
      metadata_key: "lookup.found"
      cases:
        "true":  { kafka: { topic: "orders.enriched", url: "localhost:9092" } }
        "false": { kafka: { topic: "orders.unknown_customer", url: "localhost:9092" } }
```

Because the key sits inside `IN (…)`, one query answers the whole batch and rows are matched
back to messages by `id`. Write `WHERE id = ${payload:customer_id}` instead for one query per
message. A failed lookup fails only its message; list `retry` / `dlq` after `lookup` to handle it.

## Several lookups, before the handler

On an input, the handler already sees the enriched message. Lookups in `entries` run in
parallel, so a batch waits for the slowest one, not the sum:

```yaml
input:
  kafka: { topic: "payments", url: "localhost:9092" }
  middlewares:
    - lookup:
        concurrency: 64
        entries:
          - into: features.user
            from:
              mongodb:
                url: "mongodb://localhost:27017"
                database: "features"
                collection: "users"
                find: '{"_id": "${payload:user_id}"}'
          - into: features.card_avg
            from:
              sqlx:
                url: "postgres://localhost/payments"
                table: "payments"
                lookup_query: >-
                  SELECT avg(amount)::text AS avg20 FROM (SELECT amount FROM payments
                  WHERE card_id = ${payload:card_id} ORDER BY ts DESC LIMIT 20) t
```

Each entry sets `lookup.<into>.found`; `lookup.found` is `true` only when all of them found
something. On an input a temporary error nacks the whole batch, so the source redelivers it,
and reconnects the route. A permanent error (non-JSON payload, HTTP 4xx, a query error) is
logged and drops only its message.

## Postgres gotchas

- Cast placeholders to the column type: `WHERE id = ${payload:author_id}::int`. A field
  missing from the payload, e.g. on a CDC delete, is otherwise bound as text and fails.
- Cast `NUMERIC`, `TIMESTAMPTZ` and similar result columns to `::text`. They arrive as JSON
  strings.
- A per-message `lookup_query` returns the first row only; use `ORDER BY … LIMIT 1` for
  "the latest". A batched (`IN`) query takes no plain `LIMIT`.
