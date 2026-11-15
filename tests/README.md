# Test runs

The default test run should stay Docker-free and skip ignored slow/external tests.

| Category | Command | Notes |
| --- | --- | --- |
| Fast/default tests | `cargo test` | No Docker or external services expected. |
| Docker integration suite | `cargo test --test integration_test --features full,test-utils --release -- --ignored --nocapture --test-threads=1` | Runs the ignored aggregate suites. Docker Compose must be available. |
| One Docker backend | `MQB_TEST_BACKEND=kafka cargo test --test integration_test --features full,test-utils --release -- --ignored --nocapture --test-threads=1` | `MQB_TEST_BACKEND` is a comma-separated substring filter; a `!` prefix excludes (`mongodb,aws` or `!mongodb,!aws`); examples: `kafka`, `nats`, `mongodb`, `amqp`, `mqtt`, `postgres`, `sqlite`, `websocket`. |
| SQLite direct performance | `cargo test --test sqlite_test --features sqlx,test-utils sqlite_performance_direct -- --ignored --nocapture` | File-backed, no Docker. Measured locally at about 42s on 2026-06-21. |
| Route recovery checks | `cargo test test_route_recovery_from_faults -- --ignored --nocapture` | No Docker. Measured locally at about 2s for the two consumer recovery tests on 2026-06-21. |
| Publisher recovery check | `cargo test test_publisher_recovery_from_faults -- --ignored --nocapture` | No Docker. Measured locally at about 1s on 2026-06-21. |
| Downstream Armature integration | `cargo test --test armature_integration -- --ignored --nocapture` | Clones a GitHub repo, patches it, and runs downstream tests. Requires git and network access. |

Use tracing with any command:

```bash
RUST_LOG=info,mq_bridge=trace cargo test ...
```

## Ignored test inventory

- Docker Compose aggregate suites: `tests/integration_test.rs`.
- Docker Compose focused leaf tests: AMQP nack/reconnect tests, MongoDB raw/no-duplicate tests, and TLS backend roundtrips.
- Local slow/performance tests: route recovery tests, SQLite direct performance, memory pipeline performance, and static-to-null performance.
- Local TLS/cert tests: HTTP and gRPC TLS tests generate local certificates and bind local ports.
- External/downstream tests: `armature_integration.rs` clones and tests the downstream Armature repository.

The ignored marker is intentional. It keeps default `cargo test` fast and local while making Docker, performance, TLS, and external-network tests explicit opt-ins.
