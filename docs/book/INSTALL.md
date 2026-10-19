# Install

mq-bridge ships in three forms that share one engine and one config format: the
**library** (`mq-bridge` for Rust, Python, and Node.js), and the two forms of the
**application** `mq-bridge-app` — the **CLI / server** and the **desktop app (UI)**. Pick the
install path for the form you need. To compile any of them yourself, see
[Building from source](BUILD.md).

**Library in one line:** `cargo add mq-bridge` · `pip install mq-bridge` ·
`npm install mq-bridge` — details in [Library](#library).

- [CLI / server](#cli--server) — Homebrew, Conda, `cargo binstall`, `cargo install`, Docker,
  and the [agent skill](#agent-skill)
- [Desktop app (UI)](#desktop-app-ui) — Homebrew cask, or a prebuilt bundle
- [Library](#library) — Rust, Python, Node.js

---

## CLI / server

The CLI is a single headless binary, installed under two names: `mq-bridge-app`
and the short **`mqb`**, which is what the docs use and what you'll normally
type. `mqb` is a small launcher that hands straight over to the real binary, so
the two are interchangeable — existing scripts and MCP registrations keep
working untouched.

### Homebrew (macOS, Linux) — recommended

```bash
brew install marcomq/tap/mq-bridge-app
brew upgrade mq-bridge-app     # later, to update
```

Prebuilt bottles cover **Apple Silicon macOS** and **`x86_64` Linux**. Homebrew
refreshes the tap only on `brew update`, so if `brew upgrade` doesn't pick up a
new release yet, run `brew update` first.

### Conda (Windows, macOS, Linux)

Conda is the package-manager install path for Windows and is also available on
macOS and Linux. It installs both `mq-bridge-app` and `mqb`:

```bash
conda install -c marcomq -c conda-forge mq-bridge-app
```

The `marcomq` channel carries mq-bridge itself; `conda-forge` supplies its build
and runtime dependencies.

### `cargo binstall` — prebuilt binary

Downloads the prebuilt CLI from [Releases](https://github.com/marcomq/mq-bridge/releases)
instead of compiling from source, so it installs in seconds:

```bash
cargo binstall mq-bridge-app
```

Prebuilt binaries are available for `x86_64` Linux, Apple Silicon macOS, and
`x86_64` Windows. ([`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall)
is a drop-in `cargo install` replacement.)

### `cargo install` — from source

Requires a Rust toolchain and compiles all supported endpoint client libraries
(except IBM MQ), so it may take a while:

```bash
cargo install mq-bridge-app
```

For IBM MQ, install the client library first and build with `--features=ibm-mq`.

### Docker

The CLI is published as a multi-arch image (`amd64` + `arm64`):

```bash
docker run --rm --name mq-bridge -p 9091:9091 ghcr.io/marcomq/mq-bridge-app:latest
```

To read+tail from `input.log` and forward its content, mount the working directory at
`/app` and seed the config from one of the templates baked into the image at `/config`:

```bash
touch input.log
docker run --rm --name mq-bridge -p 9091:9091 -v "$(pwd)":/app \
  ghcr.io/marcomq/mq-bridge-app:latest --ui --init-config=/config/file-to-http.yml
```

> [!NOTE]
> The default `latest` image is a plain multi-arch image for `amd64` and `arm64`.
> IBM MQ support is published separately as the `latest-ibm-mq` and `ibm-mq` tags
> on `amd64` only, since there is no redistributable IBM MQ client library for
> arm64 yet. Start that image in emulation mode with `--platform=linux/amd64`, or
> build `mq-bridge-app` yourself with `cargo build --release --features=ibm-mq`.

### Agent skill

If a coding agent drives `mqb` for you, give it the
[mq-bridge skill](https://github.com/marcomq/mq-bridge/tree/main/apps/mq-bridge-app/agent-skills):
how to look up params with `mqb describe`, write a `copy` or a config file, and avoid the
common traps. It works with Claude Code, Codex, Cursor and other agents that read `SKILL.md`:

```bash
npx skills add marcomq/mq-bridge
```

---

## Desktop app (UI)

The desktop app is a Tauri bundle of the full messaging workbench — the **same UI**
the CLI serves in a browser, only packaged as a native app.

### Homebrew cask (macOS, Apple Silicon)

```bash
brew install --cask marcomq/tap/mq-bridge
```

Homebrew quarantines cask apps by default and the desktop binaries are not
notarized yet, so macOS blocks the app on first launch. See the note below for how
to open it (via System Settings or Terminal).

### Prebuilt bundles (macOS, Windows, Linux)

Bundles for every platform are attached to each release on the
[GitHub Releases page](https://github.com/marcomq/mq-bridge/releases).

- **macOS** — download the `.dmg` / `.app` bundle.
- **Windows** — download the installer or standalone executable.
- **Linux** — download the bundle that suits your distribution: AppImage, `.deb`,
  `.rpm`, or the unpacked archive.

> [!NOTE]
> The desktop binaries are currently **not notarized**, so on macOS Gatekeeper
> blocks the app on first launch — it's reported as "damaged" or "cannot be opened
> because the developer cannot be verified". This applies to **both** the Homebrew
> cask and a downloaded bundle. There are two ways to open it:
>
> **Option 1 — System Settings (no Terminal).** Try to open the app once so the
> block is triggered, then go to **System Settings → Privacy & Security**, scroll to
> the Security section, and click **"Open Anyway"** next to the mq-bridge message.
> Authenticate, then launch the app again.
>
> **Option 2 — Terminal.** Remove the quarantine attribute directly:
> ```bash
> # app in /Applications (where the cask and the .dmg install it)
> sudo xattr -rd com.apple.quarantine /Applications/mq-bridge.app
> # app in a user-owned directory (e.g. ~/Downloads) — no sudo needed
> xattr -rd com.apple.quarantine ~/Downloads/mq-bridge.app
> ```
>
> If macOS says the app is "damaged", the "Open Anyway" button may not appear — use
> the Terminal method in that case.

---

## Library

Embed the core engine in your own code — produce or consume messages with a unified
API, one config format across all three bindings:

- **Rust** — [`mq-bridge`](https://github.com/marcomq/mq-bridge) (`cargo add mq-bridge`)
- **Python** — [`pip install mq-bridge`](https://pypi.org/project/mq-bridge/)
- **Node.js** — [`npm install mq-bridge`](https://www.npmjs.com/package/mq-bridge)

The core of the library are the `MessageConsumer` and `MessagePublisher` traits,
found in `mq_bridge::traits`.

---

## Build from source

Building the CLI/server, the desktop (Tauri) app, or the Docker image from source
is covered in [BUILD.md](BUILD.md). For IBM MQ specifically, see the
[IBM MQ Setup Guide](IBM_MQ_SETUP.md).
