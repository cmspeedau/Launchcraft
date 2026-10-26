# Launchcraft GUI Client

A modern, SKCraft-inspired client-side GUI for Launchcraft, built with **Rust + egui** (via `eframe`).

## Features

- 🎨 Modern tabbed interface (Home / Worlds / Java / Logs) with play button
- 💾 **Client-side world save backups** — one-click backup of all worlds with timestamped snapshots, restore support
- ☕ **Java settings** — Java executable path (with auto-detect via `JAVA_HOME`/PATH), min/max heap memory, extra JVM args; persisted to `settings.json`
- Cross-platform (Windows / macOS / Linux)

## Build & run

```sh
cd gui-client
cargo run --release
```

## Configuration

Settings are stored in `~/.launchcraft/settings.json` (or `%APPDATA%\.launchcraft` on Windows, or set `LAUNCHCRAFT_HOME`).

Closes #28, implements #29.
