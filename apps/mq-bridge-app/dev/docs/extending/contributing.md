# Contributing

Contributions to the **engine** (`mq-bridge`) — bug reports, feature requests, docs, and
code — are welcome. The engine repo holds the authoritative
[`CONTRIBUTING.md`](https://github.com/marcomq/mq-bridge/blob/main/CONTRIBUTING.md); this
page is a short orientation.

## Getting started

1. Fork and clone the [engine repo](https://github.com/marcomq/mq-bridge).
2. Install Rust (stable, via [rustup](https://rustup.rs/)).
3. The `tests/` folder ships Docker-Compose files for each broker, so you don't need to
   install Kafka/NATS/AMQP/etc. natively.
4. Verify your environment: `cargo test -p mq-bridge-app --features full`.

## Code style

- `cargo fmt --all` before submitting a PR.
- `cargo clippy -p mq-bridge-app --all-targets -- -D warnings` must pass (what
  `app.yml` runs). Not `--all-features`: `full` and `full-dynamic` are mutually
  exclusive — they select the engine's `link-static` and `link-dynamic`
  respectively — so enabling both fails with a `compile_error!`.
- Follow idiomatic Rust and existing conventions.

## Adding an endpoint or middleware

- Add files under `src/endpoints/` or `src/middleware/`.
- Update the factory functions in the relevant `mod.rs`.
- Add configuration models to `src/models.rs`.
- Add/adjust unit tests in the module, and integration tests under
  `tests/integration/` where applicable.
- Keep the [`REFERENCE.md`](../engine/reference.md) snippets valid — they are parsed by
  `tests/reference_docs_test.rs`.

## Building this book

The documentation lives here in `mq-bridge-app` under `dev/docs/`. To build it locally,
see [`dev/docs/README.md`](../README.md).

## See also

- Engine [`CONTRIBUTING.md`](https://github.com/marcomq/mq-bridge/blob/main/CONTRIBUTING.md) — the full, authoritative guide.
- [Custom endpoints](custom-endpoints.md) and [Custom middleware](custom-middleware.md) — extend without forking.
