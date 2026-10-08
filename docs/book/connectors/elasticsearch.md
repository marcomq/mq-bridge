# Elasticsearch

`elasticsearch` is an output that writes documents through the `_bulk` API: an
action line before each document, and a response with one entry per document,
so a rejected document fails alone. Tested against Elasticsearch 8.19.
OpenSearch has the same `_bulk` API but has not been tried.

```sh
mqb copy --drain --batch-size 5000 \
  'file://books.jsonl?format=raw' \
  'elasticsearch://localhost:9200/books?api_key=<api key>'
```

`elasticsearch+https://host/books` connects over HTTPS, and `?mode=update`
merges each payload into the stored document instead of replacing it.

## Keep an index in sync with a Postgres table

```yaml
books_to_search:
  batch_size: 5000
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost/app"
      publication: "books_pub"
      slot_name: "mqb_elastic"
      consume: capture_all                 # backfill first, then stream changes
      cursor_id: "books_backfill"
      checkpoint_store: "file:///var/lib/mq-bridge/books-phase.json"
  output:
    middlewares:
      - retry: { max_attempts: 5 }
    custom:
      name: elasticsearch
      config:
        url: "elasticsearch://localhost:9200"
        api_key: "<api key>"
        index: books
        operation: "${metadata:postgres.operation}"
        compression: gzip
```

The document `_id` is the row's `id` column, so an update replaces the document
and a delete finds it. Name another column with `id_field`.

Every document needs a string or a number there. Without the field, or with
`null` in it, Elasticsearch refuses that document with `if _id is specified it
must not be empty`, which does not name the field. An object or an array is sent
as its JSON text and becomes the `_id` as written.

See [Postgres CDC](../tutorials/postgres-cdc.md) for the publication and the slot.

## Options

| Field | Default | Meaning |
|---|---|---|
| `url` | required | `elasticsearch://host:9200`, `elasticsearch+https://host` or an `http(s)://` URL |
| `index` | required | Index to write to; the path of the URI |
| `api_key` | none | Sent as `Authorization: ApiKey <api key>` |
| `id_field` | `id` | Top-level payload field that becomes the document `_id` |
| `mode` | `index` | `index` replaces the document; `update` merges the payload into it and creates it if missing |
| `auth` | none | `oauth2` or `aws_sigv4`, as on [`http_bulk`](./http-bulk.md#fields); in a URI, JSON: `auth={"aws_sigv4":{"region":"eu-central-1","service":"es"}}` |
| `operation` | none | Template for a message's operation; `delete` or `d` removes the document |
| `compression` | `none` | `gzip` request bodies. `zstd` and `lz4` are accepted here, but Elasticsearch 8.19 answers them with a "malformed" 400 |
| `request_timeout_ms` | none | Request timeout |

`elasticsearch` is the generic [`http_bulk`](./http-bulk.md) output with these
requests filled in. For anything it does not offer, such as basic
authentication or another bulk action, write the `http_bulk` form:

```yaml
output:
  http_bulk:
    url: http://localhost:9200
    headers:
      Authorization: ApiKey <api key>
    operation: "${metadata:postgres.operation}"
    compression: gzip
    upsert:
      path: /books/_bulk
      action: '{"index":{"_id":"${payload:id}"}}'
      result:
        items:
          path: /items
          error: /index/error/reason
    delete:
      path: /books/_bulk
      line: '{"delete":{"_id":{id}}}'
      result:
        items:
          path: /items
          error: /delete/error/reason
```

## What to know

- **A rejected document fails alone**, with Elasticsearch's reason, for example
  "failed to parse field [year] of type [integer]". Add a
  [`dlq`](../cookbook/dlq.md) to keep it.
- **`index` replaces the whole document.** `mode: update` sends
  `{"update":{"_id":…}}` and `{"doc":<payload>,"doc_as_upsert":true}` instead:
  fields the payload leaves out keep their stored value. It was run against a
  stub server only, not against Elasticsearch.
- **`auth` was run against a stub server only**, for OAuth2 and for SigV4
  (Amazon OpenSearch Service).
- **The test ran with security disabled.** `api_key` is sent in the form
  Elasticsearch documents for an API key.
- **Mappings and index settings are not managed.** Create the index first if
  dynamic mapping is not what you want.
- **Another data path already exists for Kafka users:** Kafka Connect's
  Elasticsearch sink reads a topic that mq-bridge writes. This endpoint is for
  writing without Kafka in between.

The behaviour on errors is described on the [`http_bulk`](./http-bulk.md) page.
