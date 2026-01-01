# Unit Test Guidelines

This document outlines the unit testing strategy for mq-bridge.

## Test Organization

### Current Test Structure

```
tests/
├── integration/          # Integration tests (require Docker)
│   ├── all_endpoints.rs
│   ├── kafka.rs
│   ├── nats.rs
│   ├── amqp.rs
│   ├── mqtt.rs
│   ├── mongodb.rs
│   ├── memory.rs
│   └── route.rs
├── integration_test.rs   # Main integration test runner
├── request_reply_test.rs # Request-reply pattern tests
├── memory_test.rs        # Memory-only performance tests
└── performance_pipeline.rs # Pipeline performance tests
```

### Unit Tests in Source Files

Each module should have unit tests in a `#[cfg(test)]` block:

- `src/models.rs` - Configuration parsing tests
- `src/canonical_message.rs` - Message creation/parsing tests
- `src/route.rs` - Route execution logic tests
- `src/endpoints/*.rs` - Endpoint-specific tests
- `src/middleware/*.rs` - Middleware behavior tests

## Test Categories

### 1. Unit Tests (Fast, No I/O)

Test individual functions and types in isolation:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_canonical_message_creation() {
        let msg = CanonicalMessage::new(b"test".to_vec(), None);
        assert_eq!(msg.payload, b"test");
    }
}
```

### 2. Integration Tests (Require Services)

Test with real message brokers/services via Docker:

```rust
#[tokio::test]
#[ignore] // Requires Docker
async fn test_kafka_consumer() {
    // Setup Kafka, test consumer
}
```

### 3. Memory Tests (Fast, No External Dependencies)

Test using in-memory channels:

```rust
#[tokio::test]
async fn test_memory_route() {
    let input = Endpoint::new_memory("test_in", 100);
    let output = Endpoint::new_memory("test_out", 100);
    // Test route execution
}
```

## Test Coverage Goals

### High Priority (Critical Path)

- [x] Message creation and parsing
- [x] Route execution (sequential and concurrent)
- [x] Memory endpoints
- [ ] Error handling and propagation
- [ ] Middleware chaining
- [ ] Configuration parsing (YAML, JSON, env vars)

### Medium Priority (Important Features)

- [ ] All endpoint types (Kafka, NATS, AMQP, MQTT, MongoDB, HTTP)
- [ ] Middleware implementations (retry, DLQ, deduplication)
- [ ] Type handlers
- [ ] Fanout and switch endpoints
- [ ] TLS configuration

### Lower Priority (Edge Cases)

- [ ] Large message handling
- [ ] Concurrent route execution edge cases
- [ ] Resource cleanup
- [ ] Configuration validation

## Writing New Tests

### Test Naming Convention

- Unit tests: `test_<functionality>`
- Integration tests: `test_<endpoint>_<scenario>`
- Performance tests: `bench_<operation>`

### Example Test Template

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalMessage;
    
    #[tokio::test]
    async fn test_example() {
        // Arrange
        let input = Endpoint::new_memory("test", 10);
        
        // Act
        let result = some_function(input).await;
        
        // Assert
        assert!(result.is_ok());
    }
}
```

### Integration Test Template

```rust
#[tokio::test]
#[ignore] // Requires Docker
async fn test_kafka_integration() {
    use crate::tests::integration::common::*;
    
    // Start Docker services
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        // Test implementation
    }).await;
}
```

## Running Tests

```bash
# All tests
cargo test

# Unit tests only (fast)
cargo test --lib

# Integration tests (requires Docker)
cargo test --test integration_test --features full --release -- --ignored --nocapture --test-threads=1

# Request-reply tests (requires Docker)
cargo test --test request_reply_test --features full --release -- --ignored --nocapture --test-threads=1

# Memory tests
cargo test --test memory_test --features integration-test --release -- --nocapture

# Specific test
cargo test test_name

# With output
cargo test -- --nocapture
```

## Test Data

Use fixtures for common test data:

```rust
fn create_test_message() -> CanonicalMessage {
    CanonicalMessage::new(b"test payload".to_vec(), None)
}

fn create_test_route() -> Route {
    Route::new(
        Endpoint::new_memory("input", 100),
        Endpoint::new_memory("output", 100),
    )
}
```

## Mocking

For testing without external dependencies:

- Use `MemoryEndpoint` for in-memory message passing
- Use `NullPublisher` for testing consumers without side effects
- Use `StaticEndpoint` for predictable responses

## Performance Testing

Performance tests should:
- Use `criterion` for benchmarking
- Run in `benches/` directory
- Be marked with `#[ignore]` in CI
- Compare against baseline metrics

## Continuous Integration

- Unit tests run on every PR
- Integration tests run on main branch (may be flaky)
- Performance benchmarks run on schedule
- All tests must pass before merging
