# Launchcraft

<div align="center">

[![CI](https://github.com/cmspeedau/Launchcraft/actions/workflows/ci.yml/badge.svg)](https://github.com/cmspeedau/Launchcraft/actions/workflows/ci.yml)
[![Docker](https://img.shields.io/badge/docker-container-blue?logo=docker&logoColor=white)](Dockerfile)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg?logo=rust&logoColor=white)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg)]()

**A high-performance, cross-platform Minecraft launcher and modpack distribution engine built in Rust.**

Modernizing the classic SKCraft launcher philosophy with Content-Addressable Storage (CAS), zstandard delta compression, zero-trust integrity, portable mode, and headless CI/CD pack compilation.

</div>

---

## Overview

Launchcraft is designed for speed, security, and portability. It replaces traditional monolithic archive downloads with an incremental, parallel content-addressable storage engine, cutting update times and bandwidth by up to 90%.

### Key Features

- **Blazing Fast Delta Sync & CAS Engine:**
  - Parallel BLAKE3 hash computation and zstandard (`zstd`) stream de/compression.
  - Hard-linking and deduplicated object store across all local instances.
  - Extraneous file cleanup with user-preserving whitelist (`options.txt`, `saves/`, `screenshots/`).
- **Portable & Isolated by Default:**
  - Automatic **Portable Mode** detection when a `.portable` file or `data/` folder exists next to the binary.
  - Zero system pollution: runtimes, instances, assets, and caches remain inside the portable folder.
- **Save Game Backups & World Cloud Sync:**
  - Automated and on-demand local world directory zipping (`<world>-<timestamp>.zip`) with lock-file exclusion.
  - **Server-Side Policy Control:** Remote servers and pack authors can dynamically enable or disable server backup uploads via pack configuration (`server_enabled`).
  - Integrated file viewer and bidirectional upload/download management for world archives.
- **Automated JRE & Runtime Management:**
  - Automatic host OS and architecture detection (`x86_64`, `aarch64`).
  - Seamless provisioning of Java 8, 17, and 21 from Adoptium / Zulu runtimes.
- **Secure Microsoft & Xbox Authentication:**
  - OAuth 2.0 Device Code and Loopback flow.
  - Xbox Live and XSTS authentication token exchange with secure local credential rotation.
- **Headless Pack Builder CLI (`launchcraft-builder`):**
  - Standalone CLI for pack creators and server operators.
  - Scans modpack source trees, computes CAS blobs, and emits synchronized `manifest.json` distributions ready for S3 / CDN hosting.
- **Dockerized CI/CD Toolchain:**
  - Ready-to-use container for automated modpack building in GitHub Actions or GitLab CI pipelines.

---

## Architecture & Crates

The repository is organized as a Cargo workspace:

```
Launchcraft/
├── apps/
│   └── launchcraft-desktop/     # Tauri v2 desktop shell and user interface
├── crates/
│   ├── launchcraft-core/        # Directory layouts, paths, and instance metadata schemas
│   ├── launchcraft-sync/        # Content-addressable storage (CAS) and delta sync engine
│   ├── launchcraft-runtime/     # JRE provisioning, process spawner, and JVM argument builder
│   ├── launchcraft-auth/        # Microsoft OAuth, Xbox Live, and token management
│   ├── launchcraft-backup/      # World backup zipping engine, server policy, and cloud sync
│   └── launchcraft-builder/     # Headless CLI for scanning and building CAS modpacks
├── Dockerfile                   # Multi-stage container for headless pack compilation
└── ROADMAP.md                   # Detailed architecture and milestone tracking
```

---

## Docker & Container Environment

The headless pack builder (`launchcraft-builder`) is packaged as a minimal, secure multi-stage Docker image based on Alpine Linux.

### Environment & Volume Reference

| Variable / Volume | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `INPUT_DIR` | Flag / Path | `/workspace/pack` | Root directory of the source modpack (containing `mods/`, `config/`, etc.) |
| `OUTPUT_DIR` | Flag / Path | `/workspace/dist` | Destination directory where CAS objects and `manifest.json` are written |
| `/workspace/pack` | Volume Mount | *Required* | Host volume containing raw modpack files to be scanned and compiled |
| `/workspace/dist` | Volume Mount | *Required* | Host volume where the distribution bundle (`manifest.json` + `objects/`) is generated |
| `RUST_LOG` | Environment | `info` | Logging verbosity (`error`, `warn`, `info`, `debug`, `trace`) |
| `COMPRESSION_LEVEL` | Environment / Flag | `3` | Zstandard compression level (1-22) for CAS object blobs |
| `THREADS` | Environment / Flag | `auto` | Number of Rayon worker threads allocated for parallel hashing and compression |

### Running the Docker Builder

Mount your local pack directory and output target into the container:

```bash
# Build the Docker image locally
docker build -t launchcraft-builder .

# Compile a modpack distribution
docker run --rm \
  -v $(pwd)/my-modpack:/workspace/pack:ro \
  -v $(pwd)/dist:/workspace/dist \
  launchcraft-builder build \
    --input /workspace/pack \
    --output /workspace/dist \
    --name "My Server Pack" \
    --version "1.0.0"
```

---

## Quickstart & Local Development

### Prerequisites
- [Rust](https://rustup.rs/) (1.80+)
- `libzstd` development libraries:
  - **Debian / Ubuntu:** `sudo apt-get install libzstd-dev`
  - **Fedora:** `sudo dnf install libzstd-devel`
  - **macOS:** `brew install zstd` (Apple Silicon only)
  - **Windows:** Bundled automatically via Cargo

### Building and Testing

```bash
# Clone the repository
git clone https://github.com/cmspeedau/Launchcraft.git
cd Launchcraft

# Run all workspace unit and integration tests
cargo test --workspace

# Run the headless pack builder CLI
cargo run -p launchcraft-builder -- --help

# Run the desktop application
cargo run -p launchcraft-desktop
```

---

## Roadmap

Detailed development milestones, architecture decisions, and task tracking are maintained in [ROADMAP.md](ROADMAP.md) and [Issue #1](https://github.com/cmspeedau/Launchcraft/issues/1).

---

## License

Licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
