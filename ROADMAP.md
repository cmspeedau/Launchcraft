# Launchcraft Roadmap & Architecture Specification

A modern, high-performance, cross-platform Minecraft launcher built in **Rust** with a **Tauri v2** frontend. Inspired by SKCraft Launcher, modernized with modern content-addressable storage (CAS), zstandard delta compression, zero-trust integrity, and native multi-platform support.

---

## 1. Vision & Architecture Philosophy

Launchcraft modernizes the SKCraft philosophy into a fast, memory-safe, portable desktop client and headless pack-building toolchain.

### Core Stack
- **Launcher Core & Engine:** Rust (`tokio`, `reqwest`, `blake3`, `zstd`, `directories`, `zip`)
- **Desktop UI Framework:** Tauri v2 + lightweight web frontend (TypeScript/Tailwind/Solid or Svelte)
- **Modpack Distribution:** HTTP/S3/CDN Content-Addressable Storage (CAS)
- **Pack Builder:** Headless Rust CLI (`launchcraft-builder`) packaged for standalone usage and Docker CI
- **World & Save Game Management:** `launchcraft-backup` with server-side enablement control, ZIP streaming, and world management

### Target Platforms
- **Linux:** AppImage (standalone x86_64, aarch64)
- **Windows:** Portable ZIP (self-contained with `.portable` trigger) & NSIS installer
- **macOS:** Apple Silicon (`aarch64-apple-darwin`) & Intel (`x86_64-apple-darwin`) DMG / `.app` bundle

---

## 2. Directory Layout & Portability Strategy

Launchcraft features an auto-detecting **Portable Mode**:

```
Portable Mode Trigger:
  If a folder named `data/` or a file named `.portable` exists adjacent to the launcher binary:
    - Root = <executable_dir>/data (or <executable_dir>)
    - All instances, JREs, configs, and caches remain inside this directory.

Installed Mode (Standard OS defaults):
  - Linux:   ~/.config/launchcraft (config) & ~/.local/share/launchcraft (data)
  - Windows: %APPDATA%\launchcraft
  - macOS:   ~/Library/Application Support/launchcraft
```

### Filesystem Structure
```
<root_data_dir>/
├── config.json               # Launcher global preferences
├── accounts.json             # Encrypted/secured auth tokens
├── cas/                      # Content-addressable object store
│   └── objects/              # Sharded BLAKE3 blobs (aa/bb/aabb...)
├── runtimes/                 # Managed Java environments (Adoptium / Zulu)
│   ├── java-17/
│   └── java-21/
└── instances/                # Modpack instances
    ├── server-pack-1/
    │   ├── instance.json     # Local instance configuration
    │   ├── manifest.json     # Sync state manifest
    │   ├── backups/          # Compressed world ZIP backups
    │   └── minecraft/        # Game working directory
    │       └── saves/        # Singleplayer world saves
    └── custom-pack/
```

---

## 3. Detailed Milestone Roadmap

```
Phase 1: Workspace & Core Foundation (Done)
   │
   ├── Phase 2: Sync Engine & Content-Addressable Storage (CAS) (Done)
   │
   ├── Phase 3: JRE Management & Minecraft Process Launching (Done)
   │
   ├── Phase 4: Authentication & Security (Microsoft OAuth / OS Keyring) (Done)
   │
   ├── Phase 5: Tauri v2 Desktop Interface & Portable Distribution (Done)
   │
   ├── Phase 6: Headless Pack Builder & Docker Image (Done)
   │
   └── Phase 7: Save Game Backups & World Cloud Sync (Done)
         │
         └── Phase 8: Server Console `launchcraft-server` (Done)
```

---

### Phase 1: Workspace Foundation & Core Architecture (Complete)
- [x] Configure Cargo workspace (`launchcraft-core`, `crates/*`, `apps/*`).
- [x] Implement cross-platform directory resolution (`LauncherPaths`) supporting portable mode and OS conventions.
- [x] Design instance, repository, and manifest schemas (`instance.rs`).

---

### Phase 2: Sync Engine & Content-Addressable Storage (CAS) (Complete)
*Objective: Replace old-school ZIP/file re-downloads with fast, incremental, parallel BLAKE3 delta sync.*

- [x] **Content-Addressable Storage (CAS):**
  - Download blobs indexed by BLAKE3 hashes.
  - Decompress `zstd` compressed stream payloads on the fly.
  - Implement hard-linking / fast file copying from CAS store to instance `minecraft/` folders to save disk space across multiple instances.
- [x] **Incremental Fast Delta Sync:**
  - Read local directory manifest; compute BLAKE3 hashes in parallel using Rayon or Tokio tasks.
  - Diff local state against remote pack manifest:
    - Unchanged files: skip.
    - Added / modified: queue parallel download from CDN/CAS.
    - Extraneous files: remove (respecting user-whitelisted files like `saves/`, `screenshots/`, `options.txt`, local keybinds).
- [x] **Dynamic Repository Discovery:**
  - Query remote repository endpoints via HTTP ETag / version hashes.
  - Support auto-updating modpack definitions without requiring client updates.

---

### Phase 3: JRE Provisioning & Process Execution (Complete)
*Objective: Reliable, sandbox-aware Minecraft execution across Windows, macOS, and Linux.*

- [x] **Runtime Manager (`launchcraft-runtime`):**
  - Automatically detect host OS (`windows`, `macos`, `linux`) and architecture (`x86_64`, `aarch64`).
  - Query and download correct JRE distributions (Java 8, 17, 21) from Adoptium / Zulu APIs.
  - Portable extraction: isolate JREs inside `<root_data_dir>/runtimes/`.
- [x] **Minecraft Version Metadata & Library Resolution:**
  - Parse Mojang Version Manifest (`v2`) and Fabric/Forge/NeoForge loaders.
  - Download official Mojang assets and client libraries with parallel hash verification.
- [x] **Game Process Spawner:**
  - Build classpath and JVM arguments (memory allocation, GC tuning flags, natives).
  - Platform-specific hooks:
    - **macOS:** `-XstartOnFirstThread`, Dock icon integration, App Nap disabling.
    - **Windows:** Process creation flags without detached zombie consoles (`javaw.exe`).
    - **Linux:** Native Wayland / GLFW handling options.
  - Pipe game standard output / standard error to launcher log stream in real time.

---

### Phase 4: Microsoft Authentication & Security (Complete)
*Objective: Secure, frictionless OAuth flow conforming to official Mojang standards.*

- [x] **Microsoft OAuth Device Flow & Authorization Code Flow:**
  - Interactive login via browser redirect / local loopback HTTP server.
  - Xbox Live authentication (`user.auth.xboxlive.com`).
  - XSTS token exchange and Minecraft services authentication (`api.minecraftservices.com`).
  - Profile verification and UUID / skin lookup.
- [x] **Secure Token Storage:**
  - Portable cache and token manager for `<root_data_dir>/accounts.json`.
  - Account lifecycle tracking, expiration checks, and token rotation handling.

---

### Phase 5: Tauri v2 Desktop Interface & Cross-Platform Packaging (Complete)
*Objective: Modern, responsive, lightweight GUI distributed across all target operating systems.*

- [x] **Tauri v2 Shell (`apps/launchcraft-desktop`):**
  - IPC commands binding to `launchcraft-core`, `launchcraft-sync`, and `launchcraft-runtime`.
  - Real-time download progress events (bytes transferred, download speed, hash progress).
- [x] **User Interface Features:**
  - Modpack browser (remote synced packs + local custom instances).
  - Instance settings (RAM allocation, custom Java paths, JVM arguments, window dimensions).
  - Real-time game log viewer with search, filtering, and export.
- [x] **Cross-Platform Bundles:**
  - **Linux:** AppImage bundle.
  - **Windows:** Standalone portable `.zip` (includes `.portable` marker) and single-user NSIS `.exe`.
  - **macOS:** Universal `.dmg` (Apple Silicon & Intel) with notarization readiness.

---

### Phase 6: Headless Pack Builder (`launchcraft-builder`) & Docker (Complete)
*Objective: Automated CI/CD toolchain for server admins to compile modpacks.*

- [x] **Headless CLI (`crates/launchcraft-builder`):**
  - Scan directory of mods, configs, and overrides.
  - Calculate BLAKE3 hashes and compress assets into zstd CAS blobs.
  - Generate `manifest.json` and export static directory distribution ready for web hosting / S3 / CDNs.
- [x] **Docker Container:**
  - Multi-stage container for standalone execution and GitHub Actions / GitLab CI pipelines.
  - Single-command pack compilation: `docker run -v ./pack:/pack launchcraft-builder build`.

---

### Phase 7: Save Game Backups & World Cloud Sync (Complete)
*Objective: Comprehensive world backup management, server-governed upload/download policies, and world file viewer.*

- [x] **Server-Governed Policy Enforcement:**
  - Modpack and remote instance configuration specifying `server_backups_enabled` (`true`/`false`) and remote sync endpoint.
  - Client-side refusal to upload backups when server-side backups are disabled.
- [x] **World File Viewer & Inspector (`launchcraft-backup`):**
  - Scans `<instance>/minecraft/saves/` for active singleplayer worlds.
  - Computes size on disk, last modified/played timestamps, and detects locked sessions (`session.lock`).
- [x] **ZIP Compression Engine:**
  - Compresses world directories into standalone ZIP archives (`<world>-<timestamp>.zip`) before uploading or archiving.
  - Fast, safe extraction/restoration of world ZIP archives back into the `saves/` folder.
- [x] **Bidirectional Cloud Sync & Upload/Download:**
  - HTTP multipart/streaming upload of zipped world saves to remote servers.
  - Download of remote world ZIP archives with verification and automatic restore capabilities.
- [x] **Desktop UI & IPC Integration:**
  - Worlds file viewer tab in `launchcraft-desktop`.
  - Action buttons for: "Backup World (ZIP)", "Upload to Server", "Download World ZIP", and "Restore Backup".

---

### Phase 8: Server Console (`launchcraft-server`) (Complete)
*Objective: Server-side counterpart to the backup engine — a backup store API and admin GUI for server operators.*

- [x] **Backup API (`crates/launchcraft-server`):**
  - Implements the exact contract `launchcraft-backup::BackupSyncClient` speaks:
    `POST /backups/upload?filename=…` (raw zip body) and `GET /backups/download?filename=…`,
    mounted at both `/api/*` and the root, with optional `Authorization: Bearer` auth.
  - JSON listing (`GET /backups`) and console-side deletion (`DELETE /backups/<name>`);
    BLAKE3 checksum sidecars per archive, atomic writes, strict filename allowlist (no traversal).
  - Server-side policy store (`policy.json`): while uploads are disabled the API answers `403`,
    mirroring the client-side refusal.
- [x] **Admin Console (embedded single-page GUI):**
  - Dashboard (archive count, storage used, newest upload, per-world counts), Backups
    (list / download / delete with checksums), Policy toggle, and a Pack Manager
    (pack registry with New Pack / Add Existing / Modify / Build / Check, SKCraft-style).
- [x] **Deployment:**
  - `docker/Dockerfile.server` and a `backup-server` Compose service (port 9090,
    `./server-data` volume, optional `LAUNCHCRAFT_BACKUP_TOKEN`).
- [x] **End-to-End Tests:**
  - Live HTTP round-trips using the real `WorldArchiver` and `BackupSyncClient`
    (upload → list → download → byte-identical restore), policy refusal, token gating,
    traversal rejection, and root-mount aliases.

---

## 4. Architectural Safeguards
- **Zero-trust verification:** Every file is verified by cryptographic hash before execution.
- **Fail-safe updates:** Sync changes stage files in a temporary location before atomic rename.
- **Offline resilience:** Validated instances remain playable offline when connectivity is unavailable.
- **Session safety:** World backup skips temporary lock files (`session.lock`) to avoid corrupting running games.
