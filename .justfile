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

# Check that the recipes compiling pyo3 have an interpreter to compile against.
#
# Only the chain those recipes actually leave pyo3: $PYO3_PYTHON, then
# $VIRTUAL_ENV, then PATH. $CONDA_PREFIX is deliberately NOT considered — the
# python recipes unset it, for the reasons documented above that group — so
# treating it as a candidate here would reject the very recipe that fixes it.
_require-python:
    #!/usr/bin/env bash
    if [ -n "${PYO3_PYTHON:-}" ]; then
        [ -x "${PYO3_PYTHON}" ] && exit 0
        echo "error: \$PYO3_PYTHON=${PYO3_PYTHON} is not executable." >&2
        exit 1
    fi
    if [ -n "${VIRTUAL_ENV:-}" ]; then
        if [ -x "${VIRTUAL_ENV}/bin/python" ] || [ -x "${VIRTUAL_ENV}/bin/python3" ]; then exit 0; fi
        cat >&2 <<EOF
    error: \$VIRTUAL_ENV=${VIRTUAL_ENV} has no bin/python, and pyo3 prefers it
           over PATH rather than falling through.

      Clear it, or point \$PYO3_PYTHON at a real interpreter:
        unset VIRTUAL_ENV
        PYO3_PYTHON=\$(command -v python3) just <recipe>
    EOF
        exit 1
    fi
    command -v python3 >/dev/null 2>&1 && exit 0
    echo "error: no python3 on PATH, \$PYO3_PYTHON unset and \$VIRTUAL_ENV unset." >&2
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
# Two ways to obtain the C libraries; see "The two build variants" in
# Cargo.toml. They differ only in librdkafka — SQLite is bundled either way,
# and IBM MQ reaches both the same way, via runtime dlopen.

# librdkafka and SQLite compiled from bundled sources, protoc from the vendored
# binary, IBM MQ resolved at runtime via dlopen. Needs nothing installed.
[doc('Self-contained release build (the default variant)')]
[group('build')]
build-static:
    cargo build --release --features full

# Links librdkafka >= 2.12.1 from the environment via pkg-config. What a
# conda-forge recipe or a distro package wants, so the shared library stays
# patchable.
#
# Two prerequisites, from two different places, which is why this recipe is
# more than a cargo line:
#
#   librdkafka   pkg-config. A distro's librdkafka-dev, or
#                `pixi global install librdkafka`.
#   protoc       $PROTOC or PATH — `full-dynamic` drops `vendored-protoc` on
#                purpose.
#
# SQLite is NOT among them: it is bundled either way, so this variant needs
# neither libsqlite3 nor the libclang that generating its bindings would have
# required. See the sqlx dependency in Cargo.toml.
#
# The rpath is derived from pkg-config rather than hard-coded, because the
# shared library generally lives somewhere the loader does not search by
# default — a conda prefix, a Conan package cache — and a binary linked here
# would not start without it. rustc does not read LDFLAGS, hence RUSTFLAGS.
# Appended, so an ambient RUSTFLAGS survives.
[doc('Link librdkafka from the environment')]
[group('build')]
build-dynamic: _require-protoc
    #!/usr/bin/env bash
    set -euo pipefail
    if ! pkg-config --exists rdkafka; then
        echo "error: rdkafka not on PKG_CONFIG_PATH." >&2
        echo "       'just check-native-deps' reports what is missing." >&2
        exit 1
    fi
    for dir in $(pkg-config --libs-only-L rdkafka | tr ' ' '\n' | sed -n 's/^-L//p' | sort -u); do
        RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-rpath,$dir"
    done
    export RUSTFLAGS
    cargo build --release --features full-dynamic

# Reports every prerequisite rather than failing on the first, so it is usable
# as a diagnosis of a broken `build-dynamic`, and tells you how to fix whatever
# is missing. Exits non-zero if anything is.
#
# Advice is accumulated as one array element per output line rather than as
# multi-line strings: a just recipe body has to stay indented, so a string
# containing a column-0 line silently ends the recipe.
#
# Each remediation leads with `just build-static`, because `full` compiles its
# own librdkafka and carries a prebuilt protoc — so wanting `full-dynamic` at
# all is a packaging requirement, and someone who just wants a working build
# should be told the shorter way out first.
[doc('Check the environment can satisfy build-dynamic, and how to fix gaps')]
[group('build')]
check-native-deps:
    #!/usr/bin/env bash
    missing=0
    advice=()
    say() { advice+=("$@"); }

    search_path() {
        if [ -n "${PKG_CONFIG_LIBDIR:-}" ]; then
            echo "PKG_CONFIG_LIBDIR=$PKG_CONFIG_LIBDIR (this replaces the built-in path)"
        elif [ -n "${PKG_CONFIG_PATH:-}" ]; then
            echo "PKG_CONFIG_PATH=$PKG_CONFIG_PATH, then pkg-config's built-in path"
        else
            echo "pkg-config's built-in path only (PKG_CONFIG_PATH is unset)"
        fi
    }

    # An rdkafka.pc that exists but is not on the search path is the common
    # case, not the exceptional one: `pixi global install` and `conda install`
    # both place one under their prefix, and neither puts that prefix on
    # PKG_CONFIG_PATH — pixi global exposes binaries only. Look before telling
    # anyone to install something they already have.
    find_rdkafka_pc() {
        local d
        for d in "${HOME}/.pixi/envs"/*/lib/pkgconfig \
                 "${CONDA_PREFIX:-/nonexistent}/lib/pkgconfig" \
                 .pixi/envs/*/lib/pkgconfig \
                 build/conan \
                 /opt/homebrew/lib/pkgconfig \
                 /usr/local/lib/pkgconfig; do
            # Absolute, always: this path goes into an `export` line the
            # reader will paste, and a relative one only works from the
            # repository root.
            [ -f "$d/rdkafka.pc" ] && (cd "$d" && pwd)
        done
    }

    advise_rdkafka() {
        say "librdkafka $1." ""

        local found
        found=$(find_rdkafka_pc | head -1)
        if [ -n "$found" ]; then
            say "It is already installed — pkg-config just cannot see it. Its" \
                "rdkafka.pc is here:" "" "    $found" "" \
                "so all that is missing is the search path:" "" \
                "    export PKG_CONFIG_PATH=$found\${PKG_CONFIG_PATH:+:\$PKG_CONFIG_PATH}" "" \
                "\`pixi global install\` and \`conda install\` both expose binaries" \
                "rather than libraries, so installing librdkafka does not by itself" \
                "make its .pc discoverable." "" \
                "Currently searching: $(search_path)"
            return
        fi

        say "Only the dynamic variant needs it. If you do not specifically want" \
            "dynamic linkage, build the self-contained variant instead — it" \
            "compiles its own librdkafka and needs nothing installed:" \
            "" "    just build-static" ""
        say "To supply it, any one of:" "" \
            "    Fedora        sudo dnf install librdkafka-devel" \
            "    Debian        sudo apt-get install librdkafka-dev" \
            "    macOS         brew install librdkafka" \
            "    pixi          pixi global install librdkafka" \
            "    conda-forge   the librdkafka package" \
            "    Conan         just conan" ""
        say "then make sure its rdkafka.pc is somewhere pkg-config looks — the" \
            "install alone is often not enough:" "" \
            "    export PKG_CONFIG_PATH=/path/to/lib/pkgconfig:\$PKG_CONFIG_PATH" "" \
            "Currently searching: $(search_path)"
    }

    advise_protoc() {
        say "protoc is not on PATH, and \$PROTOC is unset or not executable." ""
        say "\`full-dynamic\` drops \`vendored-protoc\` deliberately, so the grpc" \
            "build script needs a protoc from the environment:" "" \
            "    Fedora        sudo dnf install protobuf-compiler" \
            "    Debian        sudo apt-get install protobuf-compiler" \
            "    macOS         brew install protobuf" \
            "    pixi          pixi global install libprotobuf" \
            "    conda-forge   the libprotobuf package" \
            "    Windows       choco install protoc" ""
        say "or point \$PROTOC at one you already have:" "" \
            "    export PROTOC=/path/to/protoc" ""
        say "Alternatively \`just build-static\` carries a prebuilt protoc through" \
            "\`vendored-protoc\` and needs none installed."
    }

    for probe in "rdkafka >= 2.12.1"; do
        name="${probe%% *}"
        if version=$(pkg-config --modversion "$name" 2>/dev/null); then
            if pkg-config "$probe"; then
                printf '  %-10s %s\n' "$name" "$version"
            else
                printf '  %-10s %s   TOO OLD, need "%s"\n' "$name" "$version" "$probe"
                advise_rdkafka "is $version, but rdkafka-sys probes for \"$probe\""
                missing=1
            fi
        else
            printf '  %-10s MISSING\n' "$name"
            advise_rdkafka "was not found by pkg-config"
            missing=1
        fi
    done

    if command -v protoc >/dev/null 2>&1; then
        printf '  %-10s %s\n' protoc "$(protoc --version)"
    elif [ -n "${PROTOC:-}" ] && [ -x "${PROTOC}" ]; then
        printf '  %-10s %s (from $PROTOC)\n' protoc "$("$PROTOC" --version)"
    else
        printf '  %-10s MISSING\n' protoc
        advise_protoc
        missing=1
    fi

    if [ "$missing" -eq 0 ]; then
        printf '\n  all prerequisites satisfied; `just build-dynamic` should work\n'
        exit 0
    fi

    printf '\n  ── how to fix ──\n\n'
    printf '  %s\n' "${advice[@]}"
    exit 1

# --- Python bindings ----------------------------------------------------------
#
# These recipes drop $CONDA_PREFIX, which CI never has but a local shell often
# does — `pixi global install` and `conda activate` both export it. It breaks
# the Python build two different ways:
#
#   * pyo3-ffi picks its interpreter from $PYO3_PYTHON, then $VIRTUAL_ENV, then
#     $CONDA_PREFIX, and does not fall through when the one it picked is wrong.
#     A prefix with no bin/python fails the build naming a path you never chose.
#   * maturin refuses outright when $VIRTUAL_ENV and $CONDA_PREFIX are *both*
#     set ("Please unset one of them") — and `uv run` always sets the first.
#     This one bites even when $CONDA_PREFIX is a perfectly good env, so
#     pointing $PYO3_PYTHON somewhere valid does not rescue it.
#
# uv owns the interpreter for this project (python/mq-bridge-py/.venv), so
# $CONDA_PREFIX has no say here and dropping it is the fix for both.

[doc('Sync the dev environment and build the extension into it')]
[group('python')]
py-dev: _require-python
    #!/usr/bin/env bash
    set -euo pipefail
    cd python/mq-bridge-py
    unset CONDA_PREFIX
    uv sync --group dev --no-install-project
    uv run maturin develop

[doc('Run the Python test suite')]
[group('python')]
py-test:
    #!/usr/bin/env bash
    set -euo pipefail
    cd python/mq-bridge-py
    unset CONDA_PREFIX
    uv run pytest -q

# python.yml's regression check that the lean, no-default build still exposes
# the always-on public API. The two cargo steps run outside uv, as they do in
# CI, so they need an interpreter of their own — hence _require-python.
[doc('Lean no-default-features regression tests')]
[group('python')]
py-test-lean: _require-python
    #!/usr/bin/env bash
    set -euo pipefail
    unset CONDA_PREFIX
    cargo test -p mq-bridge-py --no-default-features --features http,rustls-ring test_config_schema_is_always_available
    cargo test -p mq-bridge-py --no-default-features --features http,rustls-ring test_module_init_installs_rustls_provider
    cd python/mq-bridge-py
    uv run maturin develop --no-default-features -F http -F rustls-ring -F pyo3/extension-module
    uv run pytest -q tests/test_public_api.py tests/test_config_types.py

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
