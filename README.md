# mq-bridge
`mq-bridge` is an asynchronous message bridging library for Rust. It connects different messaging systems, data stores, and protocols. It is built on Tokio and supports patterns like retries, dead-letter queues, and message deduplication.

## Features

*   **Supported Backends**: Kafka, NATS, AMQP (RabbitMQ), MQTT, MongoDB, HTTP, Files, and in-memory channels.
*   **Configuration**: Routes can be defined via YAML or environment variables.
*   **Middleware**:
    *   **Retries**: Exponential backoff for transient failures.
    *   **Dead-Letter Queues (DLQ)**: Redirect failed messages.
    *   **Deduplication**: Message deduplication using `sled`.
*   **Concurrency**: Configurable concurrency per route using Tokio.

## Core Concepts

*   **Route**: A named data pipeline that defines a flow from one `input` to one `output`.
*   **Endpoint**: A source or sink for messages.
*   **Middleware**: Components that intercept and process messages (e.g., for error handling).
*   **Handler**: A programmatic component for business logic, such as transforming messages (`CommandHandler`) or consuming them (`EventHandler`).

## Usage

### Programmatic Handlers

For implementing business logic, `mq-bridge` provides a handler layer that is separate from transport-level middleware. This allows you to process messages programmatically.

*   **`CommandHandler`**: A handler for 1-to-1 or 1-to-0 message transformations. It takes a message and can optionally return a new message to be passed down the publisher chain.
*   **`EventHandler`**: A terminal handler that consumes a message without returning a new one.

You can chain these handlers with endpoint publishers.

```rust
use mq_bridge::traits::Handler;
use mq_bridge::{CanonicalMessage, Handled};
use std::sync::Arc;

// Define a handler that transforms the message payload
let command_handler = Arc::new(|mut msg: CanonicalMessage| async move {
    let new_payload = format!("handled_{}", String::from_utf8_lossy(&msg.payload));
    msg.payload = new_payload.into();
    Ok(Handled::Publish(msg))
});

// Attach the handler to a route
// let route = Route { ... }.with_handler(command_handler);
```

### Programmatic Usage

You can define and run routes directly in Rust code.

```rust
use mq_bridge::models::{Endpoint, EndpointType, MemoryConfig, CanonicalMessage, Route};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::main]
async fn main() {
    // Define a route from one in-memory channel to another
    use crate::models::{Endpoint, Route};
    
    // 1. Create boolean that is changed in handler
    let success = Arc::new(AtomicBool::new(false));
    let success_clone = success.clone();

    // 2. Define Handler
    let handler = move |mut msg: CanonicalMessage| {
        success_clone.store(true, Ordering::SeqCst);
        msg.set_payload_str(format!("modified {}", msg.get_payload_str()));
        async move { Ok(Handled::Publish(msg)) }
    };
    // 3. Define Route
    let input = Endpoint::new_memory("route_in", 200);
    let output = Endpoint::new_memory("route_out", 200);
    let route = Route::new(input, output)
        .with_handler(Arc::new(handler));

    // 4. Inject Data
    let input_channel = route.input.channel().unwrap();
    input_channel
        .send_message(CanonicalMessage::from_str("hello"))
        .await
        .unwrap();

    // 5. Run
    let res = route.run_until_err("test_route", None);
    input_channel.close();
    res.await.ok(); // eof error due to closed channel

    // 6. Verify
    assert!(success.load(Ordering::SeqCst) == true);

    let msgs = route.output.channel().unwrap().drain_messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].get_payload_str(), "modified hello");
}
```

## Configuration Details

### Environment Variables
All YAML configuration can be overridden with environment variables. The mapping follows this pattern:
`MQB__{ROUTE_NAME}__{PATH_TO_SETTING}`

For example, to set the Kafka topic for the `kafka_to_nats` route:
```sh
export MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC="my-other-topic"
```

### Middleware Configuration
Middleware is defined as a list under an endpoint.

```yaml
input:
  middlewares:
    - retry:
        max_attempts: 5
        initial_interval_ms: 200
    - dlq:
        endpoint:
          nats:
            subject: "my-dlq-subject"
            url: "nats://localhost:4222"
    - deduplication:
        sled_path: "/var/data/mq-bridge/dedup_db"
        ttl_seconds: 3600 # 1 hour
  kafka:
    # ... kafka config
```

## Running Tests
The project includes a comprehensive suite of integration and performance tests that require Docker.

To run the performance benchmarks for all supported backends:
```sh
cargo test --test integration_test --release -- --ignored --nocapture --test-threads=1
```

To run the criterion benchmarks:
```sh
cargo bench --features "full"
```

## License
`mq-bridge` is licensed under the MIT License.
