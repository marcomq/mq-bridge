This is supposed to run via `cargo test --test integration_test --features full --release -- --ignored --nocapture --test-threads=1 --show-output`

Alternatively, you can use this to start a test bridge manually from project root:
```bash
docker-compose -f tests/integration/config/integration.yml up -d
CONFIG_FILE=tests/integration/config/integration.yml cargo run
```

###
Internal memory performance test:
`cargo test --test memory_test --release -- --ignored --nocapture`

###
Run with full tracing:
`RUST_LOG=info,mq_bridge=trace cargo test...`

###
Run only one test, for example aws:
`MQB_TEST_BACKEND=aws cargo test --test integration_test --features full --release  -- --ignored --nocapture`
