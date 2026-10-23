# HTTP bulk (search engines)

Writes JSON documents in bulk to an HTTP API, and reads them back page by page:
one request per batch instead of one per message. It is meant for search engines
and similar document stores. The endpoint knows no product by name. As an output
you describe three things, and a new target is a few lines of configuration:

- **upsert**: the request that writes documents, and the body it carries;
- **delete**: the request that removes documents by id;
- **result**: how the target reports what happened.

As an input you describe one: **read**, the request that lists a page of
documents and where the next page starts. See [Reading](#reading). With
`read.stream` it reads one response that stays open instead: server-sent events
or NDJSON lines. See [An open stream](#an-open-stream-sse-and-ndjson).

For a [`lookup`](../cookbook/lookup.md) you describe **query**: the request that
answers a whole batch of lookups at once. See [Lookups](#lookups).

It needs the `http-bulk` feature, which `full` includes.

## A complete example

A route that copies a PostgreSQL table to a REST API, 2,000 rows per request. The API in this
example takes `POST /v1/products/batch` with the body `{"items": [ … ]}` and a bearer token:

```yaml
products_to_api:
  batch_size: 2000
  input:
    sqlx:
      url: "postgres://app:secret@localhost/shop"
      table: "products"
      cursor_column: "id"
      cursor_id: "products-to-api"
  output:
    middlewares:
      - retry: { max_attempts: 5, initial_interval_ms: 500 }
      - dlq: { endpoint: { file: { path: "refused-products.jsonl" } } }
    http_bulk:
      url: https://api.example.com
      headers:
        Authorization: "Bearer ${API_TOKEN}"
      upsert:
        path: /v1/products/batch
        format: json_array
        envelope: '{"items": {documents}}'
        split_on_reject: true
      max_request_bytes: 5000000
```

Save it as `products.yaml` and run it with `mqb --config products.yaml`. What happens:

1. The input reads up to 2,000 rows; each row is one message with a JSON object as payload.
2. The output puts the payloads into one array, wraps it in the `envelope` and sends one
   `POST`. If the body would exceed `max_request_bytes`, it sends several.
3. A 2xx answer means all rows were written, and the input saves its position.
4. A `429` or `5xx` answer is tried again by `retry`. A `400` or `422` is sent again in halves
   (`split_on_reject`) until the rows the API refuses stand alone; those go to the file of the
   `dlq`, and the rest is written.

Which fields your API needs follows from a few questions:

| Question about the API | Field |
| :--- | :--- |
| One JSON document per line, or one JSON array? | `upsert.format`: `ndjson` or `json_array` |
| Is the array inside an object? | `upsert.envelope`, e.g. `'{"items": {documents}}'` |
| Does every document need a line before it (Elasticsearch `_bulk`)? | `upsert.action` |
| Does the response report each document? | `upsert.result`: `lines` or `items`; none if the status says it all |
| Does it answer with a job to wait for? | `upsert.result.job` |

## Authentication

A fixed key or token goes in `headers`. Take the value from the environment so that it is
not in the file:

```yaml
output:
  http_bulk:
    url: https://search.example.com
    headers:
      X-API-Key: "${SEARCH_API_KEY}"
    upsert: { path: /docs/_bulk }
```

An OAuth2 client (client credentials grant). The token is fetched once, reused, and renewed
before it expires:

```yaml
output:
  http_bulk:
    url: https://api.example.com
    auth:
      oauth2:
        token_url: https://login.example.com/oauth2/token
        client_id: "bridge"
        client_secret: "${OAUTH_CLIENT_SECRET}"
        scope: "products.write"
    upsert:
      path: /v1/products/batch
      format: json_array
```

A login that answers with a token in JSON. The body is sent as it is; `token` and
`expires_at` (Unix seconds) or `expires_in` (seconds from now) are JSON pointers into the
answer. Without either the token is kept until a request is answered 401, and so is one
whose expiry is in milliseconds. The body is not JSON-escaped: a password with a `"` or
`\` in it must be escaped in the environment variable:

```yaml
output:
  http_bulk:
    url: https://api.example.com
    auth:
      login:
        url: https://api.example.com/users/login
        body: '{"username":"bridge","password":"${API_PASSWORD}"}'
        token: /access_token/token
        expires_at: /access_token/expiry
    upsert:
      path: /v1/items
      format: json_array
```

Amazon OpenSearch Service, with requests signed by the AWS credentials of the environment
(`service: aoss` for OpenSearch Serverless):

```yaml
output:
  http_bulk:
    url: https://search-shop-abc123.eu-central-1.es.amazonaws.com
    auth:
      aws_sigv4: { region: eu-central-1, service: es }
    upsert:
      path: /products/_bulk
      action: '{"index":{"_id":"${payload:id}"}}'
      result:
        items: { path: /items, error: /index/error/reason }
```

The OAuth2, login and SigV4 examples were checked for their configuration only, not run against
those services.

## On the command line

The configuration is too nested for query parameters, so the `http-bulk:` scheme
takes it whole: from a file with `config_file`, or inline as YAML or JSON with
`config`. The value holds the fields listed below, with or without the
`http_bulk:` key around them. `url` replaces the target, so one file serves
several hosts.

```sh
mqb copy 'postgres://app@localhost/shop?table=books' \
  'http-bulk:?config_file=meilisearch.yaml&url=http://search:7700'

mqb copy 'file://books.jsonl' \
  'http-bulk:?config={url: "http://localhost:8108", upsert: {path: /docs}}'

mqb copy 'http-bulk:?config_file=couchdb-changes.yaml' 'file://changes.jsonl' --drain
```

Percent-encode a `&`, `#` or `?` inside an inline `config`; a file needs no
escaping.

## Tested targets

Each recipe was run against the version named. Anything else with a bulk HTTP
API is a few lines of configuration away, but has not been tried.

| Target | Version | Upsert | Delete | Failures reported | Read |
| --- | --- | --- | --- | --- | --- |
| [Meilisearch](#meilisearch) | 1.53 | yes | yes | per request (task) | [by offset](#by-offset-meilisearch) |
| [Typesense](./typesense.md) | 29 | yes | yes | per document | not tried |
| [Elasticsearch](./elasticsearch.md) | 8.19 | yes | yes | per document | not tried |
| [PostgREST / Supabase](./postgrest.md) | PostgREST 16.4 | yes | yes | per request | [by key](#by-key-postgrest) |
| [Qdrant](#qdrant) | 1.19 | yes | yes | per request | [full scan](#full-scan-qdrant) |
| [CouchDB](#couchdb) | 3.5 | insert only | no | per document | [change feed](#change-feed-couchdb) |

## Recipes

In the recipes `operation` reads the operation a Postgres change stream sets,
which tells an upsert from a delete. For sending mail in bulk (SendGrid, Amazon
SES) see [SMS & email notifications](../cookbook/notifications.md).

### Meilisearch

Meilisearch answers at once with a task and indexes later, so the outcome is a
polled job. A failed task fails every document of its request.

```yaml
output:
  http_bulk:
    url: http://localhost:7700
    headers:
      Authorization: Bearer <api key>
    operation: "${metadata:postgres.operation}"
    upsert:
      path: /indexes/books/documents?primaryKey=id
      result: &task
        job:
          id: /taskUid
          poll: /tasks/{id}
          status: /status
          succeeded: [succeeded]
          failed: [failed, canceled]
          error: /error/message
    delete:
      path: /indexes/books/documents/delete-batch
      result: *task
```

The built-in [`meilisearch`](./meilisearch.md) endpoint is this recipe under a
name of its own. Index settings and partial updates are in the separate plugin. Loading 200,000 small
documents took the same time with either (about 4 to 5 s at a batch size of
50,000 on a laptop).

### Qdrant

The [`qdrant`](./qdrant.md) endpoint is this recipe under a name.

A point is `{id, vector, payload}`, and the id is an unsigned integer or a UUID,
so shape the message with a [`transform`](../cookbook/transform.md) first.
`wait=true` makes Qdrant answer after the points are stored.

```yaml
output:
  http_bulk:
    url: http://localhost:6333
    operation: "${metadata:postgres.operation}"
    upsert:
      method: PUT
      path: /collections/books/points?wait=true
      format: json_array
      envelope: '{"points": {documents}}'
    delete:
      path: /collections/books/points/delete?wait=true
      envelope: '{"points": {ids}}'
```

### CouchDB

Inserts only, unless the messages carry the document's current `_rev`: CouchDB
updates and deletes a document by it, so without it a document whose `_id`
exists is reported as a conflict and fails alone. Where a delete message holds
`_id` and `_rev`, the `line` of `delete` sends them:

```yaml
output:
  http_bulk:
    url: http://localhost:5984
    headers:
      Authorization: Basic <base64 of user:password>
    operation: "${payload:op}"
    upsert:
      path: /books/_bulk_docs
      format: json_array
      envelope: '{"docs": {documents}}'
      result:
        items:
          error: /reason
    delete:
      path: /books/_bulk_docs
      id_field: _id
      envelope: '{"docs": {ids}}'
      line: '{"_id": {id}, "_rev": "${payload:_rev}", "_deleted": true}'
      result:
        items:
          error: /reason
```

## Reading

As an `input` the endpoint asks for one page per batch. `{limit}` in the request
is the route's `batch_size`, and `{cursor}` is the read position. Each document
of the page becomes one message with the document as its JSON payload. Where the
position after a page comes from is the only thing that differs between APIs:

| `read.cursor` | Next position | Fits |
| --- | --- | --- |
| unset | the number of documents read so far | `offset=` paging |
| `item: <pointer>` | a field of the last document | `id > last` paging |
| `response: <pointer>` | a value in the response | change feeds, scroll tokens |
| `next_url: <pointer>` | a URL in the response | `"next": "https://…"` links |
| `link: true` | the `Link` header's `rel="next"` URL | GitHub-style paging |

An API that takes the next offset rather than the last one read gets `cursor.add: 1`,
which adds to the whole number at `cursor.item`. An API that wraps each record in an
envelope, say `{"offset": 7, "payload": "aGVsbG8="}`, gets `read.payload: /payload` and
`read.payload_encoding: base64`: the message is then the decoded bytes, not the document.
The position is still taken from the whole document. A checkpoint saved before `add` was
set holds the last offset read, so the first page after the change repeats that record. A
document whose `payload` is missing or does not decode stops the read at its page: the
route fails there until the API stops returning it.

A `response` cursor ends the read when the value is `null` or an empty string. Some APIs
leave the field out on the last page instead (Google's `nextPageToken`): set
`cursor.end_when_missing: true` for those. It is off by default because a mistyped pointer
would otherwise read one page and report success.

With `cursor_id` and `checkpoint_store` in `read` the position is saved after every
acknowledged batch and a restart continues there. Without them every start
reads from the beginning.

A next-page URL may be relative; it is resolved against the page that named it. One on
another scheme, host or port stops the read, so `headers` and `auth` are never sent
elsewhere. The first request uses `read.path`, which may hold `{limit}`; the saved
position is the next URL, so a restart asks for that page. The end is saved as well, so a
restart after the last page reads nothing; `mqb checkpoint` resets it.

### Next-page links: GitHub

```yaml
input:
  http_bulk:
    url: https://api.github.com
    headers:
      Authorization: "Bearer ${GITHUB_TOKEN}"
    read:
      path: /repos/OWNER/REPO/issues?state=all&per_page={limit}
      cursor: { link: true }
```

An API that puts the link into the body, such as `{"data": [...], "links": {"next": "..."}}`,
takes `items: /data` and `cursor: { next_url: /links/next }`.

### Change feed: CouchDB

A real change stream: updates and deletes arrive too, and the feed never ends.
A deleted document has `"deleted": true` next to its `doc`.

```yaml
input:
  http_bulk:
    url: http://localhost:5984
    headers:
      Authorization: Basic <base64 of user:password>
    read:
      path: /books/_changes?include_docs=true&limit={limit}&since={cursor}
      items: /results
      cursor:
        response: /last_seq
        start: 0
      cursor_id: books
      checkpoint_store: file:///var/lib/mqb/cursors.json
```

### Full scan: Qdrant

Scroll reads every point once. Qdrant answers the last page with a `null`
offset, which ends the read; the end is saved, so a restart reads nothing. It is
a copy, not a change feed: points written behind the position are missed.

```yaml
input:
  http_bulk:
    url: http://localhost:6333
    read:
      path: /collections/books/points/scroll
      body: '{"limit": {limit}, "offset": {cursor}, "with_payload": true, "with_vector": true}'
      items: /result/points
      cursor:
        response: /result/next_page_offset
```

### By offset: Meilisearch

```yaml
input:
  http_bulk:
    url: http://localhost:7700
    headers:
      Authorization: Bearer <api key>
    read:
      path: /indexes/books/documents?offset={cursor}&limit={limit}
      items: /results
```

An offset is only stable while nothing is deleted or reordered during the read.

### By key: PostgREST

The response is the array itself, so `items` stays empty. New rows with a larger
key are picked up by later polls; updates of rows already read are not.

```yaml
input:
  http_bulk:
    url: http://localhost:3000
    read:
      path: /books?id=gt.{cursor}&order=id&limit={limit}
      cursor:
        item: /id
        start: 0
```

With direct access to the database, the [Postgres connector](./postgres.md) is
the better source: its change stream sees updates and deletes.

### An open stream: SSE and NDJSON

With `read.stream` the endpoint sends one request and reads the response while
it arrives, instead of asking for pages. Every event or line is one message, and
a batch is whatever has arrived, up to the route's `batch_size`.

| `stream` | A message is | Read position |
| --- | --- | --- |
| `sse` | the `data` of one server-sent event | the event's `id`, sent back as `Last-Event-ID` |
| `ndjson` | one line | `cursor.item`, a field of the line, put where `{cursor}` stands; none without it |

```yaml
input:
  http_bulk:
    url: https://stream.wikimedia.org
    headers:
      User-Agent: my-bridge/1.0 (ops@example.com)
    read:
      path: /v2/stream/recentchange
      stream: sse
      cursor_id: recentchange
      checkpoint_store: file:///var/lib/mqb/cursors.json
```

An event's `id` and `event` fields are the `sse_id` and `sse_event` metadata of
its message. The payload is the `data` as sent; it need not be JSON. For `sse`,
`cursor.item` takes the position from a field of the data instead of the id.

An export that is one response is read once by a draining route (`--drain` on
the `mqb` command line):

```yaml
input:
  http_bulk:
    url: http://localhost:8108
    headers:
      X-TYPESENSE-API-KEY: ${TYPESENSE_API_KEY}
    read:
      path: /collections/books/documents/export
      stream: ndjson
```

What differs from reading pages:

- **The connection is opened again.** When the response ends or is cut, the
  input waits `polling_interval_ms` and connects from the read position. Without
  a position that reads the same response again, so read a finite export with
  `--drain`, or give it `cursor.item` and `{cursor}`.
- **A draining route ends with the response**, or when nothing arrives for
  `polling_interval_ms`. A response that is cut fails the route instead of
  passing for a complete one.
- **A failed message is asked for again** by connecting from the last
  acknowledged position. An `ndjson` line without `cursor.item` has no position
  and is not read again.
- **`request_timeout_ms` is the longest silence**, not the length of the
  response. A server that sends keep-alive comments stays connected.
- **`{limit}`, `items` and `cursor.response` are refused**: the server decides
  what it sends, and there is no response around the documents.
- **An event or line is held in memory until it is complete.** One above 64 MiB
  stops the route.

The Wikimedia stream above was read with this configuration, restart included.
The export was run against a stub server, not against Typesense.

## Lookups

With `query` the endpoint writes nothing: it answers the
[`lookup`](../cookbook/lookup.md) middleware, one request for a whole batch
of messages instead of one per message. Each message renders `request`, the
parts are joined into one body, and the answers are matched to the messages by
their position in the array at `responses`. The endpoint keeps no state and
caches no answers; only an `auth.oauth2` token is cached, as described under
[Behaviour](#behaviour).

This output looks up each order's book in Elasticsearch with one `_mget` per
batch, then writes the enriched orders:

```yaml
output:
  middlewares:
    - lookup:
        from:
          http_bulk:
            url: http://localhost:9200
            query:
              path: /books/_mget
              format: json_array
              request: '{"_id":"${payload:isbn}"}'
              envelope: '{"docs":{requests}}'
              responses: /docs
              value: /_source
              found: /found
        into: book
  http_bulk:
    url: http://localhost:9200
    upsert:
      path: /orders/_bulk
      action: '{"index":{"_id":"${payload:id}"}}'
```

Two more shapes, as the `query` section alone:

| Target | `query` |
| --- | --- |
| Elasticsearch `_msearch` | `path: /books/_msearch`, `request:` two lines, `{}` and `{"query":{"term":{"isbn":"${payload:isbn}"}},"size":1}`, `responses: /responses`, `value: /hits/hits/0/_source`, `error: /error/reason` |
| Qdrant batch search | `path: /collections/books/points/search/batch`, `format: json_array`, `request: '{"vector":${payload:vector \| raw},"limit":3}'`, `envelope: '{"searches":{requests}}'`, `responses: /result` |

These three shapes were run against a stub server only, not against
Elasticsearch or Qdrant.

## Fields

| Field | Default | Meaning |
| --- | --- | --- |
| `url` | required | Base URL of the target |
| `headers` | none | Sent with every request; stored as secrets |
| `auth` | none | `oauth2`, `login` or `aws_sigv4` credentials for every request; see below |
| `upsert` | an output needs it or `query` | The request that writes documents |
| `query` | none | The request that answers lookups; excludes `upsert`, `delete` and `operation` |
| `read` | required for an input | The request that reads a page of documents, or opens a stream |
| `delete` | none | The request that removes documents |
| `operation` | none | Template for a message's operation; without it every message is an upsert |
| `delete_values` | `delete`, `d` | Operation values that mean delete, ignoring case |
| `max_request_bytes` | 10 MiB | A larger batch is split into several requests |
| `compression` | `none` | `gzip`, `zstd` or `lz4` for request bodies, named in `Content-Encoding` |
| `request_timeout_ms` | see right | Timeout of one request; `0`: none. Unset: a request fails after 300 s without a response byte. With `read.stream` it is the longest silence on the open response, and unset means none |
| `connect_timeout_ms` | 10000 | Connection timeout |
| `tls` | none | `ca_file` and `accept_invalid_certs` for `https://`; `required: true` refuses a `url`, OAuth2 `token_url` or `login.url` that is not `https://` |

`upsert`:

| Field | Default | Meaning |
| --- | --- | --- |
| `path` | required | Path and query appended to `url` |
| `method` | `POST` | HTTP method |
| `format` | `ndjson` | `ndjson` (one document per line) or `json_array` |
| `content_type` | by format | `application/x-ndjson` or `application/json` |
| `action` | none | `ndjson` only: a line sent before each document, e.g. `{"index":{"_id":"${payload:id}"}}`. Values taken from the message are JSON-escaped. |
| `envelope` | none | `json_array` only: the body around the array; `{documents}` marks where the array goes |
| `document` | none | Template around each document; `{document}` marks where the payload goes, e.g. `{"doc":{document},"doc_as_upsert":true}`. Without `{document}` the template is the whole document and the payload need not be JSON, e.g. `{"data":"${message:payload \| base64}"}`. Values taken from the message are JSON-escaped. |
| `result` | HTTP status | See below |
| `split_on_reject` | `false` | Without `result`: a request the target refuses with 400, 409, 413 or 422 is sent again in halves until the refused documents stand alone |
| `max_documents` | none | Most documents in one request, for an API that limits the count rather than the size |

`query`:

| Field | Default | Meaning |
| --- | --- | --- |
| `path` | required | Path and query appended to `url` |
| `method` | `POST` | HTTP method |
| `format` | `ndjson` | `ndjson` (each lookup adds its lines) or `json_array` (each lookup adds one entry) |
| `content_type` | by format | `application/x-ndjson` or `application/json` |
| `request` | required | Template of one lookup's part of the body. Values taken from the message are JSON-escaped; `\| raw` inserts a JSON value as it is. With `ndjson` it may hold several lines. `{index}`, at most once, is the part's number in its request, from 1 |
| `envelope` | none | `json_array` only: the body around the array; `{requests}` marks where the array goes |
| `responses` | the response | JSON pointer to the array with one answer per lookup, in order |
| `value` | the entry | JSON pointer, inside an entry, to what the lookup answers |
| `found` | none | JSON pointer, inside an entry, to a boolean; anything but `true` answers `null` |
| `error` | none | JSON pointer, inside an entry, to an error text; set, it fails that lookup |
| `per_message` | `false` | One request per message; the response itself is the entry. Excludes `envelope` and `responses` |
| `parse_value` | `false` | The value is JSON text: parse it. Text that is not JSON fails its message for good |
| `concurrency` | 8 with `per_message`, else 1 | Requests in flight for one batch |
| `max_messages` | none | Most messages in one request; a larger batch is split, as it is at `max_request_bytes` |
| `responses_text` | none | JSON pointer to JSON text in the response that holds the answers; `responses` then points into that text. For an API that answers inside a string, such as a chat completion |
| `index` | none | JSON pointer, inside an entry, to the `{index}` of its message. Entries are then matched by that number instead of by position; a number that is missing, repeated or unknown makes the response unusable |
| `stop_reason` | none | JSON pointer to why the response ended, e.g. `/choices/0/finish_reason`; named in the error when the answers cannot be matched |
| `split_on_reject` | `false` | A request refused with 400, 409, 413 or 422, or a response whose answers cannot be matched: ask again in halves until the messages at fault stand alone, so only they fail. Without it the batch fails for good |

`max_messages`, `responses_text`, `index` and `split_on_reject` exclude `per_message`.

`auth` takes one of:

| Field | Meaning |
| --- | --- |
| `oauth2` | Client credentials grant: `token_url`, `client_id`, `client_secret` and an optional `scope`. The token is sent as `Authorization: Bearer`. Use an `https://` `token_url`: the secret is sent to it. With `tls.required` anything else is refused; without it, plain HTTP to another host logs a warning. |
| `login` | `url` and `body`, a JSON login request sent with POST; `token`, a JSON pointer to the token in the answer; `expires_at` (Unix seconds) or `expires_in` (seconds), optional JSON pointers to its expiry. The token is sent as `Authorization: Bearer`. `body` is stored as a secret; `url` is checked like the OAuth2 `token_url`. |
| `aws_sigv4` | Signs each request: `region` and `service` (`es`, `aoss`, …). `access_key`, `secret_key` and `session_token` are optional; without them the AWS default credential chain is used. Needs a build with the `aws` feature, which `full` includes. |

`read`:

| Field | Default | Meaning |
| --- | --- | --- |
| `path` | required | Path and query. `{cursor}` is replaced by the position, percent-encoded; `{limit}` by the batch size |
| `method` | `GET`, or `POST` with a `body` | HTTP method |
| `body` | none | JSON request body; `{cursor}` is replaced by the position as JSON (`7`, `"a"`, `null`) and `{limit}` by the batch size |
| `items` | the response | JSON pointer to the array of documents |
| `cursor.response` | none | JSON pointer to the next position in the response; `null` or an empty string there ends the read |
| `cursor.end_when_missing` | `false` | With `cursor.response`: a response without that field ends the read too. Without it such a response stops the route |
| `cursor.item` | none | JSON pointer to the position in the last document of a page |
| `cursor.add` | none | With `cursor.item`: added to that whole number, e.g. `1` for an API that takes the next offset |
| `cursor.next_url` | none | JSON pointer to the URL of the next page, e.g. `/links/next`; missing, `null` or empty ends the read |
| `cursor.link` | `false` | The next page is the `rel="next"` URL of the `Link` header; a page without one ends the read |
| `cursor.start` | `0` for a count, else `null` | Position before the first read |
| `cursor_id` | none | Names the saved read position |
| `checkpoint_store` | none | Where the position is saved: `file://`, `postgres://`, `mongodb://` or `s3://` |
| `polling_interval_ms` | 1000 | Wait after an empty page |
| `max_polling_interval_ms` | none | The wait doubles up to this while pages stay empty |
| `stream` | none | `sse` or `ndjson`: read one open response instead of pages; see [An open stream](#an-open-stream-sse-and-ndjson) |
| `payload` | the document | JSON pointer to the message payload in each document; a string is taken as text, anything else as JSON. Not with `stream` |
| `payload_encoding` | none | `base64`: decode the string at `payload` |

`{cursor}` must occur in the path or the body, except with `stream`, `cursor.next_url` or
`cursor.link`, which refuse it. Set at most one of `cursor.response`, `cursor.item`,
`cursor.next_url` and `cursor.link`.

`delete`:

| Field | Default | Meaning |
| --- | --- | --- |
| `path` | required | With `{ids}` the ids go there, comma-separated and percent-encoded; without it they are sent as a JSON array body. An id containing a comma cannot be sent in the URL form without a `line` that quotes it. |
| `method` | `POST` | HTTP method |
| `id_field` | `id` | Top-level payload field holding the id (a string or a number) |
| `max_ids` | 1000 | Most ids in one request |
| `envelope` | none | The body around the id array; `{ids}` marks where the array goes |
| `line` | none | Template per id; `{id}` is replaced by the id as JSON (`7` or `"a"`), and values such as `${payload:_rev}` are JSON-escaped. Alone it gives one body line per id; with `envelope` each line is one array entry, and one that is no JSON fails only its message; with `{ids}` each line, percent-encoded, is one list entry |
| `result` | HTTP status | See below |

`result` takes at most one of:

| Field | Meaning |
| --- | --- |
| `lines` | The response has one JSON line per document, in order. `success` is a JSON pointer to a boolean; `error` points to the reason. |
| `items` | The response is one JSON document with an array entry per document, in order. `path` is a JSON pointer to the array (empty when the response is the array); `error` points to an entry's error text, and an entry that has it failed. With `index`, a JSON pointer inside an entry to its document's position in the request (from 0), the array may list only the failures, e.g. BigQuery's `insertErrors`, and is left out or `null` when none failed; a position outside the request fails it. With `success: { path, equals }`, an entry whose value at `path` is not `equals` failed too, e.g. SES's `/Status` other than `SUCCESS` without an `Error`. |
| `job` | The response names a job. `id` points to it, `poll` is the path asked with GET (`{id}` is replaced), `status` points to its state. `succeeded` and `failed` list the final states; any other state is polled again. `error` points to the reason. `timeout_ms` (default `300000`) is how long the job may run. |

Without `result`, a 2xx status means every document was written.

## Behaviour

- **Order is kept.** A batch is cut into runs of upserts and deletes and sent in
  that order. After a request that failed but may be retried, nothing later in
  the batch is sent. With `operation` set, batches are also sent one at a time,
  whatever the route's `concurrency`, so a delete cannot overtake an upsert.
- **Failures are per document where the target says so.** A payload that is not
  a JSON object, a rejected result line and a delete without an id fail that
  message for good. A failed job or a refused request fails every document of
  that request for good, and the rest of the batch is still sent. With
  `upsert.split_on_reject: true` a request refused for its content (400, 409,
  413, 422) is sent again in halves, so only the refused documents fail. That
  costs about `2 x log2(batch)` extra requests per bad document. Turn it on only
  for a target that applies nothing of a request it refuses; one that applies a
  part before refusing would get that part twice.
- **Payloads are checked, not rewritten.** A payload that starts like a JSON
  object but is not valid JSON fails that message alone (`the payload is not
  valid JSON: …`), and the rest of the batch is sent.
- **Compression needs a target that reads it.** The body is compressed whole
  and named in `Content-Encoding`. `gzip` was run against Elasticsearch 8.19 and
  Meilisearch 1.53. Both refuse `zstd` and `lz4` with a 400 that calls the
  payload malformed and does not mention the encoding; check your target
  before choosing either.
- **Redirects are not followed**, so credentials in `headers` stay with the
  configured host.
- **Retries follow the status.** 408, 429 and 5xx (except 501 and 505) and
  connection errors are retryable; other statuses are not. Add the `retry`
  middleware to retry them and `dlq` to keep what failed for good.
- **A job has a deadline.** The endpoint polls until the job ends, logs a
  warning every minute and gives up after `timeout_ms` (5 minutes unless set).
  The documents of that request then fail as retryable, with the job id and
  the last state or poll error seen. A poll that keeps failing ends the same
  way. The job may still finish later, so retry only where sending a document
  twice is safe, as it is for an upsert by id.
- **Truncate is not applied.** A truncate message fails for good.
- **A token is fetched once and shared.** With `auth.oauth2` or `auth.login` the token is
  cached and replaced a minute before it expires (at half its lifetime if that
  is shorter); concurrent batches wait for one token request. A request
  answered 401 gets a new token and is sent once more; a second 401 fails as any
  other status. The client id and secret go in the form body of the token
  request. A failing token endpoint fails the batch by its status.
- **Credentials cover every request**: upserts, deletes, queries, reads and job
  polls. `client_secret`, `login.body`, `secret_key` and `session_token` are stored as
  secrets. A SigV4 signature covers the body as sent, after compression.

As a lookup (`query`):

- **One request per batch**, split by `max_request_bytes` like an upsert. A
  refused request fails every lookup in it, retryable by its status.
- **An entry whose `found` is not `true`, or that has nothing at `value`, is
  `null`.** The array must hold one entry per lookup: answers are matched by
  position, so a shorter or longer one fails the request for good.
- **An entry that reports an error fails only its message.** The other
  messages keep their answers; nothing is asked again.
- **A single lookup is a batch of one**: the same request with one part.
- **A `json_array` part must be valid JSON**; one that is not fails its message
  for good and is left out of the request.
- **With `per_message` every message is its own request**, for an API that
  takes one question at a time, such as a chat completion. A refused request
  fails only its message, retryable by its status. Under a `lookup` the
  lookup's `concurrency` decides how many are in flight.

As an input:

- **At least once.** The position moves when a batch is acknowledged. After a
  failed message the read continues behind the last acknowledged document; a
  page whose position comes from the response is read again as a whole.
- **Errors follow the status** as above. A response that is not JSON, or has no
  array at `items` or no position where `cursor` points, stops the route.
- **Documents are not reshaped.** The payload is the array entry as it came. Use
  a [`transform`](../cookbook/transform.md) to take CouchDB's `doc` out of a
  change, for example.
- **A draining route does not wait.** With `--drain` the first empty page ends
  the copy, also when that page names a next position.
- **A batch that is dropped without an answer is read again**, like a failed one.

Set the route's `batch_size` to a few thousand or more (`--batch-size` on the
command line, where the default is 1024): each batch is one request, and search
engines index large requests far faster than small ones. Against Meilisearch,
200,000 documents took 16 s at 1024 and 4 s at 50,000.

A target that is not listed needs three answers from its API documentation:
what the bulk write request looks like, how documents are removed by id, and
whether the response says anything per document. To read from it, it needs one
more: how a request names the page after the last one. If no combination of the
fields above fits, the endpoint cannot be used with it. An export that streams
everything in one response (Typesense) is not a paged listing; read it with
[`stream: ndjson`](#an-open-stream-sse-and-ndjson).

## A named endpoint of your own

`typesense`, `elasticsearch`, [`qdrant`](./qdrant.md) and the
[OpenAI-compatible endpoints](./openai.md) are this endpoint with the requests
filled in. From Rust each has a typed configuration, such as `QdrantConfig`,
that converts into an `Endpoint` with `.into()` and needs no registration. A
Rust crate can add one the same way: a function that turns a few fields into an
`http_bulk` configuration, registered under a name.

```rust
use mq_bridge::endpoints::http_bulk::{
    preset_base_url, preset_schema, preset_segment, register_preset_with,
};
use serde_json::{json, Value};

fn acme_schema() -> Value {
    let own = json!({
        "table": {"type": "string", "description": "Table written to.", "x-mqb-uri": "path"}
    });
    preset_schema("Acme", "acme://localhost:9000", own, &["url", "table"])
}

fn acme(_route: &str, config: &Value) -> anyhow::Result<Value> {
    let url = config["url"].as_str().unwrap_or_default();
    let table = preset_segment("table", config["table"].as_str().unwrap_or_default())?;
    Ok(json!({
        "url": preset_base_url("acme", url)?,
        "upsert": {"path": format!("/{table}/_bulk")}
    }))
}

register_preset_with("acme", acme_schema, acme)?;
```

Call it before any route starts and address the endpoint as
`acme: { url: ..., table: ... }`. The value `acme`
returns is checked as an `http_bulk` configuration, so every field under
[Fields](#fields) is available. `preset_schema` adds `url`, `api_key`,
`operation`, `compression` and `request_timeout_ms` to the fields you list;
`acme` decides what to do with them. A unit test registers the same endpoint;
it was not run against a server.

For a typed configuration of your own, derive `Serialize` on it and build the
endpoint with `named_endpoint("acme", &config)`.

This registers the endpoint in your own program. `mqb` only knows the endpoints
compiled into it or installed as plugins.
