# mq-bridge Project Context

> **Looking for what middleware or structural endpoints exist, and how to configure them?**
> [REFERENCE.md](docs/REFERENCE.md) is the complete, authoritative list — every middleware
> (`retry`, `dlq`, `transform`, `id`, `filter`, `deduplication`, `weak_join`, `buffer`, `limiter`, `delay`,
> `cookie_jar`, `encryption`, `compression`, `pack`, `unpack`, `metrics`, `random_panic`, `custom`) and every
> structural endpoint (`ref`,
> `fanout`, `switch`, `request`, `response`, `reader`, `static`, `stream_buffer`, `null`,
> `custom`), each with its fields, defaults, and a working YAML example. Do not infer these
> from the enum definitions in `models.rs`; the reference records the behaviour and the
> spelling traps too. Every snippet in it is parsed by `tests/reference_docs_test.rs`.

## Project Overview

`mq-bridge` is an asynchronous message bridging library for Rust that connects different messaging systems, data stores, and protocols. It acts as a **programmable integration layer**, allowing for transformation, filtering, handling, events, and complex routing.

## Core Architecture

### Key Concepts

1. **Route**: A named data pipeline that defines a flow from one `input` endpoint to one `output` endpoint
2. **Endpoint**: A source (consumer) or sink (publisher) for messages - supports Kafka, NATS, AMQP, MQTT, MongoDB, ZeroMQ, HTTP, Files, and in-memory channels
3. **Top-Level API**: A set of functions in the `mq_bridge` root for managing routes and publishers (e.g., `get_route`, `stop_route`, `get_publisher`).
4. **Middleware**: Components that intercept and process messages (retries, DLQ, deduplication, metrics)
5. **Handler**: Programmatic components for business logic (CommandHandler for 1-to-1 transformations, EventHandler for terminal consumption)
6. **CanonicalMessage**: The unified message format used throughout the system
7. **Request-Reply**: Native support for synchronous request-reply pattern on NATS, MongoDB, and Memory endpoints

### Design Principles

- **Protocol Abstraction**: Write business logic against `CanonicalMessage`, swap transports via configuration
- **Unopinionated**: Doesn't enforce specific architectural patterns (CQRS/ES), focuses on reliable data movement
- **Async-First**: Built on Tokio for efficient async I/O
- **Feature Flags**: Optional dependencies via Cargo features (kafka, amqp, nats, mqtt, mongodb, zeromq, http, metrics, dedup)

### File Structure

```
src/
├── lib.rs                 # Main library entry point
├── canonical_message.rs   # Unified message format
├── traits.rs             # Core traits (MessageConsumer, MessagePublisher, Handler)
├── models.rs             # Configuration models (Route, Endpoint, Middleware)
├── route.rs              # Route execution logic (sequential/concurrent)
├── endpoints/            # Endpoint implementations
│   ├── mod.rs           # Factory functions for creating consumers/publishers
│   ├── amqp.rs          # AMQP (RabbitMQ) consumer/publisher
│   ├── aws.rs           # AWS SQS/SNS
│   ├── clickhouse.rs    # ClickHouse sink + cursor source
│   ├── dir_spool/       # Crash-safe directory FIFO queue (payload file + JSON sidecar)
│   ├── file/            # File-based endpoints
│   ├── grpc.rs          # gRPC consumer/publisher
│   ├── http/            # HTTP consumer/publisher (+ streaming)
│   ├── ibm_mq.rs        # IBM MQ (client loaded at runtime via dlopen)
│   ├── kafka.rs         # Kafka consumer/publisher
│   ├── memory/          # In-memory channels + IPC transports
│   ├── mongodb.rs       # MongoDB consumer/publisher/change streams
│   ├── mqtt.rs          # MQTT consumer/publisher
│   ├── nats.rs          # NATS consumer/publisher
│   ├── object_store.rs  # Cloud object storage (S3 / GCS / Azure)
│   ├── poll.rs          # Shared cursor-polling helper (sqlx, clickhouse)
│   ├── postgres/        # Postgres CDC (logical replication + pgoutput)
│   ├── redis_streams.rs # Redis Streams
│   ├── sled.rs          # Embedded sled queue
│   ├── sqlx/            # PostgreSQL / MySQL / SQLite
│   ├── websocket.rs     # WebSocket consumer/publisher
│   ├── zeromq/          # ZeroMQ consumer/publisher backends and codecs
│   └── structural/      # Structural endpoints (no external system)
│       ├── fanout.rs          # Broadcast to every listed endpoint
│       ├── switch.rs          # Content-based routing
│       ├── request.rs         # Request/reply call, forward the response
│       ├── response.rs        # Reply to the origin of the current request
│       ├── reader.rs          # Trigger a pull from a consumer
│       ├── static_endpoint.rs # Fixed, pre-rendered message
│       ├── stream_buffer.rs   # Correlation-partitioned in-memory stream
│       └── null.rs            # Null endpoint (sink)
├── middleware/           # Middleware implementations
│   ├── buffer.rs        # Batch accumulation
│   ├── cookie_jar.rs    # Cookie / metadata persistence across requests
│   ├── deduplication.rs # Message deduplication (sled)
│   ├── delay.rs         # Artificial delay
│   ├── deferred_commit.rs # Hold commits for batches a middleware emptied
│   ├── dlq.rs           # Dead-letter queue
│   ├── encryption.rs    # AEAD payload encryption
│   ├── filter.rs        # Expression predicate: keep only matching messages
│   ├── id.rs            # Replay-stable business identity into `mqb.id`
│   ├── limiter.rs       # Throughput limiting (msg/s)
│   ├── metrics.rs       # Metrics collection
│   ├── random_panic.rs  # Testing middleware
│   ├── retry.rs         # Exponential backoff retry
│   ├── transform/       # Declarative JSON mapping + schema coercion
│   └── weak_join.rs     # Correlation-keyed join
├── support/              # Cross-cutting helpers
│   ├── compression.rs   # gzip / lz4 / zstd
│   ├── connection_registry.rs # Shared connection reuse
│   ├── crypto.rs        # AEAD core (used by encryption middleware + at-rest)
│   └── interpolation.rs # `${namespace:selector}` templating
├── command_handler.rs    # Command handler wrapper
├── event_handler.rs      # Event handler wrapper
├── event_store.rs        # In-memory event store
├── type_handler.rs       # Typed message handlers
├── publisher.rs          # Standalone publisher API
├── checkpoint.rs         # Durable cursor stores (file/s3/postgres/mongodb)
├── extensions.rs         # Custom endpoint/middleware factory registration
├── response.rs           # Ergonomic response helpers
├── test_utils.rs         # Shared test helpers
├── errors.rs             # Error types
└── outcomes.rs           # Result types (Handled, Sent, Received)
```

### Consumer vs Subscriber

- **Consumer**: Persistent mode - uses consumer groups/durable queues, resumes from last committed position
- **Subscriber**: Ephemeral mode - unique IDs per instance, receives only new messages

Both implement `MessageConsumer` trait. Subscribers often wrap consumers (e.g., `MemorySubscriber` wraps `MemoryConsumer`).

### Route Execution

Routes can run in two modes:
- **Sequential** (`concurrency: 1`): Single-threaded processing
- **Concurrent** (`concurrency > 1`): Worker pool with configurable concurrency

Routes handle graceful shutdown via `shutdown_rx` channel. The `select!` macro cancels `receive_batch()` futures when shutdown is received.

### Testing

- **Unit tests**: In each module
- **Integration tests**: `tests/integration/` - require Docker services
- **Performance tests**: `tests/performance_pipeline.rs` and `benches/performance_bench.rs`
- **Memory tests**: `tests/memory_test.rs` - no external dependencies

Integration tests use Docker Compose files in `tests/integration/docker-compose/`.

### Configuration

Routes are defined via:
- YAML files
- JSON files
- Environment variables (prefix: `MQB__`, separator: `__`)

Example:
```yaml
kafka_to_nats:
  concurrency: 4
  batch_size: 128
  input:
    kafka:
      topic: "orders"
      url: "localhost:9092"
  output:
    nats:
      subject: "orders.processed"
      url: "nats://localhost:4222"
```

### Common Patterns

1. **Creating endpoints**: Use factory functions in `endpoints/mod.rs` (`create_consumer_from_route`, `create_publisher_from_route`)
2. **Adding middleware**: Middlewares are applied in `middleware/mod.rs` via `apply_middlewares_to_consumer` and `apply_middlewares_to_publisher`
3. **Custom endpoints**: Implement `CustomEndpointFactory` trait
4. **Custom middleware**: Implement `CustomMiddlewareFactory` trait
5. **Typed handlers**: Use `TypeHandler` to deserialize messages into Rust types based on `kind` metadata field
6. **Request-Reply**: Enable via `request_reply: true` in publisher config (NATS, MongoDB, Memory). Uses UUID v7 for correlation.

### Error Handling

- `ConsumerError`: Errors during message consumption (Connection, EndOfStream)
- `ProcessingError` (aliased as `HandlerError`/`PublisherError`): Errors during processing (Retryable, NonRetryable)

### Performance Considerations

- Batch processing is preferred (`receive_batch`, `send_batch`)
- Concurrency is configurable per route
- Middleware adds overhead - use only what's needed
- Memory endpoints are fastest (no I/O), useful for testing

### When Making Changes

1. **New endpoint**: Add to `endpoints/mod.rs` factory functions, implement `MessageConsumer`/`MessagePublisher` traits
2. **New middleware**: Add to `middleware/mod.rs`, implement middleware wrapper types
3. **Configuration changes**: Update `models.rs` with new config structs, add to `EndpointType`/`Middleware` enums
4. **Tests**: Add integration tests in `tests/integration/`, update Docker Compose if needed
5. **Documentation**: Update README.md and add doc comments

### Dependencies

- **tokio**: Async runtime
- **serde**: Serialization/deserialization
- **anyhow**: Error handling
- **tracing**: Structured logging
- **config**: Configuration management
- **async-trait**: Async trait support
- **uuid**: Unique identifiers (v4 and v7)

Optional (feature-gated):
- **rdkafka**: Kafka support
- **async-nats**: NATS support
- **lapin**: AMQP support
- **rumqttc**: MQTT support
- **mongodb**: MongoDB support
- **actix/reqwest**: HTTP support
- **sled**: Deduplication storage
- **metrics**: Metrics collection
