# Launchcraft GUI Client

A modern, SKCraft-inspired client-side GUI for Launchcraft, built with **Rust + egui** (via `eframe`).

## Features

- 🎨 Tabbed interface (Home / Worlds / Java / Logs) with play button
- 🌍 **World save backups** — per-instance, per-world zipped backups (`launchcraft-backup::WorldArchiver`), with restore and a local backups folder
- ☁ **Server sync** — fresh or existing archives upload to the instance's backup endpoint (`launchcraft-backup::BackupSyncClient`), honoring the server/pack policy (`server_backups_enabled`); uploads run on a background tokio runtime, never on the UI thread
- ☕ **Java settings** — Java executable path (with auto-detect via `JAVA_HOME`/PATH), min/max heap memory, extra JVM args; persisted to `settings.json`
- Cross-platform (Windows / macOS / Linux)

## Build & run

```sh
cargo run -p launchcraft-gui
```

## Configuration

Instances are read from the launcher data directory resolved by
`launchcraft-core::LauncherPaths` (portable `data/` folder next to the binary,
or the OS standard app-data directory, e.g. `~/Library/Application Support/com.Launchcraft.Launchcraft`
on macOS). Java settings are stored in `<config>/settings.json`, shared with
the desktop app.

Closes #28, implements #29.
