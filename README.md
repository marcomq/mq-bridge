# mq-bridge library

[![Crates.io](https://img.shields.io/crates/v/mq-bridge.svg)](https://crates.io/crates/mq-bridge)
[![Docs.rs](https://docs.rs/mq-bridge/badge.svg)](https://docs.rs/mq-bridge)
[![Benchmark](https://github.com/marcomq/mq-bridge/actions/workflows/benchmark.yml/badge.svg)](https://marcomq.github.io/mq-bridge/dev/bench/)
![Linux](https://img.shields.io/badge/Linux-supported-green?logo=linux)
![Windows](https://img.shields.io/badge/Windows-supported-green?logo=windows)
![macOS](https://img.shields.io/badge/macOS-supported-green?logo=apple)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

```text
      ┌────── mq-bridge-lib ──────┐
──────┴───────────────────────────┴──────
```

`mq-bridge` is an asynchronous message library for Rust. It connects different messaging systems, data stores, and protocols. Unlike a classic bridge that simply forwards messages, `mq-bridge` acts as a **programmable integration layer**, allowing for transformation, filtering, routing and event/command handling. It is built on Tokio and supports patterns like retries, dead-letter queues, and message deduplication.

## Features

*   **Supported Backends**: Kafka, NATS, AMQP (RabbitMQ), MQTT, MongoDB, HTTP, ZeroMQ, Files, AWS (SQS/SNS), IBM MQ, and in-memory channels.
    > **Note**: IBM MQ is not included in the `full` feature set. It requires the `ibm-mq` feature and the IBM MQ Client library. See [mqi crate](https://crates.io/crates/mqi/) for installation details.
*   **Configuration**: Routes can be defined via YAML, JSON or environment variables.
*   **Programmable Logic**: Inject custom Rust handlers to transform or filter messages in-flight.
*   **Middleware**:
    *   **Retries**: Exponential backoff for transient failures.
    *   **Dead-Letter Queues (DLQ)**: Redirect failed messages.
    *   **Deduplication**: Message deduplication using `sled`.
*   **Concurrency**: Configurable concurrency per route using Tokio.

## Philosophy & Focus

`mq-bridge` is designed as a **programmable integration layer**. Its primary goal is to decouple your application logic from the underlying messaging infrastructure.

Unlike libraries that enforce specific architectural patterns (like strict CQRS/Event Sourcing domain modeling) or concurrency models (like Actors), `mq-bridge` remains unopinionated about your domain logic. Instead, it focuses on **reliable data movement** and **protocol abstraction**.

## Status

This library was created in 2025 and is still kind of new. There are automated unit and integration tests. 
There are integration tests for consumers and publishers to verify that they are working 
as expected with standard docker containers of the latest stable version.

It may still be possible that there are issues with
- old or very new versions of broker servers
- specific settings of the brokers
- subscribers, as those haven't been tested a lot
- TLS integration, as this also hasn't been tested a lot and is usually non-trivial to set up

### When to use mq-bridge
*   **Hybrid Messaging**: Connect systems speaking different protocols (e.g., MQTT to Kafka) without writing custom adapters.
*   **Infrastructure Abstraction**: Write business logic that consumes `CanonicalMessage`s, allowing you to swap the underlying transport (e.g., switching from RabbitMQ to NATS) via configuration.
*   **Resilient Pipelines**: Apply uniform reliability patterns (Retries, DLQ, Deduplication) across all your data flows.
*   **Sidecar / Gateway**: Deploy as a standalone service to ingest, filter, and route messages before they reach your core services.

### When NOT to use mq-bridge
*   **Stateful Stream Processing**: For windowing, joins, or complex aggregations over time, dedicated stream processing engines are more suitable.
*   **Domain Aggregate Management**: If you need a framework to manage the lifecycle, versioning, and replay of domain aggregates (Event Sourcing), use a specialized library. `mq-bridge` handles the *bus*, not the *entity*.
*   **Specialization**: `mq-bridge` focuses on a subset of messaging patterns like pub/sub and batching, emulating them if not natively supported. If you need very specific features from a messaging library or protocol, the abstraction layer of `mq-bridge` may prevent you from using them.

## Core Concepts

*   **Route**: A named data pipeline that defines a flow from one `input` to one `output`.
*   **Endpoint**: A source or sink for messages.
*   **Middleware**: Components that intercept and process messages (e.g., for error handling).
*   **Handler**: A programmatic component for business logic, such as transforming/consuming messages (`CommandHandler`) or subscribe them (`EventHandler`).

## Endpoint Behavior

`mq-bridge` endpoints generally default to a **Consumer** pattern (Queue), where messages are persisted (if supported by the backend) and distributed among workers.

To achieve **Subscriber** (Pub/Sub) behavior—where messages are broadcast to all active instances—you must configure the specific backend accordingly. There is no global "subscriber mode" toggle; it is determined by the configuration of the endpoint.

| Backend | Default Behavior (Queue) | Configuration for Subscriber (Pub/Sub) | Response Support |
| :--- | :--- | :--- | :--- |
| **Kafka** | Persistent (Consumer Group) | Omit `group_id` (generates unique ID) | No |
| **NATS** | Persistent (JetStream Durable) | Set `subscriber_mode: true` | Yes |
| **AMQP** | Persistent (Durable Queue) | Set `subscribe_mode: true` | No |
| **MQTT** | Persistent Session | Set `clean_session: true` | No |
| **IBM MQ** | Persistent Queue | Set `topic` instead of `queue` | No |
| **MongoDB** | Persistent (Collection) | Set `change_stream: true` | Yes |
| **AWS** | Persistent (SQS) | Not supported directly (Use SNS->SQS) | No |
| **Memory** | Ephemeral (Channel) | Set `subscribe_mode: true` | Yes |
| **File** | Persistent (Delete after read) | Set `subscribe_mode: true` (Tail) | No |
| **HTTP** | Ephemeral (Request) | N/A | Yes (Implicit) |
| **ZeroMQ** | Ephemeral (PULL) | Set `socket_type: "sub"` | No |

### Response Mode
The `response` output endpoint allows sending a reply back to the requester. This is useful for synchronous request-reply patterns (e.g., HTTP-to-NATS-to-HTTP).

*   **Availability**: Only available if the **Input** endpoint supports request-reply (HTTP, NATS, Memory, MongoDB).
*   **Configuration**: Use `response: {}` as the output endpoint.
*   **Caveats**:
    *   If the input does not support responses (e.g., File, Kafka), the message sent to `response` will be dropped.
    *   Ensure timeouts are configured correctly on the requester side, as the bridge processing time adds latency.
    *   Middleware that drops metadata (like `correlation_id`) may break the response chain.

## Usage

There is a separate repository to use mq-bridge as standalone app, for example as docker container that can be configured via yaml or env variables:
https://github.com/marcomq/mq-bridge-app

### Programmatic Handlers

For implementing business logic, `mq-bridge` provides a handler layer that is separate from transport-level middleware. This allows you to process messages programmatically.

#### Raw Handlers

*   **`CommandHandler`**: A handler for 1-to-1 or 1-to-0 message transformations. It takes a message and can optionally return a new message to be passed down the publisher chain.
*   **`EventHandler`**: A terminal handler that reads new messages without removing them for other event handlers.

You can chain these handlers with endpoint publishers.

```rust
use mq_bridge::traits::Handler;
use mq_bridge::{CanonicalMessage, Handled};
use std::sync::Arc;

// Define a handler that transforms the message payload
let command_handler = |mut msg: CanonicalMessage| async move {
    let new_payload = format!("handled_{}", String::from_utf8_lossy(&msg.payload));
    msg.payload = new_payload.into();
    Ok(Handled::Publish(msg))
};

// Attach the handler to a route
// let route = Route { ... }.with_handler(command_handler);
```

#### Typed Handlers

For more structured, type-safe message handling, `mq-bridge` provides `TypeHandler`. It deserializes messages into a specific Rust type before passing them to a handler function. This simplifies message processing by eliminating manual parsing and type checking.

Message selection is based on the `kind` metadata field in the `CanonicalMessage`.

```rust
use mq_bridge::type_handler::TypeHandler;
use mq_bridge::{CanonicalMessage, Handled};
use serde::Deserialize;
use std::sync::Arc;

// 1. Define your message structures
#[derive(Deserialize)]
struct CreateUser {
    id: u32,
    username: String,
}

#[derive(Deserialize)]
struct DeleteUser {
    id: u32,
}

// 2. Create a TypeHandler and register your typed handlers
let typed_handler = TypeHandler::new()
    .add("create_user", |cmd: CreateUser| async move {
        println!("Handling create_user: {}, {}", cmd.id, cmd.username);
        // ... your logic here
        Ok(Handled::Ack)
    })
    .add("delete_user", |cmd: DeleteUser| async move {
        println!("Handling delete_user: {}", cmd.id);
        // ... your logic here
        Ok(Handled::Ack)
    });

// 3. Attach the handler to a route
let route = Route::new(input, output).with_handler(typed_handler);

// 4. To send a message to the route's input, create a publisher for that endpoint.
//    In a real application, you would create this publisher once and reuse it.
let input_publisher = Publisher::new(route.input.clone()).await.unwrap();

// 5. Create a typed command, serialize it, and send it via the publisher.
let command = CreateUser { id: 1, username: "test".to_string() };
let message = msg!(&command, "create_user"); // This sets the `kind` metadata field.
input_publisher.send(message).await.expect("Failed to send message");

// The running route will receive the message, see the `kind: "create_user"` metadata,
// deserialize the payload into a `CreateUser` struct, and pass it to your registered handler.
```

### Programmatic Usage

You can define and run routes directly in Rust code.

```rust
use mq_bridge::{models::Endpoint, stop_route, CanonicalMessage, Handled, Route};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[tokio::main]
async fn main() {
    // Define a route from one in-memory channel to another

    // 1. Create a boolean that is changed in the handler
    let success = Arc::new(AtomicBool::new(false));
    let success_clone = success.clone();

    // 2. Define the Handler
    let handler = move |mut msg: CanonicalMessage| {
        success_clone.store(true, Ordering::SeqCst);
        msg.set_payload_str(format!("modified {}", msg.get_payload_str()));
        async move { Ok(Handled::Publish(msg)) }
    };
    // 3. Define Route
    let input = Endpoint::new_memory("route_in", 200);
    let output = Endpoint::new_memory("route_out", 200);
    let route = Route::new(input, output).with_handler(handler);

    // 4. Run (deploys the route in the background)
    route.deploy("test_route").await.unwrap();

    // 5. Inject Data
    let input_channel = route.input.channel().unwrap();
    input_channel
        .send_message("hello".into())
        .await
        .unwrap();

    // 6. Verify
    let mut verifier = route.connect_to_output("verifier").await.unwrap();
    let received = verifier.receive().await.unwrap();
    assert_eq!(received.message.get_payload_str(), "modified hello");
    assert!(success.load(Ordering::SeqCst));

    stop_route("test_route").await;
}
```

## Patterns: Request-Response

`mq-bridge` supports request-response patterns, essential for building interactive services (e.g., web APIs). This pattern allows a client to send a request and wait for a correlated response. Due to the asynchronous nature of messaging, ensuring the correct response is delivered to the correct requester is critical, especially under concurrent loads.

`mq-bridge` offers two ways to handle this, with the `response` output being the most direct and safest for handling concurrency.

### The `response` Output Endpoint (Recommended)

The recommended approach for request-response is to use the dedicated `response` endpoint in your route's `output`.

**How it works:**
1. An input endpoint that supports request-response (like `http`) receives a request.
2. The message is passed through the route's processing chain. This is where you typically attach a `handler` to process the request and generate a response payload.
3. The final message is sent to the `output`.
4. If the output is `response: {}`, the bridge sends the message back to the original input source, which then sends it as the reply (e.g., as an HTTP response).

This model inherently solves the correlation problem. The response is part of the same execution context as the request, so there's no risk of mixing up responses between different concurrent requests.

#### Example: MongoDB Request-Response

Consider a scenario where a service writes a request document to MongoDB and waits for a reply. This library picks up the document, processes it via a handler, and writes the result back to a reply collection.

**YAML Configuration (`mq-bridge.yaml`):**
```yaml
mongo_responder:
  input:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "requests"
  output:
    # The 'response' endpoint sends the processed message back to the 'requests_replies' collection
    # (or whatever reply_to was set to by the sender).
    response: {}
```

**Programmatic Handler Attachment (in Rust):**
You would then load this configuration and attach a handler to the route's output endpoint in your Rust code.

```rust
use mq_bridge::models::{Config, Handled};
use mq_bridge::CanonicalMessage;

async fn run() {
    // 1. Load configuration from YAML
    // let config: Config = serde_yaml_ng::from_str(include_str!("mq-bridge.yaml")).unwrap();
    // let mut route = config.get("api_gateway").unwrap().clone();

    // 2. Define the handler that processes the request
    let handler = |mut msg: CanonicalMessage| async move {
        // Example: echo the request body with a prefix
        let request_body = String::from_utf8_lossy(&msg.payload);
        let response_body = format!("Handled response for: {}", request_body);
        msg.payload = response_body.into();
        Ok(Handled::Publish(msg))
    };

    // 3. Attach the handler to the output endpoint
    // route.output.handler = Some(std::sync::Arc::new(handler));
    
    // 4. Run the route
    // route.deploy("api_gateway").await.unwrap();
}
```

## Patterns: CQRS 
mq-bridge is well-suited for implementing Command Query Responsibility Segregation (CQRS). By combining Routes with Typed Handlers, the bridge serves as both the Command Bus and the Event Bus. 
* Command Bus: An input source (e.g., HTTP) receives a command. A TypeHandler processes it (Write Model) and optionally emits an event. 
* Event Bus: The emitted event is published to a broker (e.g., Kafka). Downstream routes subscribe to these events to update Read Models (Projections). 

```rust 
// 1. Command Handler (Write Side) 
let command_bus = TypeHandler::new()
    .add("submit_order", |cmd: SubmitOrder| async move {
        // Execute business logic, save to DB...
        // Emit event
        let evt = OrderSubmitted { id: cmd.id };
        Ok(Handled::Publish(
            msg!(evt, "order_submitted")
        ))
});

// 2. Event Handler (Read Side / Projection) 
let projection_handler = TypeHandler::new()
    .add("order_submitted", |evt: OrderSubmitted| async move {
        // Update read database / cache
        Ok(Handled::Ack)
}); 
```

## Configuration Reference

The best way to understand the configuration structure is through a comprehensive example. `mq-bridge` uses a YAML map where keys are route names.

```yaml
# mq-bridge.yaml

# Route 1: Kafka to NATS
kafka_to_nats:
  concurrency: 4
  input:
    kafka:
      url: "localhost:9092"
      topic: "orders"
      group_id: "bridge_group"
      # TLS Configuration (Optional)
      tls:
        required: true
        ca_file: "./certs/ca.pem"
  output:
    nats:
      url: "nats://localhost:4222"
      subject: "orders.processed"
      stream: "orders_stream"

# Route 2: HTTP Webhook to MongoDB with Middleware
webhook_to_mongo:
  input:
    http:
      url: "0.0.0.0:8080"
      # Optional: Send response back to HTTP caller via another endpoint
      response_out:
        static: "Accepted"
    middlewares:
      - retry:
          max_attempts: 3
          initial_interval_ms: 500
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "app_db"
      collection: "webhooks"
      format: "json" # a bit slower, but better readability

# Route 3: File to AMQP (RabbitMQ)
file_ingest:
  input:
    file: "./data/input.jsonl"
  output:
    amqp:
      url: "amqp://localhost:5672"
      exchange: "logs"
      queue: "file_logs"

# Route 4: AWS SQS to SNS
aws_sqs_to_sns:
  input:
    aws:
      # To consume from SNS, subscribe this SQS queue to the SNS topic in AWS Console/Terraform.
      queue_url: "https://sqs.us-east-1.amazonaws.com/000000000000/my-queue"
      region: "us-east-1"
      # Credentials (optional if using env vars or IAM roles)
      access_key: "test"
      secret_key: "test"
  output:
    aws:
      topic_arn: "arn:aws:sns:us-east-1:000000000000:my-topic"
      region: "us-east-1"

# Route 5: IBM MQ Example
ibm_mq_route:
  input:
    ibm_mq:
      queue_manager: "QM1"
      connection_name: "localhost(1414)"
      channel: "DEV.APP.SVRCONN"
      queue: "DEV.QUEUE.1"
      user: "app"
      password: "admin"
  output:
    memory:
      topic: "received_from_mq"

# Route 6: MQTT to Switch (Content-based Routing)
iot_router:
  input:
    mqtt:
      url: "mqtt://localhost:1883"
      topic: "sensors/+"
      qos: 1
  output:
    switch:
      metadata_key: "sensor_type"
      cases:
        temp:
          kafka:
            url: "localhost:9092"
            topic: "temperature"
      default:
        memory:
          topic: "dropped_sensors"

# Route 7: ZeroMQ PUSH/PULL
zeromq_pipeline:
  input:
    zeromq:
      url: "tcp://0.0.0.0:5555"
      socket_type: "pull"
      bind: true
  output:
    zeromq:
      url: "tcp://localhost:5556"
      socket_type: "push"
      bind: false
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

### Specialized Endpoints

#### Switch

The `switch` endpoint is a conditional publisher that routes messages to different outputs based on a metadata key.

It checks the specified `metadata_key` in each message. If the key's value matches one of the `cases`, the message is forwarded to that endpoint. If no case matches, it's sent to the `default` endpoint. If there is no default, the message is dropped.

This is useful for content-based routing.

**Example**: Route orders to different systems based on `country_code` metadata.

```yaml
output:
  switch:
    metadata_key: "country_code"
    cases:
      US:
        kafka:
          topic: "us_orders"
          url: "kafka-us:9092"
      EU:
        nats:
          subject: "eu_orders"
          url: "nats-eu:4222"
    default:
      file:
        path: "/var/data/unroutable_orders.log"
```

### IDE Support (Schema Validation) 
mq-bridge includes a JSON schema for configuration validation and auto-completion. 
1. Ensure you have a YAML plugin installed (e.g., YAML for VS Code). 
2. Configure your editor to reference the schema. For VS Code, add this to .vscode/settings.json: 
```json 
{ 
  "yaml.schemas": { 
    "https://raw.githubusercontent.com/marcomq/mq-bridge/main/mq-bridge.schema.json": ["mq-bridge.yaml", "config.yaml"]
  } 
} 
```
To regenerate the schema from this repo, run: `cargo test --features schema`

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
The times are not stable yet, it is therefore recommended to perform the integration performance test if you want to measure throughput.

## License
`mq-bridge` is licensed under the MIT License.
