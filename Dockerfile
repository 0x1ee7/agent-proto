# syntax=docker/dockerfile:1
#
# Stages:
#   nats     downloads nats-server 2.15.2 (linux amd64) and verifies its tarball SHA-256
#   dev      toolchain, clippy, rustfmt, python3 and nats-server; the repo is mounted at /work
#   build    release binaries a2ad, a2a, a2a-tui and a2abus-mcp-shim
#   runtime  minimal image that runs a2ad (with the pinned nats-server it supervises)
#   fetch    downloads omp, Node and the Claude Code ACP adapter (checksummed or version-pinned)
#   demo     runtime plus those agents and the stub model servers; no credentials
#
# Pins: Rust 1.98.1 matches the Arch Linux toolchain used for parity tests. nats-server 2.15.2 is
# the version the RFC-0010 spikes ran against; the digest is the one in the upstream
# nats-server-v2.15.2 SHA256SUMS file. omp 18.12.6 is the version the omp ACP spike ran against;
# its digest is the one the GitHub release publishes for omp-linux-x64.

ARG RUST_IMAGE=rust:1.98.1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG NATS_VERSION=2.15.2
ARG NATS_TGZ_SHA256=67cb8e10bc6f0c4b47f75a728529a20953dbae2a4f9a4a52ae8517d65ae303ff
ARG OMP_VERSION=18.12.6
ARG OMP_SHA256=85d9daf3a05fc76e8cd425ae1904eb576af8d21cc85fcf4690742626e07ccfa4
ARG NODE_VERSION=24.22.1
ARG NODE_TXZ_SHA256=6c172a9ab6718f7b801fb5a7bd486af2ee5e32758086a04b84fcce2aa77095e6

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
    cargo build --locked --release -p a2ad -p a2a-cli -p a2a-tui -p a2a-mcp \
 && mkdir -p /out \
 && cp /cache/target/release/a2ad /cache/target/release/a2a /cache/target/release/a2a-tui /cache/target/release/a2abus-mcp-shim /out/

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

# Demo stages. Nothing downloaded here is a credential: the adapter and omp are the unmodified
# upstream programs, and the demo profiles point both at local stub model servers.

FROM ${RUNTIME_IMAGE} AS fetch
ARG OMP_VERSION
ARG OMP_SHA256
ARG NODE_VERSION
ARG NODE_TXZ_SHA256
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /out \
 && curl -sSfL -o /out/omp \
      "https://github.com/can1357/oh-my-pi/releases/download/v${OMP_VERSION}/omp-linux-x64" \
 && echo "${OMP_SHA256}  /out/omp" | sha256sum -c - \
 && chmod 0755 /out/omp
RUN curl -sSfL -o /tmp/node.tar.xz \
      "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
 && echo "${NODE_TXZ_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
 && mkdir /tmp/node \
 && tar -xJf /tmp/node.tar.xz -C /tmp/node --strip-components=1
COPY deploy/compose/demo/package.json /out/claude-acp/package.json
RUN PATH="/tmp/node/bin:$PATH" npm install --prefix /out/claude-acp --omit=dev --no-audit --no-fund \
 && install -m 0755 /tmp/node/bin/node /out/node

FROM runtime AS demo
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 git \
 && rm -rf /var/lib/apt/lists/*
COPY --from=fetch /out/omp /usr/local/bin/omp
COPY --from=fetch /out/node /usr/local/bin/node
COPY --from=fetch /out/claude-acp /opt/claude-acp
COPY spikes/omp-acp/stub/stub_openai.py spikes/claude-acp/stub_anthropic.py spikes/claude-acp/serve_stub.py /opt/a2a-demo/stubs/
COPY deploy/compose/a2ad.demo.toml /etc/a2ad/a2ad.demo.toml
COPY --chmod=0755 deploy/compose/demo/follow-logs.sh /usr/local/bin/follow-logs
COPY --chmod=0755 deploy/compose/demo/demo-tui.sh /usr/local/bin/demo-tui
COPY deploy/compose/demo/a2a-cli.toml /etc/a2ad/a2a-cli.toml
COPY --chmod=0755 deploy/compose/demo/demo-a2a.sh /usr/local/bin/demo-a2a
COPY --chown=a2a:a2a deploy/compose/demo/omp-agent /home/a2a/demo/omp-agent
COPY --chown=a2a:a2a deploy/compose/demo/claude-cfg /home/a2a/demo/claude-cfg
USER a2a
# Task fixture: a git repository the agents work in (workspaces are cloned from it).
RUN mkdir -p /home/a2a/demo/stub-logs/omp /home/a2a/demo/claude-work /home/a2a/repo \
 && cd /home/a2a/repo \
 && git init -q -b main \
 && printf 'A2A-Bus demo repository.\n' > README.txt \
 && git add README.txt \
 && git -c user.name=demo -c user.email=demo@example.invalid commit -q -m "Initial commit"
CMD ["run", "--config", "/etc/a2ad/a2ad.demo.toml"]
