# Contributing to mq-bridge

Thank you for your interest in contributing to **mq-bridge**! We welcome bug reports, feature requests, documentation improvements, and code contributions.

## Getting Started

1. **Fork the repository** and clone your fork locally.
2. **Install Rust** (stable, via [rustup](https://rustup.rs/)).
3. **Install required dependencies** for optional features (Kafka, NATS, AMQP, etc.) as needed. The test folder also has Docker-Compose files for the specific brokers, so you don't need to install them natively.
4. **Run tests** to verify your environment:
   ```sh
   cargo test --features full
   ```

## Code Style

- Run `cargo fmt --all` before submitting a PR.
- Ensure code passes `cargo clippy --all-targets --features lint-all -- -D warnings`.
  Not `--all-features`: that enables `link-static` and `link-dynamic` at once,
  which the crate rejects with a `compile_error!`. `lint-all` is every feature
  that gates code, with one linkage picked.
- Follow idiomatic Rust and existing code conventions.

## Making Changes

- **New endpoints or middleware:**
  - Add new files in `src/endpoints/` or `src/middleware/`.
  - Update factory functions in `mod.rs` as needed.
  - Add configuration models to `src/models.rs`.
- **Tests:**
  - Add or update unit tests in the relevant module.
  - Add integration tests in `tests/integration/` if applicable.
- **Documentation:**
  - Update `README.md` and add doc comments for public APIs.

## Running Tests

- **Unit tests:**
  ```sh
  cargo test
  ```
- **Integration tests:**
  ```sh
  cargo test --test integration_test --features full
  ```
- **Performance tests:**
  ```sh
  cargo bench
  ```
- **Memory tests:**
  ```sh
  cargo test --test memory_test
  ```

Some integration tests require Docker services. See `tests/integration/docker-compose/` for setup.

## Submitting a Pull Request

1. **Create a branch** for your change.
2. **Write clear commit messages**.
3. **Open a pull request** against the `main` branch.
4. **Describe your changes** and reference any related issues.
5. Ensure all tests pass and CI checks succeed.

## Reporting Issues

- Use [GitHub Issues](https://github.com/marcomq/mq-bridge/issues) for bugs, enhancements, or questions.
- Provide as much detail as possible (logs, configs, steps to reproduce).

## Code of Conduct

Be respectful and inclusive. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) if present.

---

Thank you for helping make **mq-bridge** better!
