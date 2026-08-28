# --- Builder Stage ---
FROM --platform=$BUILDPLATFORM rust:1.94-bookworm AS builder
ARG TARGETARCH
ARG BUILDPLATFORM
ARG ENABLE_IBM_MQ=false
ARG IBM_MQ_SHA256=faadc63c7a0d7ae24a85cdb0ed5f0669a61c5f70123bbd3cea3c540c77eea855
# Bump DOCKER_CACHE_VERSION in release.yml to invalidate this cache
ARG CACHE_BUST=1

WORKDIR /usr/src/mq-bridge

RUN dpkg --add-architecture arm64 && \
    apt-get update && \
    apt-get install -y \
        pkg-config \
        curl \
        cmake \
        protobuf-compiler \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu \
        gcc \
        libssl-dev:arm64 \
        zlib1g-dev:arm64 \
        libsasl2-dev:arm64 \
        libcurl4-openssl-dev:arm64 \
        libssl-dev \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add aarch64-unknown-linux-gnu

# Write the cmake toolchain file for aarch64 cross-compilation.
RUN cat > /aarch64-toolchain.cmake <<'EOF'
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER   aarch64-linux-gnu-gcc)
set(CMAKE_CXX_COMPILER aarch64-linux-gnu-g++)
set(CMAKE_AR           aarch64-linux-gnu-ar)
# Debian multiarch installs cross-libraries to /usr/lib/aarch64-linux-gnu
# and headers to /usr/include/aarch64-linux-gnu — not under /usr/aarch64-linux-gnu.
# All three paths must be listed so cmake's FindZLIB, FindOpenSSL etc. can
# locate the :arm64 sysroot packages we installed in the apt step.
set(CMAKE_FIND_ROOT_PATH
    /usr/aarch64-linux-gnu
    /usr/lib/aarch64-linux-gnu
    /usr/include/aarch64-linux-gnu
)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
# BOTH instead of ONLY so cmake can also search /usr/include for headers
# that Debian multiarch installs outside the sysroot prefix (e.g. zlib.h)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE BOTH)

# Explicitly pin ZLIB so FindZLIB doesn't search and picks the right arch.
# On Debian multiarch, zlib1g-dev:arm64 puts the library in
# /usr/lib/aarch64-linux-gnu but the header stays in /usr/include.
# CACHE FORCE is required — plain set() in a toolchain file gets ignored
# by FindZLIB which does its own cache population. Without FORCE, cmake
# finds the .so but still reports ZLIB_INCLUDE_DIR as missing.
set(ZLIB_LIBRARY     /usr/lib/aarch64-linux-gnu/libz.so CACHE FILEPATH "" FORCE)
set(ZLIB_INCLUDE_DIR /usr/include                       CACHE PATH     "" FORCE)
EOF

# Generate cargo config.toml conditionally per target platform.
#
# IMPORTANT: [target.X.env] is NOT valid cargo config syntax — cargo only
# supports [env] at the top level. So we write two different config files:
#
# arm64: includes TARGET_CMAKE_TOOLCHAIN_FILE (read by rdkafka-sys build.rs
#        to pass the cmake toolchain to librdkafka) plus ARM64 sysroot paths.
#
# amd64: minimal config, no sysroot overrides, native build works as-is.
#
# The cache mount targets /usr/local/cargo/registry, not /usr/local/cargo,
# so this file is never shadowed during the cargo build step.
#
# Heredocs cannot be used inside if/else in a RUN command — the EOF terminator
# ends the RUN instruction, making the Dockerfile parser see `else` as an
# unknown instruction. Instead we use printf to write both files, then select
# the right one with cp.
RUN mkdir -p /usr/local/cargo && \
    printf '[target.aarch64-unknown-linux-gnu]\nlinker = "aarch64-linux-gnu-gcc"\nar = "aarch64-linux-gnu-ar"\n\n[env]\nTARGET_CMAKE_TOOLCHAIN_FILE = "/aarch64-toolchain.cmake"\nPKG_CONFIG_SYSROOT_DIR = "/usr/aarch64-linux-gnu"\nPKG_CONFIG_PATH = "/usr/lib/aarch64-linux-gnu/pkgconfig"\nPKG_CONFIG_ALLOW_CROSS = "1"\nOPENSSL_INCLUDE_DIR = "/usr/include/aarch64-linux-gnu"\nOPENSSL_LIB_DIR = "/usr/lib/aarch64-linux-gnu"\n' > /cargo-config-arm64.toml && \
    printf '[target.x86_64-unknown-linux-gnu]\nlinker = "gcc"\n' > /cargo-config-amd64.toml && \
    if [ "$TARGETARCH" = "arm64" ]; then \
        cp /cargo-config-arm64.toml /usr/local/cargo/config.toml; \
    else \
        cp /cargo-config-amd64.toml /usr/local/cargo/config.toml; \
    fi

# CC_*/CXX_* are consumed by the `cc` crate, not cargo, so must be env vars
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1

# IBM MQ — only available for AMD64 when explicitly enabled
WORKDIR /opt/mqm
RUN set -e; \
    if [ "$TARGETARCH" = "amd64" ] && [ "$ENABLE_IBM_MQ" = "true" ]; then \
        if [ -z "$IBM_MQ_SHA256" ]; then echo "Error: IBM_MQ_SHA256 is required" >&2; exit 1; fi; \
        curl -fsSL --retry 5 --retry-delay 5 --retry-all-errors --connect-timeout 20 \
            -o /tmp/ibm-mq.tgz https://public.dhe.ibm.com/ibmdl/export/pub/software/websphere/messaging/mqdev/redist/9.4.5.0-IBM-MQC-Redist-LinuxX64.tar.gz \
        && echo "${IBM_MQ_SHA256}  /tmp/ibm-mq.tgz" | sha256sum -c - \
        && tar -xzf /tmp/ibm-mq.tgz \
        && rm /tmp/ibm-mq.tgz; \
    else \
        echo "Skipping IBM MQ installation for $TARGETARCH (enabled=$ENABLE_IBM_MQ)"; \
        mkdir -p /opt/mqm/lib64 /opt/mqm/licenses; \
    fi

ENV MQ_HOME="/opt/mqm"
ENV RUSTFLAGS="-L native=/opt/mqm/lib64"
# librdkafka declares cmake_minimum_required(3.2), which cmake 4 refuses. The
# repo root .cargo/config.toml sets this too, but cargo only finds that config
# from the build WORKDIR upward — setting it here keeps the image build correct
# regardless of where the build is invoked from.
ENV CMAKE_ARGS="-DCMAKE_POLICY_VERSION_MINIMUM=3.5"

# The Docker build context is the monorepo root.
WORKDIR /usr/src/mq-bridge
COPY . .

# DEBUG: run cmake standalone so the full error is visible in GHA logs
# even when cargo's output is truncated. Remove once build is stable.
# RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked     if [ "$TARGETARCH" = "arm64" ]; then         RDKAFKA_SRC=$(find /usr/local/cargo/registry -path "*/rdkafka-sys-*/librdkafka" -type d 2>/dev/null | head -1) &&         echo "rdkafka src: $RDKAFKA_SRC" &&         if [ -n "$RDKAFKA_SRC" ]; then             mkdir -p /tmp/rdkafka-cmake-test && cd /tmp/rdkafka-cmake-test &&             cmake "$RDKAFKA_SRC"                 -DCMAKE_TOOLCHAIN_FILE=/aarch64-toolchain.cmake                 -DRDKAFKA_BUILD_STATIC=1 -DRDKAFKA_BUILD_TESTS=0                 -DRDKAFKA_BUILD_EXAMPLES=0 -DWITH_ZLIB=1                 -DWITH_CURL=0 -DWITH_SSL=0 -DWITH_SASL=0                 -DWITH_ZSTD=0 -DENABLE_LZ4_EXT=0 &&             echo "=== cmake configure OK ===" ||             (echo "=== CMakeError.log ===" &&              cat /tmp/rdkafka-cmake-test/CMakeFiles/CMakeError.log 2>/dev/null &&              exit 1);         else             echo "rdkafka-sys not yet in cargo registry cache, skipping cmake test";         fi;     fi

# Build the application.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/src/mq-bridge/target,id=target-${TARGETARCH}-${CACHE_BUST},sharing=locked \
    if [ "$TARGETARCH" = "amd64" ]; then \
        RUST_TARGET="x86_64-unknown-linux-gnu"; \
        if [ "$ENABLE_IBM_MQ" = "true" ]; then \
            CARGO_FEATURES="--features=ibm-mq"; \
        else \
            CARGO_FEATURES=""; \
        fi; \
    else \
        RUST_TARGET="aarch64-unknown-linux-gnu"; \
        CARGO_FEATURES="--features=arm64-cross-compile"; \
    fi && \
    CARGO_PROFILE_RELEASE_LTO=thin \
    CMAKE_BUILD_PARALLEL_LEVEL=1 \
    cargo build -vv --manifest-path apps/mq-bridge-app/Cargo.toml --target "$RUST_TARGET" --profile release-with-lto $CARGO_FEATURES --jobs 1 -p mq-bridge-app && \
    cp target/$RUST_TARGET/release-with-lto/mq-bridge-app mq-bridge-app && \
    cp target/$RUST_TARGET/release-with-lto/mqb mqb

# Identify and copy only the necessary MQ libraries for the final stage
RUN mkdir /mq-libs && \
    if [ "$TARGETARCH" = "amd64" ] && [ "$ENABLE_IBM_MQ" = "true" ]; then \
        ldd mq-bridge-app | grep '/opt/mqm/lib64/' | awk '{print $3}' | xargs -I {} cp -L {} /mq-libs/; \
    fi

# Stage the correct libz for the final image based on target arch
RUN touch input.log error.log && mkdir /app_placeholder && \
    mkdir /dist_libs && \
    if [ "$TARGETARCH" = "amd64" ]; then \
        cp /usr/lib/x86_64-linux-gnu/libz.so.* /dist_libs/; \
    else \
        cp /usr/lib/aarch64-linux-gnu/libz.so.* /dist_libs/; \
    fi

# --- Final Stage MQ-BRIDGE-APP ---
FROM gcr.io/distroless/cc-debian12:nonroot AS final

# Ownership marker for the MCP Registry: must match `name` in server.json.
LABEL io.modelcontextprotocol.server.name="io.github.marcomq/mq-bridge-app"

COPY --from=builder /usr/src/mq-bridge/mq-bridge-app /usr/local/bin/mq-bridge-app
COPY --from=builder /usr/src/mq-bridge/mqb /usr/local/bin/mqb
COPY --from=builder --chown=nonroot:nonroot /app_placeholder /app
COPY --from=builder --chown=nonroot:nonroot /usr/src/mq-bridge/input.log /app/input.log
COPY --from=builder --chown=nonroot:nonroot /usr/src/mq-bridge/error.log /app/error.log
COPY --from=builder /dist_libs/libz.so.* /lib/

COPY --from=builder /mq-libs /opt/mqm/lib64
COPY --from=builder /opt/mqm/licenses /opt/mqm/licenses

ENV LD_LIBRARY_PATH="/opt/mqm/lib64"
ENV HOST_ADDR=host.docker.internal:3000

COPY apps/mq-bridge-app/dev/config /config

WORKDIR /app

EXPOSE 9090
EXPOSE 9091

ENTRYPOINT ["/usr/local/bin/mq-bridge-app"]

# Metrics bind every interface in here, unlike the loopback default on a host:
# inside a container that is what makes them scrapeable at all, and the container
# boundary is the real gate — nothing is reachable without `-p` or a Service.
#
# This is ENV rather than part of CMD on purpose. A Kubernetes pod almost always
# sets `args:`, which replaces CMD wholesale; ENV survives that, so metrics keep
# working no matter how the command line is overridden.
ENV MQB__METRICS_ADDR=0.0.0.0:9090

# Default arguments for a bare `docker run <image>`, serving the UI that a host
# run would stop and ask about. Docker drops CMD as soon as the caller passes
# any argument of its own, which is exactly what keeps `docker run <image> copy …`
# and `… mcp …` headless. Anything passing its own arguments and still wanting
# the UI asks for it, e.g. `--ui --config /app/x.yml`.
CMD ["--ui"]
