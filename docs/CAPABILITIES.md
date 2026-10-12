# Endpoint capabilities

What each endpoint type can do, in one place. The fields and examples are in
[REFERENCE.md](REFERENCE.md). Message identity, replay position and which sinks are idempotent
are in [DELIVERY.md](DELIVERY.md).

The same table is available in code as `mq_bridge::endpoints::capabilities::capabilities()` and
per endpoint as `EndpointType::capabilities()`. The tables below are rendered from it, and tests
in `src/endpoints/capabilities.rs` compare it with the rules the engine applies.

## How to read the tables

- **yes / no**: holds for every configuration.
- **with `option`**: only with that option set. **unless `option`**: not with that option set.
- **depends**: decided by the configuration or by the endpoints it wraps; the notes say how.
- **–**: does not apply.

**Acknowledges.** The input redelivers a message that was read but not committed, so a crash
in the route does not lose it. An input marked "no" makes the route at-most-once; the route
logs that at start.

**Commits in order.** The input needs acknowledgements in the order the batches were read (an
offset or a cursor). With `concurrency > 1` the route then holds a finished batch back until
the ones before it are done. "no" means each message is acknowledged on its own.

**Replies.** The output answers a send with a response message. A
[`request`](REFERENCE.md#request) endpoint forwards that response, and a
[`lookup`](REFERENCE.md#lookup) middleware merges it into the message. A `request` whose `to`
is marked "no", or lacks the option named under "with", is refused when the route starts,
unless its `forward_to` is `null`. "depends" is not checked at start.

**Ordered publish.** The output writes batches one after another, in the order they were read,
whatever the route's `concurrency` is.

**TLS.** Where the encryption of a connection is configured. "`tls` block" is the `tls` field
of the endpoint; the notes say where a URL scheme or parameter does the same. Every input
ends when its source is empty if the route has `exit_on_empty`, so there is no column for it.

<!-- generated: begin -->

## Direction

| Endpoint | Input | Output |
|---|---|---|
| `amqp` | yes | yes |
| `aws` | yes | yes |
| `clickhouse` | yes | yes |
| `custom` | yes | yes |
| `dir_spool` | yes | yes |
| `elasticsearch` | no | yes |
| `fanout` | no | yes |
| `file` | yes | yes |
| `grpc` | yes | yes |
| `http` | yes | yes |
| `http_bulk` | yes | yes |
| `ibmmq` | yes | yes |
| `kafka` | yes | yes |
| `meilisearch` | yes | yes |
| `memory` | yes | yes |
| `mongodb` | yes | yes |
| `mqtt` | yes | yes |
| `nats` | yes | yes |
| `null` | no | yes |
| `object_store` | yes | yes |
| `openai_chat` | no | yes |
| `openai_embeddings` | no | yes |
| `postgres_cdc` | yes | no |
| `qdrant` | yes | yes |
| `reader` | no | yes |
| `redis_streams` | yes | yes |
| `ref` | yes | yes |
| `request` | no | yes |
| `response` | no | yes |
| `sequence` | yes | no |
| `sled` | yes | yes |
| `sqlx` | yes | yes |
| `static` | yes | yes |
| `stream_buffer` | yes | yes |
| `switch` | no | yes |
| `typesense` | no | yes |
| `websocket` | yes | yes |
| `zeromq` | yes | yes |

## Inputs

| Endpoint | Acknowledges | Commits in order | Notes |
|---|---|---|---|
| `amqp` | yes | no |  |
| `aws` | yes | no |  |
| `clickhouse` | yes | yes | A cursor source; needs `cursor_column`. |
| `custom` | depends | depends | Decided by the registered factory. |
| `dir_spool` | yes | no |  |
| `file` | yes | yes |  |
| `grpc` | depends | no | A server answers the caller, who resends on failure. A Bridge client acknowledges; a dynamic client (descriptor set or reflection) does not. |
| `http` | unless `fire_and_forget: true` | no | A listener. The caller gets the outcome and resends on failure. |
| `http_bulk` | yes | yes | A paged read of a search index. |
| `ibmmq` | yes | yes |  |
| `kafka` | yes | yes |  |
| `meilisearch` | yes | yes | A paged read of one index. |
| `memory` | yes | no | A nacked message is retried inside the consumer and is lost if the process dies. |
| `mongodb` | yes | depends | `delete` commits in any order; `snapshot`, `capture_new` and `capture_all` in order. |
| `mqtt` | unless `qos: 0` | no |  |
| `nats` | unless `no_jetstream: true` | no |  |
| `object_store` | yes | yes |  |
| `postgres_cdc` | yes | yes |  |
| `qdrant` | yes | yes | A scroll through one collection. |
| `redis_streams` | yes | no |  |
| `ref` | depends | depends | Follows the referenced endpoint. |
| `sequence` | depends | yes | Follows the endpoint of the current phase; commits always in order. |
| `sled` | yes | no |  |
| `sqlx` | yes | depends | A queue table commits in any order; a `cursor_column` or `publication` source in order. |
| `static` | yes | no | Emits the configured body. |
| `stream_buffer` | yes | yes |  |
| `websocket` | no | no | A listener. A reply goes back on the same connection; a rejected frame is not signalled and nothing is redelivered. |
| `zeromq` | no | no |  |

## Outputs

| Endpoint | Replies | Ordered publish | Notes |
|---|---|---|---|
| `amqp` | no | no |  |
| `aws` | no | no |  |
| `clickhouse` | with `lookup_query` | no | The reply is the first row; `clickhouse.found` says whether there was one. |
| `custom` | depends | depends | Decided by the registered factory. |
| `dir_spool` | no | yes |  |
| `elasticsearch` | no | with `operation` |  |
| `fanout` | depends | depends | Replies when one of its endpoints does; the first in list order wins. Ordered when one of them is. |
| `file` | no | yes |  |
| `grpc` | depends | no | The dynamic client returns the RPC's reply. |
| `http` | yes | no | The reply is the HTTP response, with the status in `http_status_code`. |
| `http_bulk` | depends | with `operation` | Replies with `query` set. |
| `ibmmq` | no | no |  |
| `kafka` | no | no |  |
| `meilisearch` | no | with `operation` |  |
| `memory` | with `request_reply: true` | no |  |
| `mongodb` | depends | no | Replies with `request_reply`, `report_outcome` or `find`. |
| `mqtt` | no | no |  |
| `nats` | with `request_reply: true` | no |  |
| `null` | no | no |  |
| `object_store` | no | no |  |
| `openai_chat` | yes | no | The reply is the model's answer. |
| `openai_embeddings` | yes | no | The reply is the vector. |
| `qdrant` | no | with `operation` |  |
| `reader` | yes | no | The reply is the message read from the wrapped input. |
| `redis_streams` | no | no |  |
| `ref` | depends | depends | Follows the referenced endpoint; a publisher registered in code is not checked. |
| `request` | depends | depends | Passes up what its `forward_to` returns. Ordered when `to` or `forward_to` is. |
| `response` | yes | no | The message itself, as the reply to the route's caller. |
| `sled` | no | no |  |
| `sqlx` | with `lookup_query` | no | The reply is the first row; `sqlx.found` says whether there was one. |
| `static` | yes | no | The reply is the configured body. |
| `stream_buffer` | no | no |  |
| `switch` | depends | depends | Replies when the chosen destination does. Ordered when one of them is. |
| `typesense` | no | with `operation` |  |
| `websocket` | no | no |  |
| `zeromq` | depends | no | Replies on a `req` socket. |

## Encryption and authentication

| Endpoint | TLS | Notes |
|---|---|---|
| `amqp` | `tls` block | User and password in the URL or as fields. `accept_invalid_certs` is not available. |
| `aws` | always (provider) | IAM credentials from the config or the environment. |
| `clickhouse` | `tls` block | User and password. |
| `custom` | – | Decided by the registered factory. |
| `dir_spool` | – |  |
| `elasticsearch` | `tls` block | API key, or `auth` for OAuth2 and AWS SigV4. |
| `fanout` | – |  |
| `file` | – | At-rest encryption with `encryption`. |
| `grpc` | `tls` block | A server checks client certificates with `tls.ca_file` and has no other authentication; reflection is always on. A client sends `api_key` or `bearer_token`. |
| `http` | `tls` block | A listener has `basic_auth` and checks client certificates with `tls.ca_file`. A client sends `basic_auth` or custom headers. |
| `http_bulk` | `tls` block | `auth` and custom headers. |
| `ibmmq` | `tls` block | Its own `tls` block (`cipher_spec`, key repository). User and password. |
| `kafka` | `tls` block | SASL user and password; they force `sasl_ssl`. |
| `meilisearch` | `tls` block | API key. |
| `memory` | – | In-process, or a local socket that checks the peer's user id. |
| `mongodb` | `tls` block | TLS and credentials can also be set in the URL (`tls=true`, `tlsCAFile=`). |
| `mqtt` | `tls` block | User and password. |
| `nats` | `tls` block | User and password, or a token. |
| `null` | – |  |
| `object_store` | always (provider) | Credentials as the provider's client reads them. |
| `openai_chat` | `tls` block | API key. |
| `openai_embeddings` | `tls` block | API key. |
| `postgres_cdc` | `tls` block | User and password in the URL. |
| `qdrant` | `tls` block | API key. |
| `reader` | – |  |
| `redis_streams` | `tls` block | TLS with a `rediss://` URL or `tls.required`. User and password. `accept_invalid_certs` is not available. |
| `ref` | – |  |
| `request` | – |  |
| `response` | – |  |
| `sequence` | – |  |
| `sled` | – |  |
| `sqlx` | `tls` block | TLS can also be set in the URL (`sslmode=`, `ssl-mode=`). On MySQL an `ssl-mode` in the URL wins over the `tls` block. |
| `static` | – |  |
| `stream_buffer` | – |  |
| `switch` | – |  |
| `typesense` | `tls` block | API key. |
| `websocket` | `tls` block | A listener has `basic_auth`, checks client certificates with `tls.ca_file` and limits connections with `max_connections`. A client sends `basic_auth`. |
| `zeromq` | none | No encryption and no authentication. Use it on a trusted network only. |

<!-- generated: end -->
