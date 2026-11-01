#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if [[ -d /opt/mq-bridge-rust ]]; then
  export PATH="/opt/mq-bridge-rust/cargo/bin:$PATH" RUSTUP_HOME=/opt/mq-bridge-rust/rustup
fi
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --locked
mkdir -p bin
cp target/release/twelve-dollar-mq-bridge bin/server
