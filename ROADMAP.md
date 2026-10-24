# Launchcraft Roadmap & Architecture Specification

A modern, high-performance, cross-platform Minecraft launcher built in **Rust** with a **Tauri v2** frontend. Inspired by SKCraft Launcher, modernized with modern content-addressable storage (CAS), zstandard delta compression, zero-trust integrity, and native multi-platform support.

---

## 1. Vision & Architecture Philosophy

Launchcraft modernizes the SKCraft philosophy into a fast, memory-safe, portable desktop client and headless pack-building toolchain.

### Core Stack
- **Launcher Core & Engine:** Rust (`tokio`, `reqwest`, `blake3`, `zstd`, `directories`)
- **Desktop UI Framework:** Tauri v2 + lightweight web frontend (TypeScript/Tailwind/Solid or Svelte)
- **Modpack Distribution:** HTTP/S3/CDN Content-Addressable Storage (CAS)
- **Pack Builder:** Headless Rust CLI (`launchcraft-builder`) packaged for standalone usage and Docker CI

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
    │   └── minecraft/        # Game working directory (mods, configs, saves)
    └── custom-pack/
```

---

## 3. Detailed Milestone Roadmap

```
Phase 1: Workspace & Core Foundation (Done)
   │
   ├── Phase 2: Sync Engine & Content-Addressable Storage (CAS) (Done)
   │
   ├── Phase 3: JRE Management & Minecraft Process Launching
   │
   ├── Phase 4: Authentication & Security (Microsoft OAuth / OS Keyring)
   │
   ├── Phase 5: Tauri v2 Desktop Interface & Portable Distribution
   │
   └── Phase 6: Headless Pack Builder & Docker Image
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

### Phase 3: JRE Provisioning & Process Execution (In Progress)
*Objective: Reliable, sandbox-aware Minecraft execution across Windows, macOS, and Linux.*

- [ ] **Runtime Manager (`launchcraft-runtime`):**
  - Automatically detect host OS (`windows`, `macos`, `linux`) and architecture (`x86_64`, `aarch64`).
  - Query and download correct JRE distributions (Java 8, 17, 21) from Adoptium / Zulu APIs.
  - Portable extraction: isolate JREs inside `<root_data_dir>/runtimes/`.
- [ ] **Minecraft Version Metadata & Library Resolution:**
  - Parse Mojang Version Manifest (`v2`) and Fabric/Forge/NeoForge loaders.
  - Download official Mojang assets and client libraries with parallel hash verification.
- [ ] **Game Process Spawner:**
  - Build classpath and JVM arguments (memory allocation, GC tuning flags, natives).
  - Platform-specific hooks:
    - **macOS:** `-XstartOnFirstThread`, Dock icon integration, App Nap disabling.
    - **Windows:** Process creation flags without detached zombie consoles (`javaw.exe`).
    - **Linux:** Native Wayland / GLFW handling options.
  - Pipe game standard output / standard error to launcher log stream in real time.

---

### Phase 4: Microsoft Authentication & Security
*Objective: Secure, frictionless OAuth flow conforming to official Mojang standards.*

- [ ] **Microsoft OAuth Device Flow & Authorization Code Flow:**
  - Interactive login via browser redirect / local loopback HTTP server.
  - Xbox Live authentication (`user.auth.xboxlive.com`).
  - XSTS token exchange and Minecraft services authentication (`api.minecraftservices.com`).
  - Profile verification and UUID / skin lookup.
- [ ] **Secure Token Storage:**
  - Utilize OS credential managers (`keyring` crate) where available (Windows Credential Manager, macOS Keychain, Secret Service on Linux).
  - Encrypted fallback cache for portable mode.
  - Refresh token rotation handling with grace periods for offline play.

---

### Phase 5: Tauri v2 Desktop Interface & Cross-Platform Packaging
*Objective: Modern, responsive, lightweight GUI distributed across all target operating systems.*

- [ ] **Tauri v2 Shell (`apps/launchcraft-desktop`):**
  - IPC commands binding to `launchcraft-core`, `launchcraft-sync`, and `launchcraft-runtime`.
  - Real-time download progress events (bytes transferred, download speed, hash progress).
- [ ] **User Interface Features:**
  - Modpack browser (remote synced packs + local custom instances).
  - Instance settings (RAM allocation, custom Java paths, JVM arguments, window dimensions).
  - Real-time game log viewer with search, filtering, and export.
- [ ] **Cross-Platform Bundles:**
  - **Linux:** AppImage bundle.
  - **Windows:** Standalone portable `.zip` (includes `.portable` marker) and single-user NSIS `.exe`.
  - **macOS:** Universal `.dmg` (Apple Silicon & Intel) with notarization readiness.

---

### Phase 6: Headless Pack Builder (`launchcraft-builder`) & Docker
*Objective: Automated CI/CD toolchain for server admins to compile modpacks.*

- [ ] **Headless CLI (`crates/launchcraft-builder`):**
  - Scan directory of mods, configs, and overrides.
  - Calculate BLAKE3 hashes and compress assets into zstd CAS blobs.
  - Generate `manifest.json` and upload payloads to target S3/web server.
- [ ] **Docker Container:**
  - Lightweight Alpine/Debian-based container for GitHub Actions / GitLab CI.
  - Single-command pack compilation: `docker run -v ./pack:/pack launchcraft-builder build`.

---

## 4. Architectural Safeguards
- **Zero-trust verification:** Every file is verified by cryptographic hash before execution.
- **Fail-safe updates:** Sync changes stage files in a temporary location before atomic rename.
- **Offline resilience:** Validated instances remain playable offline when connectivity is unavailable.
