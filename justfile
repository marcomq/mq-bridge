# Prescribed actions for mq-bridge. Each recipe is the command
# .github/workflows/ runs, so a green `just ci` means the PR gate has already
# been answered locally. When a workflow changes, change it here too.
#
# Two cargo workspaces live in this repository: the engine at the root, and the
# application under apps/mq-bridge-app with its own Cargo.lock. Recipes for the
# latter are prefixed `app-` and run from inside it.
#
# `just` on its own lists everything, grouped.

# The feature set for every lint and doc build. NOT `--all-features`: that
# enables `link-static` and `link-dynamic` together, which src/lib.rs rejects
# with a compile_error!. See the `lint-all` comment in Cargo.toml.
lint_features := "lint-all"

_default:
    @just --list

# Fail early, with instructions, for the recipes that need a protoc the build
# does not supply itself.
#
# The engine's own `grpc` needs none: `full` and `lint-all` include
# `vendored-protoc`, which hands its build script a prebuilt binary. Two things
# are not covered by that. The app workspace depends on `pulsar`, whose build
# script calls protoc and vendors nothing — `std::env::set_var("PROTOC", …)` in
# the engine's build script sets it only in that process, not in a sibling's.
# And `full-dynamic` deliberately drops `vendored-protoc`, because a distro or
# conda-forge build has to compile against the protobuf it packages.
_require-protoc:
    #!/usr/bin/env bash
    if [ -n "${PROTOC:-}" ] && [ -x "${PROTOC:-}" ]; then exit 0; fi
    if command -v protoc >/dev/null 2>&1; then exit 0; fi
    cat >&2 <<'EOF'
    error: protoc not found, and this target does not vendor one.

      Fedora        sudo dnf install protobuf-compiler
      Debian        sudo apt-get install protobuf-compiler
      macOS         brew install protobuf
      conda-forge   <env>/bin/protoc, from the libprotobuf package
      Windows       choco install protoc

    Or point $PROTOC at a binary you already have.
    EOF
    exit 1

# --- Gates --------------------------------------------------------------------

[doc('Everything ci.yml gates a PR on, bar the Docker suites')]
[group('gate')]
ci: fmt-check lint config-compat check-features test test-no-docker doc

[doc('Format the workspace')]
[group('gate')]
fmt:
    cargo fmt --all

[doc('Fail on unformatted code')]
[group('gate')]
fmt-check:
    cargo fmt --all -- --check

[doc('Clippy as CI does, warnings denied')]
[group('gate')]
lint:
    cargo clippy --all-targets --features {{ lint_features }} -- -D warnings

[doc('Build rustdoc, warnings denied')]
[group('gate')]
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --features {{ lint_features }} --no-deps

# Not a workspace default member, so nothing else builds it. It pins the
# exhaustive `GrpcConfig` literal a downstream crate would write.
[doc('Check the downstream config-compat fixture')]
[group('gate')]
config-compat:
    cargo check -p grpc-config-compat

# `grpc` carries `vendored-protoc` because protoc no longer arrives with the
# feature itself and a bare checkout may have none installed; every `full*` set
# already includes it.
[doc('cargo check each feature subset CI covers')]
[group('gate')]
check-features:
    #!/usr/bin/env bash
    set -uo pipefail
    failed=""
    for f in "" full kafka nats grpc,vendored-protoc mqtt mongodb http; do
        echo "::: cargo check --features ${f:-<default>}"
        if [ -z "$f" ]; then
            cargo check --all-targets || failed="$failed <default>"
        else
            cargo check --all-targets --features "$f" || failed="$failed $f"
        fi
    done
    if [ -n "$failed" ]; then echo "FAILED:$failed" >&2; exit 1; fi

# A hard gate in CI: an unapproved license, a banned crate or a non-crates.io
# source is always actionable here.
[doc('Licenses, bans and sources')]
[group('gate')]
deny:
    cargo deny check bans licenses sources

# Informational in CI, because every current advisory is in a transitive
# dependency we cannot upgrade ourselves. deny.toml carries the reasoning.
[doc('RustSec advisories (never fails)')]
[group('gate')]
deny-advisories:
    -cargo deny check advisories

# --- Tests --------------------------------------------------------------------

[doc('Unit tests, full features')]
[group('test')]
test:
    cargo test --lib --features full

[doc('Integration tests that need no Docker services')]
[group('test')]
test-no-docker:
    cargo test --test ref_test --test sqlite_test --test tls_example --test websocket_test --features full

# #[ignore]d, so it has to be named explicitly.
[doc('Route commit-task JoinSet leak soak')]
[group('test')]
test-memory-leak:
    cargo test --test memory_leak_test --features full -- --ignored --nocapture

# Compiled once into an archive, as ci.yml does, so the filtered runs need no
# recompilation.
[doc('Build the nextest archive for the Docker suites')]
[group('test')]
integration-archive:
    cargo nextest archive --release --features full,test-utils \
        --test integration_test --test armature_integration \
        --archive-file nextest-archive.tar.zst

# Needs a running Docker daemon, the archive above, and the TLS fixtures from
# `just integration-certs`. Narrow it with a nextest filter, e.g.
# `just integration 'binary(integration_test) & test(=test_all_status)'`.
[doc('Run the Docker suites from the archive; takes a nextest filter')]
[group('test')]
integration filter='all()':
    cargo nextest run --archive-file nextest-archive.tar.zst \
        --run-ignored all --no-capture -E '{{ filter }}'

[doc('Generate the TLS fixtures the integration services need')]
[group('test')]
integration-certs:
    #!/usr/bin/env bash
    set -euo pipefail
    chmod +x tests/integration/scripts/gen_certs.sh
    for svc in mongodb kafka ibm-mq; do ./tests/integration/scripts/gen_certs.sh "$svc"; done

# --- Build variants -----------------------------------------------------------
#
# Three ways to obtain the C libraries; see "The three build variants" in
# Cargo.toml. They differ only in librdkafka, libsqlite and libmqm_r.

# librdkafka and SQLite compiled from bundled sources, protoc from the vendored
# binary, IBM MQ resolved at runtime via dlopen. Needs nothing installed.
[doc('Self-contained release build (the default variant)')]
[group('build')]
build-static:
    cargo build --release --features full

# libmqm_r is bound at link time, so the IBM MQ client must be present at BUILD
# time (MQ_INSTALLATION_PATH, default /opt/mqm) and the binary will not start
# without it. See python/mq-bridge-py/examples/IBM_MQ.md.
[doc('Self-contained, but libmqm_r bound at link time')]
[group('build')]
build-static-ibm-mq:
    cargo build --release --features full-static-ibm-mq

# Links librdkafka >= 2.12.1 and libsqlite3 >= 3.34.1 from the environment via
# pkg-config, and takes protoc from $PROTOC or PATH. What a conda-forge recipe
# or a distro package wants, so the shared libraries stay patchable. Also needs
# libclang, for the bindgen-generated SQLite bindings.
[doc('Link librdkafka and libsqlite from the environment')]
[group('build')]
build-dynamic: _require-protoc
    cargo build --release --features full-dynamic

[doc('Check the environment can satisfy build-dynamic')]
[group('build')]
check-native-deps:
    pkg-config --modversion rdkafka sqlite3
    protoc --version

# --- Python bindings ----------------------------------------------------------

[doc('Sync the dev environment and build the extension into it')]
[group('python')]
py-dev:
    cd python/mq-bridge-py && uv sync --group dev --no-install-project && uv run maturin develop

[doc('Run the Python test suite')]
[group('python')]
py-test:
    cd python/mq-bridge-py && uv run pytest -q

# python.yml's regression check that the lean, no-default build still exposes
# the always-on public API.
[doc('Lean no-default-features regression tests')]
[group('python')]
py-test-lean:
    cargo test -p mq-bridge-py --no-default-features --features http,rustls-ring test_config_schema_is_always_available
    cargo test -p mq-bridge-py --no-default-features --features http,rustls-ring test_module_init_installs_rustls_provider
    cd python/mq-bridge-py && uv run maturin develop --no-default-features -F http -F rustls-ring -F pyo3/extension-module \
        && uv run pytest -q tests/test_public_api.py tests/test_config_types.py

# --- Application (separate workspace) -----------------------------------------

[doc('Everything app.yml gates the app crates on')]
[group('app')]
app-ci: app-check app-lint app-test

[doc('cargo check the app crates')]
[group('app')]
app-check: _require-protoc
    cd apps/mq-bridge-app && cargo check -p mq-bridge-app-core -p mq-bridge-app --all-targets

[doc('Clippy the app crates, warnings denied')]
[group('app')]
app-lint: _require-protoc
    cd apps/mq-bridge-app && cargo clippy -p mq-bridge-app-core -p mq-bridge-app --all-targets -- -D warnings

[doc('Unit and bin tests for the app crates')]
[group('app')]
app-test: _require-protoc
    cd apps/mq-bridge-app && cargo test -p mq-bridge-app-core -p mq-bridge-app --lib --bins

# Reaches the engine's `link-dynamic` through the passthroughs in crates/core
# and crates/cli. `--no-default-features` is required: `default = ["full"]`
# would otherwise be on too and collide with it.
[doc('The app, linked against the environment libraries')]
[group('app')]
app-build-dynamic: _require-protoc
    cd apps/mq-bridge-app && cargo build --release -p mq-bridge-app \
        --no-default-features --features full-dynamic

# --- Node bindings ------------------------------------------------------------

[doc('cargo check the node bindings as node.yml does')]
[group('node')]
node-check:
    cargo check -p mq-bridge-node --no-default-features --features "http middleware schema"

# --- Release chores -----------------------------------------------------------

# Everything downstream of the root [workspace.package] version is rewritten
# from it. Pass the new version, or none to re-sync.
[doc('Set the version everywhere')]
[group('release')]
version new='':
    node scripts/sync-version.mjs {{ new }}

[doc('Fail if any committed copy of the version has drifted')]
[group('release')]
version-check:
    node scripts/sync-version.mjs --check
