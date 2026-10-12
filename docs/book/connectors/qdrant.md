# Qdrant

`qdrant` writes points to a Qdrant collection, removes them by id, and reads a
collection once from start to end. A point is `{id, vector, payload}`; the
vector comes from an embeddings API, which
[`openai_embeddings`](./openai.md#openai_embeddings) asks once per batch.

```sh
mqb copy --drain \
  'file://points.jsonl?format=raw' \
  'qdrant://localhost:6333/docs?api_key=<api key>'
```

`qdrant+https://host/docs` connects over HTTPS.

Upserts and the read were run against Qdrant 1.19. A delete through this
endpoint was run against a stub server only; its request is the one of the
[`http_bulk` recipe](./http-bulk.md#qdrant), which was run against Qdrant.

## Before the first run

Qdrant does not create a collection on write. Create it once, with the vector
size of your embedding model:

```sh
curl -X PUT http://localhost:6333/collections/docs \
  -H 'Content-Type: application/json' \
  -d '{"vectors": {"size": 384, "distance": "Cosine"}}'
```

A point id is an unsigned integer or a UUID. Any other id is refused.

## Keep a collection in sync with a Postgres table

Inserts and updates are embedded and upserted; a delete removes the point and
calls no model. Output middlewares run bottom-up.

```yaml
docs_to_qdrant:
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost/app"
      publication: "docs_pub"
      slot_name: "docs_to_qdrant"
      consume: capture_all                 # backfill first, then stream changes
      checkpoint_store: "file:///var/lib/mq-bridge/docs-phase.json"
  output:
    switch:
      metadata_key: "postgres.operation"
      cases:
        delete:
          qdrant: &qdrant
            url: "http://localhost:6333"
            collection: docs
            operation: "${metadata:postgres.operation}"
      default:
        middlewares:
          # 2. Shape the point: {id, vector, payload}.
          - transform:
              mapping:
                id: "$.id"
                vector: "$.vector"
                "payload.title": "$.title"
                "payload.body": "$.body"
          # 1. One embeddings request per batch; the vector lands in `vector`.
          - lookup:
              from:
                openai_embeddings:
                  url: "http://localhost:11434"
                  model: all-minilm
                  input: "${payload:body}"
              into: vector
          - retry: { max_attempts: 5 }
        qdrant: *qdrant
```

[Keep a vector index in sync with Postgres](../use-cases/sync-postgres-to-qdrant.md)
explains the pipeline, its failure behaviour and its limits, with a runnable
example.

## Copy a collection out

```sh
mqb copy --drain 'qdrant://localhost:6333/docs' 'file://points.jsonl?format=raw'
```

Each message is one point with its payload and vector. It is a copy, not a
change feed: points written behind the read position are missed. Set
`cursor_id` and `checkpoint_store` to continue after a restart.

## Options

| Field | Default | Meaning |
|---|---|---|
| `url` | required | `qdrant://host:6333`, `qdrant+https://host` or an `http(s)://` URL |
| `collection` | required | Collection to write to or read; the path of the URI |
| `api_key` | none | Sent as `api-key` |
| `wait` | `true` | Qdrant answers after the points are stored. `false` answers when they are queued, so a later failure is not reported |
| `operation` | none | Template for a message's operation; `delete` or `d` removes the point |
| `compression` | `none` | `gzip`, `zstd` or `lz4` request bodies; not tried against Qdrant |
| `request_timeout_ms` | see right | Request timeout; `0`: none. Unset: a request fails after 300 s without a response byte |
| `cursor_id` | none | Input only: names the saved read position |
| `checkpoint_store` | none | Input only: where the position is saved, e.g. `file:///var/lib/mqb/cursors.json` |
| `tls` | none | TLS options as on [`http_bulk`](http-bulk.md): `ca_file`, `cert_file` + `key_file`, `required`, `accept_invalid_certs`. JSON text in a URI. |

`qdrant` is the generic [`http_bulk`](./http-bulk.md) endpoint with these
requests filled in:

```yaml
output:
  http_bulk:
    url: http://localhost:6333
    headers:
      api-key: <api key>
    operation: "${metadata:postgres.operation}"
    upsert:
      method: PUT
      path: /collections/docs/points?wait=true
      format: json_array
      envelope: '{"points":{documents}}'
    delete:
      path: /collections/docs/points/delete?wait=true
      envelope: '{"points":{ids}}'
```

From Rust, `mq_bridge::endpoints::http_bulk::QdrantConfig` holds the same
fields and converts into an `Endpoint` with `.into()`.

For a vector search per message, use the
[batch search lookup](./http-bulk.md#lookups) of `http_bulk`.

## What to know

- **Qdrant answers for the whole request.** One refused point, for example a
  vector of the wrong size, fails every point of that request for good. Add a
  [`dlq`](../cookbook/dlq.md) to keep them.
- **An upsert replaces the point**, payload and vector. A repeated upsert or
  delete after a restart changes nothing, so at-least-once delivery converges.
- **No collection management.** Creating collections, indexes and aliases stays
  with the Qdrant API.

The behaviour on errors is described on the [`http_bulk`](./http-bulk.md) page.
