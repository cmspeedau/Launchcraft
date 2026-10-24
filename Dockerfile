# ------------------------------------------------------------------------------
# Build Stage
# ------------------------------------------------------------------------------
# Track the latest stable 1.x toolchain (matches CI's `rust-toolchain@stable`).
# Pinned old toolchains break because Cargo.lock is not committed and
# current dependencies require the `edition2024` cargo feature (Rust >= 1.85).
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libzstd-dev \
    build-essential \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/launchcraft

COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
COPY apps ./apps

RUN cargo build --release -p launchcraft-builder --verbose

# ------------------------------------------------------------------------------
# Runtime Stage
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libzstd1 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/launchcraft/target/release/launchcraft-builder /usr/local/bin/launchcraft-builder

WORKDIR /workspace

VOLUME ["/workspace/pack", "/workspace/dist"]

ENTRYPOINT ["launchcraft-builder"]
CMD ["build", "--input", "/workspace/pack", "--output", "/workspace/dist"]
