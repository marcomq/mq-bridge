# HTTP

Two different things under one name:

- As an **output** it is an HTTP client: every message is sent as one request to a URL.
- As an **input** it is an HTTP server: it listens on an address, and every request it
  receives is one message. This is how you take in webhooks or put an HTTP API in front of
  a queue.

To write many documents per request to a search engine or a REST API, see
[HTTP bulk](./http-bulk.md). For a response that stays open (server-sent events), the same
page has [An open stream](./http-bulk.md#an-open-stream-sse-and-ndjson).

## URL format

```text
http://host[:port]/path        output: the target URL
http://address:port            input:  the address to listen on
```

| What you want | URL |
| :--- | :--- |
| POST each message to an API | `https://api.example.com/v1/orders` |
| PUT instead of POST | `https://api.example.com/v1/orders?method=PUT` |
| Send an API key header | `https://api.example.com/v1/orders?custom_headers={"X-API-Key":"abc123"}` |
| Basic authentication | `https://api.example.com/v1/orders?basic_auth=["user","secret"]` |
| Give up on a request after 5 s | `https://api.example.com/v1/orders?request_timeout_ms=5000` |
| Listen on all interfaces, port 8080 | `http://0.0.0.0:8080` |
| Accept only `POST /hooks/github` | `http://0.0.0.0:8080?path=/hooks/github&method=POST` |
| Answer `202` at once, process afterwards | `http://0.0.0.0:8080?fire_and_forget=true` |

Rules that the table follows:

- **An output keeps its path and its own query parameters.** In
  `https://api.example.com/search?api-version=2` the parameter `api-version` is not a field of
  this connector, so it stays in the request URL. A parameter that *is* a field, such as
  `method` or `path`, is taken as configuration and removed.
- **An input has no path in the URL.** `http://0.0.0.0:8080/hook` is refused with
  `Invalid listen address`; write `?path=/hook`. Without `path` every path is accepted.
- **`custom_headers`, `basic_auth` and `tls` take JSON.** Put the URL in single quotes.

## Config (YAML / library)

Every URL parameter is a field of the same name under `http:`. For an input `url` is the
listen address, without a scheme; for an output it is the target URL.

### Calling an API (output)

Send every row of a table to an API, with a header, retries, and a file for what the API
keeps refusing:

```yaml
orders_to_api:
  input:
    sqlx:
      url: "postgres://app:secret@localhost/shop"
      table: "orders"
      cursor_column: "id"
      cursor_id: "orders-to-api"
  output:
    middlewares:
      - retry: { max_attempts: 5, initial_interval_ms: 500 }
      - dlq: { endpoint: { file: { path: "refused-orders.jsonl" } } }
    http:
      url: "https://api.example.com/v1/orders"
      method: "POST"
      custom_headers:
        Authorization: "Bearer ${API_TOKEN}"
        Content-Type: "application/json"
      request_timeout_ms: 10000
      batch_concurrency: 8
```

- **The message payload is the request body**, sent as it is. Metadata entries are sent as
  request headers; `custom_headers` are added to every request.
- **A message from an HTTP input keeps its method and path.** If the message has `http_method`,
  `http_path` or `http_query` in its metadata, as every message of an HTTP input does, they
  replace `method` and the path of `url`. See [Answering a request](#answering-a-request-request--reply).
- **A status outside 2xx fails the message.** With `pass_through_status: true` every status
  counts as an answer instead, and the status is in the metadata of the response for a
  [`switch`](../cookbook/switch.md) to act on.
- **Requests of one batch run in parallel**, `batch_concurrency` at a time (default 20). Lower
  it for an API with a rate limit, or add a [`limiter`](../engine/reference.md) middleware.
- **`compression: gzip`** compresses request bodies above `compression_threshold_bytes`
  (default 1024).

### Receiving webhooks (input)

Take in webhooks and put them on a Kafka topic:

```yaml
github_webhooks:
  input:
    http:
      url: "0.0.0.0:8080"
      path: "/hooks/github"
      method: "POST"
      max_body_bytes: 1048576
  output:
    kafka: { url: "localhost:9092", topic: "github-events" }
```

```sh
curl -X POST http://localhost:8080/hooks/github \
  -H 'X-GitHub-Event: push' -d '{"ref":"refs/heads/main"}'
# 202 Message processed
```

- **The caller gets its answer when the output has accepted the message**: `202` after the
  Kafka broker confirmed it. If the output fails, the caller gets an error and can send again.
  With `fire_and_forget: true` the answer is `202` at once, and a failure after that is only
  in the log.
- **The request body is the payload; headers are metadata.** The listener adds `http_method`,
  `http_path`, `http_query` and `http_version`. In the example above the message has
  `x-github-event: push` and `http_path: /hooks/github` in its metadata.
- **Other requests are refused**: `404` for another path, `405` for another method, `401`
  without the credentials of `basic_auth`, `413` above `max_body_bytes` (default 256 MiB).
- **Several routes can listen on one port** when each has its own `path`.

Protect a listener with a user and password, and serve it over TLS:

```yaml
input:
  http:
    url: "0.0.0.0:8443"
    basic_auth: ["webhook", "${WEBHOOK_PASSWORD}"]
    tls:
      required: true
      cert_file: "/etc/ssl/server.pem"
      key_file: "/etc/ssl/server.key"
```

A certificate without `required: true` is not used; the listener warns at start.

### Answering a request (request / reply)

With the `response` output the caller gets the message back as the HTTP response, after the
middlewares of the route have changed it. This route answers every `POST /echo` with the body
it was sent:

```yaml
echo:
  input:
    http: { url: "0.0.0.0:8080", path: "/echo" }
  output:
    response: {}
```

An HTTP output in the middle makes the route a proxy: the request goes to another service,
and that service's response goes back to the caller.

```yaml
proxy:
  input:
    http: { url: "0.0.0.0:8080" }
  output:
    request:
      to:
        http: { url: "http://backend.internal:9000", pass_through_status: true }
      forward_to:
        response: {}
```

`GET /users/7?full=1` on port 8080 becomes `GET http://backend.internal:9000/users/7?full=1`:
a message that came in over HTTP keeps its method, path and query string on the way out, and
the output's `url` supplies only the scheme, host and port.

More in the [Request / reply tutorial](../tutorials/request-reply.md).

## Examples

**Consume a RabbitMQ queue and POST each message to an API, continuous:**

```bash
mqb copy \
  --from 'rabbitmq://guest:guest@localhost:5672/%2f?queue=orders' \
  --to 'http://internal-api.local/ingest?method=POST'
```

**Run an HTTP listener as the source (webhook receiver), continuous:**

```bash
mqb copy \
  --from 'http://0.0.0.0:8080?path=/hooks&method=POST' \
  --to 'kafka://kafka.local:9092?topic=webhooks'
```

**Send a file line by line, with a 5-second timeout per request:**

```bash
mqb copy --drain \
  --from 'file:///data/events.jsonl?format=raw' \
  --to 'https://api.example.com/ingest?method=POST&request_timeout_ms=5000'
```

## Key options

| Option | Purpose |
|---|---|
| `method` | HTTP method. Publisher: request method (defaults to POST). Consumer: restrict to this method. |
| `path` | Consumer-only: accept only this request path. |
| `custom_headers` | Headers added to every request (publisher) or response (consumer). |
| `basic_auth` | `[user, password]`. Publisher: sent with each request. Consumer: required from each caller. |
| `request_timeout_ms` | Per-request timeout. Defaults to 30000ms. |
| `batch_concurrency` | Publisher-only: requests in flight for one batch. Defaults to 20. |
| `pass_through_status` | Publisher-only: treat a status outside 2xx as an answer, not as a failure. |
| `workers` | Consumer-only: worker thread count. Defaults to unlimited. |
| `fire_and_forget` | Consumer-only: respond 202 immediately, don't wait for downstream processing. |
| `max_body_bytes` | Consumer-only: largest request body. Defaults to 256 MiB. |
| `message_id_header` | Header to extract the message ID from. Defaults to `message-id`. |

**Metadata and headers.** A consumer stores each request header as metadata; a publisher
sends each metadata entry as a request header, and a consumer sends the metadata of a reply
as response headers. Not passed on: `host`, `connection`, `keep-alive`, `proxy-connection`,
`upgrade` (request side), the framing headers `content-length`, `transfer-encoding`,
`content-encoding`, and anything that is no valid header (a space in the name, a line break
in the value). To send a fixed `Host`, use `custom_headers`. A publisher decodes a gzip,
lz4 or zstd response and removes its `content-encoding`.

Full field list: [reference/http.md](../reference/http.md).
