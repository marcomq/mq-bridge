# Desktop App Slot

This directory is reserved for a future Tauri desktop application.

You can build it by running `cargo run -p mq-bridge-app-desktop`

Planned shape:

- `crates/core`: shared backend logic and transport-agnostic application services
- `crates/cli`: installable HTTP/CLI application published as `mq-bridge-app`
- `crates/desktop`: future Tauri application depending on `crates/core`

Keeping the desktop app in its own crate avoids pulling Tauri dependencies into the CLI build or the `cargo install mq-bridge-app` package.
