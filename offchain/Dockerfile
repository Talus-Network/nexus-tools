# syntax=docker/dockerfile:1
# Shared Dockerfile for all offchain Nexus tools.
#
# Builds a single tool binary identified by BINARY and TOOL_DIR build args
# (TOOL_DIR is the tool's path relative to the build context, e.g.
# "offchain/tools/http"), embeds TOOL_FQN_VERSION (computed by CI from the
# tool's subtree hash) so the tool's FQN strings carry the content-addressed
# version.
#
# Building via --manifest-path scopes cargo to the workspace member's
# Cargo.toml, sidestepping ambiguity when a tool crate's name collides with
# a transitive crates.io dep of the same name (e.g. our `http` tool vs the
# `http` crate pulled in by reqwest).

FROM rust:1.93.1-slim-bookworm AS builder

ARG BINARY
ARG TOOL_DIR
ARG TOOL_FQN_VERSION=1

# Surfaced as a cargo env var via build.rs -> env!("TOOL_FQN_VERSION").
ENV TOOL_FQN_VERSION=${TOOL_FQN_VERSION}

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the entire offchain workspace. Cargo's incremental build + the
# buildkit cache mounts keep this fast.
COPY offchain/ ./offchain/

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/offchain/target,sharing=locked \
    set -e && \
    cargo build --locked --profile release \
        --manifest-path "${TOOL_DIR}/Cargo.toml" \
        --bin "${BINARY}" && \
    cp "offchain/target/release/${BINARY}" "/app/${BINARY}"

# ── Collect runtime libs for the distroless stage ─────────────────────
FROM debian:bookworm-slim AS lib-collector

ARG BINARY
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/${BINARY} /tmp/binary

RUN set -e; \
    case "${TARGETARCH}" in \
        "amd64") LIB_DIR="/lib/x86_64-linux-gnu"; LINKER="/lib64/ld-linux-x86-64.so.2" ;; \
        "arm64") LIB_DIR="/lib/aarch64-linux-gnu"; LINKER="/lib/ld-linux-aarch64.so.1" ;; \
        *) echo "Unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    mkdir -p "/collected${LIB_DIR}" /collected/lib64 && \
    ldd /tmp/binary | awk '/=>/ { print $3 }' | while read -r lib; do \
        if [ -n "$lib" ] && [ -f "$lib" ]; then \
            cp -L "$lib" "/collected${LIB_DIR}/"; \
        fi \
    done && \
    cp -L "${LINKER}" "/collected${LINKER}"

# ── Final distroless image ───────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

ARG BINARY

COPY --from=builder /app/${BINARY} /usr/local/bin/${BINARY}
COPY --from=lib-collector /collected/ /

ENV PORT=8080
EXPOSE 8080
