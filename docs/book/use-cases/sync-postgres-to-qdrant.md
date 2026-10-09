<!-- description: Keep a Qdrant vector index in sync with a Postgres table: logical replication (CDC) feeds an embeddings API and upserts or deletes points. Runnable docker-compose example. -->

# How to keep a vector index in sync with Postgres (CDC → embeddings → Qdrant)

One mq-bridge route reads a Postgres table through logical replication, calls an embeddings API
for each inserted or updated row and upserts the result into Qdrant; a deleted row removes its
point. It copies the existing rows first and needs no Kafka in between.

Runnable example: [`examples/postgres-cdc-to-qdrant`](https://github.com/marcomq/mq-bridge/tree/main/examples/postgres-cdc-to-qdrant).

## The problem: a RAG index that drifts from the database

Retrieval-augmented generation (RAG) answers from whatever is in the vector index. When the source
rows live in Postgres, the index has to follow them, and the usual approaches each miss something:

- **A nightly re-embedding job** leaves the index up to a day stale and pays for embedding rows
  that did not change.
- **Dual writes in the application** (write the row, then call the embeddings API and the vector
  database) lose updates when the second call fails after the first committed, and miss every
  change made by a migration, an admin console or another service.
- **Polling an `updated_at` column** never sees deletes, so removed documents keep showing up in
  search results.

Change data capture reads the write-ahead log, so it sees every committed insert, update and
delete regardless of who made it.

## The solution: one route from the WAL to the index

```mermaid
flowchart LR
  PG[(Postgres<br/>docs table)] -- logical replication --> B[mq-bridge route]
  B -- "insert / update: row text<br/>POST /v1/embeddings" --> E[Embeddings API]
  E -- "vector" --> B
  B -- "upsert point with vector" --> Q[(Qdrant)]
  B -- "delete:<br/>delete point" --> Q
```

Postgres needs `wal_level = logical` and a publication on the table:

```sql
CREATE PUBLICATION docs_pub FOR TABLE docs WITH (publish = 'insert, update, delete');
```

The route, as it runs in the example (`routes.yaml`):

```yaml
docs_to_qdrant:
  input:
    postgres_cdc:
      url: "postgres://app:app@postgres:5432/app"
      publication: "docs_pub"
      slot_name: "docs_to_qdrant"
      consume: capture_all                             # copy existing rows, then follow changes
      checkpoint_store: "file:///app/checkpoint.json"  # a restart does not repeat the copy
  output:
    switch:
      metadata_key: "postgres.operation"
      cases:
        # Deletes carry only the primary key: remove the point, call no model.
        delete:
          http_bulk: &qdrant
            url: "http://qdrant:6333"
            operation: "${metadata:postgres.operation}"
            upsert:
              method: PUT
              path: /collections/docs/points?wait=true
              format: json_array
              envelope: '{"points": {documents}}'
            delete:
              path: /collections/docs/points/delete?wait=true
              envelope: '{"points": {ids}}'
      # Inserts and updates. Output middlewares run bottom-up.
      default:
        middlewares:
          # 3. Shape the Qdrant point: {id, vector, payload}.
          - transform:
              mapping:
                id: "$.id"
                vector: "$.embedding.data[0].embedding"
                "payload.title": "$.title"
                "payload.body": "$.body"
          # 2. Call the embeddings API; the response lands in `embedding`.
          - lookup:
              from:
                http: { url: "http://ollama:11434" }
              metadata:
                http_path: "/v1/embeddings"
                http_method: POST
              into: embedding
          # 1. Add the fields the embeddings API expects: `model` and `input`.
          - transform:
              mapping:
                id: "$.id"
                title: "$.title"
                body: "$.body"
                input: "$.body"
                model: { path: "$.model", default: "all-minilm" }
          - retry: { max_attempts: 5 }
        http_bulk: *qdrant
```

How it fits together:

- [`postgres_cdc`](../connectors/postgres.md#postgresql-cdc) emits one message per changed row.
  The payload is the row as flat JSON; the operation is in the `postgres.operation` metadata key.
- [`switch`](../cookbook/switch.md) sends deletes straight to Qdrant. Everything else goes through
  the embedding steps first.
- [`lookup`](../cookbook/lookup.md) posts the message to the embeddings API and stores the
  response under `embedding`. The request and response shape is the one of the OpenAI embeddings
  API, which Ollama also serves, so a hosted provider needs only a different `url`, a model name
  and an authorization header (`custom_headers` on the [`http`](../reference/http.md) endpoint).
- [`http_bulk`](../connectors/http-bulk.md#qdrant) batches points into Qdrant's upsert and delete
  requests. The row's primary key is the point id, so an update overwrites the point of that row.

The collection must exist with the vector size of the model (384 for `all-minilm`); the example's
compose file creates it.

### Run the route from Rust

The same file runs embedded in a service. Enable the features the route uses:

```toml
[dependencies]
mq-bridge = { version = "0.4.18", features = ["postgres-cdc", "http-bulk", "http", "yaml"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mq_bridge::deploy_file("routes.yaml").await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

### Run the route from Python

The PyPI package includes every connector:

```python
from mq_bridge import Route

Route.from_file("routes.yaml", "docs_to_qdrant").run()
```

### Run it without code

The example uses the `mq-bridge-app` container: `mq-bridge-app --config routes.yaml`.

## Runnable example

```bash
git clone https://github.com/marcomq/mq-bridge && cd mq-bridge/examples/postgres-cdc-to-qdrant
docker compose up -d
./search.sh "when do I get my money back"
```

The stack is Postgres 16, Qdrant 1.19.0, Ollama 0.12.3 with a local 384-dimension model, and
mq-bridge-app 0.4.18. `./test.sh` checks the backfill, an insert, an update, a delete, a restart
and a semantic search; it runs in CI.

## Failure semantics

| Event | What happens |
| :--- | :--- |
| **mq-bridge restarts** | It resumes from the replication slot and the checkpoint file. Changes made while it was down are applied after the restart. Without `checkpoint_store`, `capture_all` copies the whole table again, which re-embeds every row. Keep the checkpoint file on a volume. |
| **Duplicates** | Delivery is [at-least-once](../engine/delivery.md): a change can be delivered again after a crash or restart. A repeated upsert writes the same point id again and a repeated delete removes nothing, so the index converges. The cost of a replay is the repeated embedding calls. |
| **Deletes** | A delete event carries only the key columns. The table needs a primary key (or a replica identity), and the point id must be that key. |
| **`TRUNCATE`** | Not synchronized. A truncate event names no rows, so the route has nothing to delete by; the publication leaves it out (`publish = 'insert, update, delete'`). After truncating the table, delete the points or recreate the collection yourself. |
| **Qdrant unavailable** | `http_bulk` treats 408, 429 and most 5xx responses as retryable and `retry` repeats the request. A message is acknowledged to Postgres only after the output accepted it, so unconfirmed changes stay in the replication slot. |
| **Embeddings API unavailable or rejecting** | A temporary error is retried. A permanent error (an HTTP 4xx, a non-JSON row) is logged and drops only that message; add a [`dlq`](../cookbook/dlq.md) after the `lookup` to keep such rows. |
| **mq-bridge stays down** | Postgres keeps the WAL the slot has not confirmed. Monitor the slot's lag and drop the slot if you retire the route. |

Limits of this recipe:

- **One vector per row.** There is no chunking step. To split long documents, do it in a
  [handler](../tutorials/embedding.md) and emit one point per chunk, or store chunks as rows.
- **Every update re-embeds the row**, including updates that did not change the embedded column.
- **Large unchanged columns are omitted from update events.** Postgres does not write an unchanged
  TOASTed value (roughly: text over 2 kB) to the WAL, and mq-bridge leaves that field out of the
  payload. An update that changes only `title` on a row with a long `body` then arrives without
  `body`. The example's rows are short and do not hit this; the recipe has not been tested with
  TOASTed columns.
- **Point ids** are the unsigned integer primary key here. Qdrant accepts
  [unsigned integers and UUIDs](https://qdrant.tech/documentation/concepts/points/#point-ids) as point ids, so a table with another key type needs a `transform` that derives one.
- **The request to the embeddings API is the message payload.** Build it with a `transform` as
  above. The `lookup` middleware's `payload` template does not JSON-escape values, so a row
  containing a double quote breaks a templated request.

## When not to use mq-bridge for this

- **The vectors can live in Postgres.** With [pgvector](https://github.com/pgvector/pgvector) the
  embedding is a column next to the row, and a trigger or the application can keep it current in
  the same transaction. There is no second system to sync. (Writing to a `vector` column through
  mq-bridge's SQL output has not been tested.)
- **You need chunking, re-ranking or multi-step document processing.** That is application logic;
  a framework or your own worker that consumes the CDC stream fits better than a config file.
- **The source is not Postgres or MongoDB.** mq-bridge has native CDC for those two only. For
  MySQL, SQL Server or Oracle see [CDC via Debezium](../cookbook/debezium.md).
- **You already run Kafka Connect with Debezium.** Adding a sink to that platform is less to
  operate than a second CDC reader on the same database.

## Alternatives

| | mq-bridge | Debezium | Redpanda Connect |
| :--- | :--- | :--- | :--- |
| Postgres CDC | `postgres_cdc` endpoint, built in | Postgres connector | [`postgres_cdc` input](https://docs.redpanda.com/redpanda-connect/components/inputs/postgres_cdc/); its page states "This component requires an enterprise license" |
| Embedding step | `lookup` middleware calling an HTTP embeddings API | [Embeddings transformation](https://debezium.io/documentation/reference/stable/ai/embeddings.html) (Hugging Face, Ollama, ONNX MiniLM, OpenAI, Voyage AI) | Embedding processors for several providers |
| Writing to Qdrant | `http_bulk` endpoint with a Qdrant request shape | Not part of Debezium's documented sinks; a separate consumer or sink connector | `qdrant` output |
| How it runs | Library in a Rust, Python or Node.js process, or one binary | ["Most commonly"](https://debezium.io/documentation/reference/stable/architecture.html) on Kafka Connect; also Debezium Server or an embedded Java engine | One binary, YAML config |
| Sources with CDC | Postgres, MongoDB | Postgres, MySQL, SQL Server, Oracle, Db2, MongoDB and more | See its component list |

### Why mq-bridge can be the better fit here

- **One process covers the whole pipeline.** Capture, the embeddings call and the Qdrant write are
  one route in one file. With Debezium the capture and the embedding step run in Kafka Connect or
  Debezium Server, and writing to Qdrant is a further component.
- **No JVM and no Kafka to operate.** mq-bridge is a native binary (the 0.4.18 macOS arm64 build of
  `mq-bridge-app` is about 100 MB with every connector) or a library inside a Rust, Python or
  Node.js service you already run.
- **Postgres CDC is part of the open-source core.** mq-bridge is licensed
  [MIT OR Apache-2.0](https://github.com/marcomq/mq-bridge/blob/main/LICENSE), CDC included.
  Redpanda Connect's `postgres_cdc` input states that it requires an enterprise license (linked
  above).
- **Backfill and streaming are one setting.** `consume: capture_all` copies the existing rows and
  then follows the WAL, so a new index needs no separate initial load.
- **The route is testable without infrastructure.** The same route and handler code runs against
  in-memory endpoints; see [Test without a broker](test-without-a-broker.md).

Where the alternatives are the better fit: Debezium covers more databases, and Redpanda Connect
has ready-made embedding processors and a `qdrant` output, so neither needs the request shaping
shown above.

No throughput comparison against these tools has been measured, so none is claimed. mq-bridge's own
numbers are on the
[benchmark dashboard](https://marcomq.github.io/mq-bridge/dev/bench/) and in the
[tuning page](../operations/tuning.md).
