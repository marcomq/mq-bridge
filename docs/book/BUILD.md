# Build from source

Developer build instructions for `mq-bridge-app` — the CLI/server, the desktop
(Tauri) app, and the Docker image. For prebuilt installs (`cargo binstall`,
release bundles, Docker Hub images) see the [main README](https://github.com/marcomq/mq-bridge/blob/main/apps/mq-bridge-app/README.md).

## Prerequisites

- Rust toolchain (latest stable version recommended)
- Access to the message brokers you want to connect (e.g. Kafka, NATS, RabbitMQ)

## CLI / server

1.  **Clone the repository:**
    ```bash
    git clone https://github.com/marcomq/mq-bridge
    cd mq-bridge
    ```

2.  **Build and run the CLI (empty):**
    ```bash
    cargo run --release -p mq-bridge-app
    ```

3.  **Build and run with a config:**
    ```bash
    cargo run --release -p mq-bridge-app -- --config apps/mq-bridge-app/dev/config/file-to-http.yml
    ```

Create a `config.yml` in the project root or set environment variables — or start
without one and use the UI to define `config.yml`.

For IBM MQ, install the client library first and build with `--features=ibm-mq`.
See the [IBM MQ Setup Guide](./IBM_MQ_SETUP.md).

## Desktop app (Tauri)

The desktop UI is a Svelte frontend driven by a Tauri backend:

```bash
npm --prefix apps/mq-bridge-app install
npm --prefix apps/mq-bridge-app run dev
```

To build the UI bundle and serve it from the Rust backend:

```bash
npm --prefix apps/mq-bridge-app run build:ui
cargo run --release -p mq-bridge-app
```

## Docker image (no local Rust required)

Requires Docker and Docker Compose:

```bash
docker compose -f apps/mq-bridge-app/docker-compose.yml up --build
```

This builds and starts the bridge CLI application.
