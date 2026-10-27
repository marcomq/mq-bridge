# mq-bridge-app with the Redpanda Connect and faucet plugins from their release archives.
FROM ubuntu:24.04 AS plugins
ARG TARGETARCH
ARG CONNECT_VERSION=0.2.2
ARG FAUCET_VERSION=0.1.1
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN set -e; \
    case "$TARGETARCH" in amd64) T=x86_64-unknown-linux-gnu ;; arm64) T=aarch64-unknown-linux-gnu ;; esac; \
    mkdir -p /plugins/connect /plugins/faucet; \
    curl -fsSL "https://github.com/marcomq/mq-bridge-connect/releases/download/v$CONNECT_VERSION/mq-bridge-connect-$CONNECT_VERSION-$T.tar.gz" \
      | tar xz --no-same-owner --strip-components=1 -C /plugins/connect; \
    curl -fsSL "https://github.com/marcomq/mq-bridge-faucet/releases/download/$FAUCET_VERSION/mq-bridge-faucet-$FAUCET_VERSION-$T.tar.gz" \
      | tar xz --no-same-owner -C /plugins/faucet

# faucet's arm64 library needs glibc 2.39; the official image (Debian 12) has 2.36.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libsasl2-2 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir /app && chown 65532:65532 /app
COPY --from=ghcr.io/marcomq/mq-bridge-app:1.2.0 /usr/local/bin/mq-bridge-app /usr/local/bin/
# Discovery loads only root-owned libraries (hence --no-same-owner above).
COPY --from=plugins /plugins/connect/*.so /plugins/faucet/*.so /usr/local/lib/mq-bridge/
COPY --from=plugins /plugins/connect/THIRD_PARTY_NOTICES /plugins/connect/LICENSE-* /usr/share/licenses/mq-bridge-connect/
COPY --from=plugins /plugins/faucet/LICENSE-* /usr/share/licenses/mq-bridge-faucet/
USER 65532
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/mq-bridge-app"]
