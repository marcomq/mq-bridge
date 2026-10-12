# Connectors

One page per connector: purpose, practical examples, and the same settings in both forms —
as a `mqb copy` **URL** and as a **YAML** endpoint for config files and the library. The
nested *Parameters* page under each connector lists every field (type, default, description),
generated from the same schema the engine uses.

## From URL to config

A URL's query parameters are config fields of the same name. The scheme picks the config
key, and the part before the query string becomes `url` (or `path`):

| URL scheme | Config key | `url` / `path` in config |
| :--- | :--- | :--- |
| `kafka://host:9092` | `kafka` | `url: "host:9092"` (bare host list) |
| `nats://host:4222` | `nats` | `url: "nats://host:4222"` |
| `amqp://`, `rabbitmq://` | `amqp` | `url: "amqp://host:5672/%2f"` |
| `mqtt://`, `mqtts://` | `mqtt` | `url: "mqtt://host:1883"` |
| `redis://`, `rediss://` | `redis_streams` | `url: "redis://host:6379"` |
| `postgres://`, `mysql://`, `sqlite://` | `sqlx` | the connection string as `url` |
| `postgres-cdc://`, `pgcdc://` | `postgres_cdc` | `url: "postgres://…"` |
| `mongodb://` | `mongodb` | `url: "mongodb://host:27017"` |
| `clickhouse://`, `clickhouses://` | `clickhouse` | `url: "http://host:8123"` / `https://` |
| `http-bulk:?config_file=<path>` | `http_bulk` | nested config from a file, or inline with `config=<YAML or JSON>` |
| `typesense://host:8108/<collection>`, `elasticsearch://host:9200/<index>`, `qdrant://host:6333/<collection>` | the endpoint of that name | `+https` in the scheme for HTTPS; see [Typesense](./typesense.md), [Elasticsearch](./elasticsearch.md), [Qdrant](./qdrant.md) |
| `openai://host?model=…&prompt=…`, `openai-embeddings://host?model=…&input=…` | `openai_chat` / `openai_embeddings` | `+https` in the scheme for HTTPS; see [OpenAI-compatible APIs](./openai.md) |
| `http://`, `https://` | `http` | unchanged |
| `ws://`, `wss://` | `websocket` | unchanged |
| `grpc://`, `grpcs://` | `grpc` | `url: "http://host:50051"` / `https://` |
| `zeromq://`, `zmq://` | `zeromq` | `url: "tcp://host:5555"` |
| `ibmmq://host:1414` | `ibmmq` | `url: "host(1414)"` |
| `aws://` | `aws` | no `url`; `queue_url` / `topic_arn` |
| `file:///path` | `file` | `path: "/path"` |
| `dir-spool:///path` | `dir_spool` | `path: "/path"` |
| `s3://`, `gs://`, `az://`, `local-store://` | `object_store` | `url: "s3://bucket/prefix"`, `"gs://bucket/prefix"`, `"az://container/prefix"` (`local-store://` → `file://`); credentials come from the [environment](./object-store.md#credentials) |

So `kafka://localhost:9092?topic=orders&group_id=sync` becomes:

```yaml
kafka: { url: "localhost:9092", topic: "orders", group_id: "sync" }
```

Transport behaviour shared by all connectors — consumer vs. subscriber, nack support, CDC vs.
polling, file formats, plugins — is on [Overview & capabilities](../reference/endpoints.md).

## All connectors

- [PostgreSQL / MySQL / MariaDB / SQLite](./postgres.md) (including [PostgreSQL CDC](./postgres.md#postgresql-cdc))
- [ClickHouse](./clickhouse.md)
- [MQTT](./mqtt.md)
- [Kafka](./kafka.md)
- [RabbitMQ (AMQP)](./rabbitmq.md)
- [NATS](./nats.md)
- [Redis Streams](./redis.md)
- [HTTP](./http.md)
- [WebSocket](./websocket.md)
- [gRPC](./grpc.md)
- [MongoDB](./mongodb.md)
- [AWS SQS / SNS](./aws.md)
- [ZeroMQ](./zeromq.md)
- [IBM MQ](./ibmmq.md)
- [File (CSV / JSON / JSONL)](./file.md)
- [Directory spool](./dir-spool.md)
- [Object storage: S3, MinIO / R2, Google Cloud Storage, Azure Blob, local directory](./object-store.md)
- [Apache Pulsar](./pulsar.md)
- [Meilisearch](./meilisearch.md)
- [HTTP bulk (search engines)](./http-bulk.md)
- [Typesense](./typesense.md)
- [Elasticsearch](./elasticsearch.md)
- [PostgREST and Supabase](./postgrest.md)
- [Qdrant](./qdrant.md)
- [OpenAI-compatible APIs (LLMs)](./openai.md)
- [Connect plugin (Redpanda Connect components)](./connect.md)
