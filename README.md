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
use mq_bridge::traits::{CommandHandler, CommandHandlerPublisher};
use std::sync::Arc;

// Define a handler that transforms the message payload
let command_handler = Arc::new(|mut msg: mq_bridge::CanonicalMessage| async move {
    let new_payload = format!("handled_{}", String::from_utf8_lossy(&msg.payload));
    msg.payload = new_payload.into();
    Ok(mq_bridge::outcomes::HandlerOutcome::Publish(msg))
});
```

### Programmatic Usage

You can define and run routes directly in Rust code.

```rust
use mq_bridge::models::{Endpoint, EndpointType, MemoryConfig, Route};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::main]
async fn main() {
    // Define a route from one in-memory channel to another
    let route = Route {
        input: Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: "mem-in".to_string(),
            capacity: Some(100),
        })),
        output: Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: "mem-out".to_string(),
            capacity: Some(100),
        })),
        concurrency: 1,
    };

    // Get handles to the memory channels for testing
    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    // Run the route. It will stop when the input channel is closed and empty.
    let (run_result, _) = tokio::join!(
        route.run_until_err("memory-test", None),
        async {
            // Send a message
            in_channel.send_message(mq_bridge::CanonicalMessage::new(b"hello".to_vec(), None)).await.unwrap();
            // Close the input channel to allow the route to terminate
            in_channel.close();
        }
    );

    // Ensure the route ran without errors
    run_result.unwrap();

    // Verify the message was received
    let received_messages = out_channel.drain_messages();
    assert_eq!(received_messages.len(), 1);
    assert_eq!(received_messages[0].payload, "hello".as_bytes());

    println!("Message successfully bridged from 'mem-in' to 'mem-out'!");
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
