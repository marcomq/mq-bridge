#!/usr/bin/env bash
# Runs once as root on a clean Ubuntu 24.04: a C compiler (bundled SQLite, ring, zstd) and Rust.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential ca-certificates curl
export CARGO_HOME=/opt/mq-bridge-rust/cargo RUSTUP_HOME=/opt/mq-bridge-rust/rustup
installer=$(mktemp); trap 'rm -f "$installer"' EXIT
curl -fsSL --retry 3 https://sh.rustup.rs -o "$installer"
sh "$installer" -y --no-modify-path --profile minimal --default-toolchain 1.99.1
chmod -R a+rX /opt/mq-bridge-rust
