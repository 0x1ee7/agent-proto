# syntax=docker/dockerfile:1
#
# Stages:
#   nats     downloads nats-server 2.15.2 (linux amd64) and verifies its tarball SHA-256
#   dev      toolchain, clippy, rustfmt, python3 and nats-server; the repo is mounted at /work
#   build    release binaries a2ad, a2a and a2abus-mcp-shim
#   runtime  minimal image that runs a2ad (with the pinned nats-server it supervises)
#
# Pins: Rust 1.98.1 matches the Arch Linux toolchain used for parity tests. nats-server 2.15.2 is
# the version the RFC-0010 spikes ran against; the digest is the one in the upstream
# nats-server-v2.15.2 SHA256SUMS file.

ARG RUST_IMAGE=rust:1.98.1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG NATS_VERSION=2.15.2
ARG NATS_TGZ_SHA256=67cb8e10bc6f0c4b47f75a728529a20953dbae2a4f9a4a52ae8517d65ae303ff

FROM ${RUNTIME_IMAGE} AS nats
ARG NATS_VERSION
ARG NATS_TGZ_SHA256
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN curl -sSfL -o /tmp/nats.tar.gz \
      "https://github.com/nats-io/nats-server/releases/download/v${NATS_VERSION}/nats-server-v${NATS_VERSION}-linux-amd64.tar.gz" \
 && echo "${NATS_TGZ_SHA256}  /tmp/nats.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/nats.tar.gz -C /tmp \
 && install -m 0755 "/tmp/nats-server-v${NATS_VERSION}-linux-amd64/nats-server" /nats-server

FROM ${RUST_IMAGE} AS dev
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add clippy rustfmt
COPY --from=nats /nats-server /usr/local/bin/nats-server
ENV CARGO_TARGET_DIR=/cache/target \
    NATS_SERVER_BIN=/usr/local/bin/nats-server \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1
WORKDIR /work
CMD ["bash"]

FROM dev AS build
COPY . /src
WORKDIR /src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cache \
    cargo build --locked --release -p a2ad -p a2a-cli -p a2a-mcp \
 && mkdir -p /out \
 && cp /cache/target/release/a2ad /cache/target/release/a2a /cache/target/release/a2abus-mcp-shim /out/

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 a2a \
 && mkdir -p /state /etc/a2ad \
 && chown a2a:a2a /state
COPY --from=nats /nats-server /usr/local/bin/nats-server
COPY --from=build /out/ /usr/local/bin/
COPY deploy/compose/a2ad.toml /etc/a2ad/a2ad.toml
USER a2a
VOLUME ["/state"]
ENTRYPOINT ["a2ad"]
CMD ["run", "--config", "/etc/a2ad/a2ad.toml"]
