# Typesense

`typesense` is an output that writes documents to a Typesense collection. Its
import API takes one document per line and answers with one result line per
document, so a rejected document fails alone. Tested against Typesense 29.

```sh
mqb copy --drain --batch-size 10000 \
  'file://books.jsonl?format=raw' \
  'typesense://localhost:8108/books?api_key=<api key>'
```

`typesense+https://host/books` connects over HTTPS.

On this page:

- [Before the first run](#before-the-first-run)
- [Keep a collection in sync with a Postgres table](#keep-a-collection-in-sync-with-a-postgres-table)
- [One-off load](#one-off-load)
- [Options](#options)
- [What to know](#what-to-know)

## Before the first run

Typesense does not create a collection on import. Create it once:

```sh
curl -X POST http://localhost:8108/collections \
  -H 'X-TYPESENSE-API-KEY: <api key>' \
  -H 'Content-Type: application/json' \
  -d '{"name": "books", "fields": [
        {"name": "title", "type": "string"},
        {"name": "year", "type": "int32"}]}'
```

Every document needs an `id`, and it must be a string. A numeric `id`, which is
what an integer primary key gives, is rejected with "Document's `id` field
should be a string". The `transform` below turns it into one and leaves every
other field as it is.

## Keep a collection in sync with a Postgres table

```yaml
books_to_search:
  batch_size: 10000
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost/app"
      publication: "books_pub"
      slot_name: "mqb_typesense"
      consume: capture_all                 # backfill first, then stream changes
      cursor_id: "books_backfill"
      checkpoint_store: "file:///var/lib/mq-bridge/books-phase.json"
  output:
    middlewares:
      - transform:
          schema:
            type: object
            properties:
              id: { type: string }
      - retry: { max_attempts: 5 }
    custom:
      name: typesense
      config:
        url: "typesense://localhost:8108"
        api_key: "<api key>"
        collection: books
        operation: "${metadata:postgres.operation}"
```

Inserts and updates become upserts. A delete removes the document whose `id`
is the deleted row's primary key, so the table needs a primary key named `id`. See [Postgres CDC](../tutorials/postgres-cdc.md)
for the publication and the slot.

## One-off load

```sh
echo '{"type":"object","properties":{"id":{"type":"string"}}}' > id-as-string.json

mqb copy --drain --batch-size 10000 \
  'file://books.jsonl?format=raw' \
  'typesense://localhost:8108/books?api_key=<api key>|transform?schema_file=id-as-string.json'
```

Loading 20,000 small documents this way took under a second on a laptop.

## Options

| Field | Default | Meaning |
|---|---|---|
| `url` | required | `typesense://host:8108`, `typesense+https://host` or an `http(s)://` URL |
| `collection` | required | Collection to write to; the path of the URI |
| `api_key` | none | Sent as `X-TYPESENSE-API-KEY` |
| `operation` | none | Template for a message's operation; `delete` or `d` removes the document |
| `compression` | `none` | `gzip`, `zstd` or `lz4` request bodies; check that your server reads the codec, a refusal does not name it |
| `request_timeout_ms` | none | Request timeout |

`typesense` is the generic [`http_bulk`](./http-bulk.md) output with these
requests filled in. For anything it does not offer, such as another import
action or extra headers, write the `http_bulk` form:

```yaml
output:
  http_bulk:
    url: http://localhost:8108
    headers:
      X-TYPESENSE-API-KEY: <api key>
    operation: "${metadata:postgres.operation}"
    upsert:
      path: /collections/books/documents/import?action=upsert
      content_type: text/plain
      result:
        lines:
          success: /success
          error: /error
    delete:
      method: DELETE
      path: /collections/books/documents?filter_by=id:[{ids}]
      max_ids: 100
```

## What to know

- **A document without `id` is still written.** Typesense invents an id, and a
  later delete cannot find the document.
- **A rejected document fails alone**, with Typesense's reason, for example
  "Field `year` must be an int32". Add a [`dlq`](../cookbook/dlq.md) to keep it.
- **Deletes go into the URL**, 100 ids per request, and an id containing a
  comma cannot be deleted this way.
- **No collection management.** Schema changes, aliases and synonyms stay with
  the Typesense API.

The behaviour on errors is described on the [`http_bulk`](./http-bulk.md) page.
