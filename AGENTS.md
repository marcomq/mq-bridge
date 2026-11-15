# mq-bridge Project Context

> **Looking for what middleware or structural endpoints exist, and how to configure them?**
> [REFERENCE.md](docs/REFERENCE.md) is the complete, authoritative list — every middleware
> (`retry`, `dlq`, `transform`, `id`, `filter`, `deduplication`, `weak_join`, `buffer`, `limiter`, `delay`,
> `cookie_jar`, `encryption`, `compression`, `pack`, `unpack`, `timeout`, `metrics`, `otel`, `lookup`, `aggregate`, `random_panic`, `custom`) and every
> structural endpoint (`ref`,
> `fanout`, `switch`, `sequence`, `request`, `response`, `reader`, `static`, `stream_buffer`, `null`,
> `custom`), each with its fields, defaults, and a working YAML example. Do not infer these
> from the enum definitions in `models.rs`; the reference records the behaviour and the
> spelling traps too. Every snippet in it is parsed by `tests/reference_docs_test.rs`.

## Project Overview

`mq-bridge` is an asynchronous Rust library that moves messages and data between brokers, databases, object storage, files and HTTP. What it can do, and where each capability is documented, is in [llms.txt](llms.txt) and the README's "Capabilities at a glance" table; this file covers how to work on the engine. The app in `apps/mq-bridge-app` has its own [AGENTS.md](apps/mq-bridge-app/AGENTS.md).

Source layout (module tree), consumer vs subscriber, error types, common patterns and performance notes: [ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Key Concepts

1. **Route**: A named data pipeline that defines a flow from one `input` endpoint to one `output` endpoint
2. **Endpoint**: A source (consumer) or sink (publisher) for messages - supports Kafka, NATS, AMQP, MQTT, MongoDB, ZeroMQ, HTTP, Files, and in-memory channels
3. **Top-Level API**: A set of functions in the `mq_bridge` root for managing routes and publishers (e.g., `get_route`, `stop_route`, `get_publisher`).
4. **Middleware**: Components that intercept and process messages (retries, DLQ, deduplication, metrics)
5. **Handler**: Programmatic components for business logic (CommandHandler for 1-to-1 transformations, EventHandler for terminal consumption, ThreadedHandler for blocking `&mut` handlers such as local inference: emit many per input, cancel by correlation id)
6. **CanonicalMessage**: The unified message format used throughout the system
7. **Request-Reply**: Native support for synchronous request-reply pattern on NATS, MongoDB, and Memory endpoints

Business logic is written against `CanonicalMessage`; transports are swapped via configuration. Optional dependencies sit behind Cargo features.

## Route Execution

- **Sequential** (`concurrency: 1`): One send at a time in source order; the next batch is read while it runs
- **Concurrent** (`concurrency > 1`): Worker pool with configurable concurrency

Routes handle graceful shutdown via `shutdown_rx` channel. The `select!` macro cancels `receive_batch()` futures when shutdown is received.

## Testing

- **Unit tests**: In each module
- **Integration tests**: `tests/integration/` - require Docker services (Compose files in `tests/integration/docker-compose/`)
- **Performance tests**: `tests/performance_pipeline.rs` and `benches/performance_bench.rs`
- **Memory tests**: `tests/memory_test.rs` - no external dependencies

## Configuration

Routes are defined via YAML files, JSON files, or environment variables (prefix: `MQB__`, separator: `__`). The grammar and examples are in [CONFIGURATION.md](docs/CONFIGURATION.md).

## When Making Changes

1. **New endpoint**: Add to `endpoints/mod.rs` factory functions, implement `MessageConsumer`/`MessagePublisher` traits
2. **New middleware**: Add to `middleware/mod.rs`, implement middleware wrapper types
3. **Configuration changes**: Update `models.rs` with new config structs, add to `EndpointType`/`Middleware` enums
4. **Tests**: Add integration tests in `tests/integration/`, update Docker Compose if needed
5. **Documentation**: Update README.md and add doc comments; when middleware or endpoint behaviour changes, also check the book's hand-written pages in `docs/book/cookbook` and `operations`

## Dependencies

Dependencies and the feature that gates each optional one are in `Cargo.toml`, with comments explaining the non-obvious choices. The only default features are `file` and `dir-spool`; an endpoint whose feature is off is served by a registered factory of the same name if there is one (see `plugins/full`).
