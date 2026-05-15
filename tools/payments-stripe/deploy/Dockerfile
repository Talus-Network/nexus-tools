# syntax=docker/dockerfile:1.7

# ---- Builder ----------------------------------------------------------------
FROM rust:1.83-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*

# Copy the whole workspace — `members = ["tools/*"]` requires every
# sibling crate to exist for cargo's resolver.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY tools tools
COPY helpers helpers

RUN cargo +stable build --release --package payments-stripe

# ---- Runtime ----------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app

COPY --from=builder /build/target/release/payments-stripe /app/payments-stripe

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

USER nonroot
ENTRYPOINT ["/app/payments-stripe"]
