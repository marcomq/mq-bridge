<!-- description: Problem-first guides for mq-bridge: keep a vector or search index in sync with Postgres or MongoDB, classify and route messages with an LLM, enrich events and keep per-key history, implement the transactional outbox, test message handlers without a broker. Each has a runnable example. -->

# Use cases

Each page starts from a problem, shows one working solution with mq-bridge, and says where it
stops working. Every page has a runnable example in the repository's
[`examples/`](https://github.com/marcomq/mq-bridge/tree/main/examples) directory that CI executes.

Every page has the same sections: the problem, the solution with a diagram and configuration, a
runnable example, failure semantics (restarts, duplicates, deletes, an unavailable sink), when not
to use mq-bridge, and a comparison with alternatives.

## Keep an index in sync with a database

**[RAG index sync: Postgres CDC → embeddings → Qdrant](sync-postgres-to-qdrant.md).** A
retrieval-augmented generation (RAG) or semantic-search index drifts from the Postgres rows it
was built from. One route reads the table through logical replication, embeds each changed row
with an OpenAI-compatible API (a local Ollama in the example) and upserts or deletes the point in
Qdrant. Existing rows are copied first.
Example: [`postgres-cdc-to-qdrant`](https://github.com/marcomq/mq-bridge/tree/main/examples/postgres-cdc-to-qdrant).

**[Search index sync: MongoDB → Meilisearch](sync-mongodb-to-meilisearch.md).** Search shows
prices and products the database no longer has. One route copies a MongoDB collection and
follows its change stream into a Meilisearch index, deletes included; the same input feeds
Typesense or Elasticsearch.
Example: [`mongodb-to-meilisearch`](https://github.com/marcomq/mq-bridge/tree/main/examples/mongodb-to-meilisearch).

## Add intelligence and context to a stream

**[LLM triage: classify and route messages](llm-ticket-triage.md).** Free-text messages
(tickets, emails, reviews) have no field to route by. A model names the category, and a `switch`
routes on the answer, with any answer outside the known categories sent to people.
Example: [`llm-ticket-triage`](https://github.com/marcomq/mq-bridge/tree/main/examples/llm-ticket-triage).

**[Enrich events and keep per-key history](enrich-and-aggregate-orders.md).** An event needs
a database record and the history of its key to be acted on. A `lookup` adds the record, an
`aggregate` with a Postgres store adds counts, sums and moving averages per key that survive
restarts and are shared by instances, and outliers go to a review table.
Example: [`order-features`](https://github.com/marcomq/mq-bridge/tree/main/examples/order-features).

## Reliable messaging in services

**[Transactional outbox with Postgres and Kafka](transactional-outbox-postgres-kafka.md).** A
service must update its database and publish an event, and the two writes can diverge. The
service writes the event into an outbox table in the same transaction; a route relays it from
the WAL to Kafka.
Example: [`transactional-outbox`](https://github.com/marcomq/mq-bridge/tree/main/examples/transactional-outbox).

**[Test message handlers without a broker](test-without-a-broker.md).** Tests for Kafka, NATS
or RabbitMQ consumers need a running broker or mock the client. The same handler runs against
an in-memory endpoint in tests, in Rust and Python.
Example: [`test-without-a-broker`](https://github.com/marcomq/mq-bridge/tree/main/examples/test-without-a-broker).

## More recipes

Shorter recipes elsewhere in this book: [LLM calls in a pipeline](../cookbook/llm.md),
[running aggregates](../cookbook/aggregate.md), [enrichment and lookups](../cookbook/lookup.md),
[query a stream in DuckDB](../cookbook/duckdb.md),
[Postgres CDC to a file](../tutorials/postgres-cdc.md),
[cross-process IPC](../tutorials/ipc-bridge.md).
