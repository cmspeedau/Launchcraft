# ------------------------------------------------------------------------------
# Build Stage
# ------------------------------------------------------------------------------
FROM rust:1.80-alpine AS builder

RUN apk add --no-cache musl-dev gcc g++ make zstd-dev

WORKDIR /usr/src/launchcraft

COPY Cargo.toml ./
COPY crates ./crates
COPY apps ./apps

WORKDIR /usr/src/launchcraft/crates/launchcraft-builder
RUN cargo build --release

# ------------------------------------------------------------------------------
# Runtime Stage
# ------------------------------------------------------------------------------
FROM alpine:3.20

RUN apk add --no-cache ca-certificates libgcc

COPY --from=builder /usr/src/launchcraft/target/release/launchcraft-builder /usr/local/bin/launchcraft-builder

WORKDIR /workspace

VOLUME ["/workspace/pack", "/workspace/dist"]

ENTRYPOINT ["launchcraft-builder"]
CMD ["build", "--input", "/workspace/pack", "--output", "/workspace/dist"]
