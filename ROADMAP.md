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
- **Server Console:** axum JSON API + React/TypeScript/Vite SPA embedded in the `launchcraft-server` binary
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
               │
               ├── Phase 9: Client Login, Version Store & Launch (Complete)
               │
               ├── Phase 10: Custom Pack Creation UI (Complete)
               │
               ├── Phase 11: Server-Side Runtime Distribution (In Progress)
               │
               └── Version 2: Pack Studio & Game Saves (Planned — server side only)
                     │
                     ├── Phase 12: Pack Studio Console Foundation
                     │
                     ├── Phase 13: Per-Pack Mod Management
                     │
                     ├── Phase 14: Versions, Loaders & Validation
                     │
                     ├── Phase 15: Library & Distribution UI
                     │
                     ├── Phase 16: Game Saves
                     │
                     └── Phase 17: Lifecycle & Destructive Actions
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
- [x] **Pack Distribution (client-facing, public):**
  - `GET /packs` (built-pack list), `GET /packs/<id>/manifest.json` and
    `GET /packs/<id>/cas/objects/<shard>/<hash>` — the exact layout `launchcraft-sync`'s
    `SyncEngine` fetches, served without auth like a static repo-server (token-gated
    admin stays intact). Strict hex-hash validation; no path traversal.
- [x] **Launcher Pack Browser (`gui-client`):**
  - SKCraft-style Home tab: pulls `GET /packs` automatically on start-up from the
    configured server, shows per-pack state (not installed / installed /
    requires update) and installs or updates instances in the background via the
    real `SyncEngine`, recording a `RemoteSync` source in `instance.json`.
  - Live progress bar during install/update/reinstall, driven by the `SyncEngine`
    byte/file progress callbacks.
  - One **Server URL** (Home tab) drives packs and backups: installs store
    `<url>/api` as the instance backup endpoint and saving the URL re-points every
    instance; the Worlds tab shows the shared endpoint instead of editing one per instance.
    (Old `client.json` `pack_server_url` keys load unchanged via a serde alias.)
  - Java settings list every runtime found on the machine (JAVA_HOME, `/usr/lib/jvm`,
    SDKMAN, macOS/Windows trees) by parsed version — pick one or keep a custom path.
- [x] **Modpack Browser (console, server-side — CurseForge · Modrinth · FTB):**
  - One unified endpoint set (`GET /console/modpacks|search|versions`,
    `POST /console/modpacks/add|update/:id`) behind one console browser; the CurseForge
    key (`LAUNCHCRAFT_CURSEFORGE_KEY`) stays server-side and Modrinth/FTB need no key —
    CORS never enters the picture.
  - One-click **Add** rebuilds any provider's archive into a Pack Manager workspace:
    `manifest.json` (Forge) references resolved through the Core API, `modrinth.index.json`
    / `.mrpack` files fetched by their direct URLs, and legacy FTB baked `minecraft/`
    trees copied wholesale; `overrides/` merge zip-slip-safe, bounded concurrency, then
    register + build. Missing required mods abort; optional mods warn. Tests run against
    an in-process mock of all three upstreams.
  - **Updates** for imported packs (Prism-style): the registry stores
    `{provider, project, file}` provenance, so a pack row gets an "Update" action —
    re-imports the chosen (default newest) version in place, prunes dropped mods for
    manifest packs, wipes and replaces baked FTB trees, re-merges overrides, bumps
    `pack.json`'s version and rebuilds. Same-version requests report `{updated: false}`;
    hand-built packs are refused with 400.
  - **Upload Modpack…** — a sibling window of the browser (`POST
    /console/modpacks/upload`, body-gated by `LAUNCHCRAFT_MAX_UPLOAD_MB`): the picked
    archive streams into the identical import pipeline; optional fields supply name/version
    for manifest-less baked trees; uploaded packs register without provenance (no Update).
  - **Mod loader data on import** — every add/update/upload also fills `src/loader/`:
    Fabric/Quilt loader `profile.json` + all its library jars (from the loader meta APIs,
    stored under their upstream URL paths), Forge/NeoForge the official installer jar
    (from their Mavens); mrpack loader/game versions are read from `dependencies`.
    The builder records the loader in the pack manifest and the loader files ride the
    normal CAS distribution, so synced instances can launch. Baked trees are skipped
    (they ship their own); an unrecognised loader warns instead of failing the import.
- [x] **End-to-End Tests:**
  - Live HTTP round-trips using the real `WorldArchiver` and `BackupSyncClient`
    (upload → list → download → byte-identical restore), policy refusal, token gating,
    traversal rejection, and root-mount aliases.
  - A pack built through the console is installed by the real `SyncEngine` straight
    from the public `/packs` endpoints (manifest + CAS objects over HTTP).

### Phase 9: Client Login, Version Store & Launch (Complete)
- [x] **Microsoft Chain Hardening (`launchcraft-auth`):**
  - XSTS `xuid` captured into the account (feeds `${auth_xuid}` at launch); the
    token endpoint tolerates `authorization_pending` responses that omit token
    fields; `finish_device_login` drives poll → consent → full chain in one call;
    every upstream base URL injectable for tests (axum-mocked whole chain, incl.
    refresh and game-not-owned paths).
- [x] **Version & Asset Pipeline (`launchcraft-runtime::meta`):**
  - Piston-meta version list + profile download with `inheritsFrom` chain loading;
    client jar, asset index (sha1-gated) and objects (content-addressed, 16-way
    concurrent) into the standard `versions/` + `assets/` layout; applicable
    libraries and natives classifiers into a shared maven-layout `libraries/`;
    writes are tmp-then-rename and sha1-checked, caches skip by size/hash.
  - Fabric/Quilt loader installs (`<mc>-<loader>-<version>` overlay profiles,
    all three loader library dialects — `downloads`, `url`+`maven`, bare maven
    coordinates — normalized to identical paths the server's loader store uses).
- [x] **Launch Planning & Game Process:**
  - Merged profile → real launch: overlay-first classpath, zip-slip-safe natives
    extraction, rule-gated modern `arguments` and legacy `minecraftArguments`
    both fully substituted (`${auth_*}`, `${classpath}`, `${natives_directory}`…);
    `ProcessSpawner` now drains stdout **and** stderr so chatty clients can't
    deadlock on a full pipe.
- [x] **Accounts + Versions tabs (`gui-client`):**
  - Device-code card with copyable code and auto-opened sign-in page; multiple
    accounts with set-active/refresh/remove and skin-face avatars decoded
    client-side from the PNG; PLAY streams the game's console into the Logs tab.
  - Versions tab: releases/snapshots filter, fabric/quilt loader picker, live
    install progress through the shared job channel; `sha1` tamper and 404
    install paths covered by axum-mocked end-to-end tests.
- [x] **Automatic Java Provisioning (`launchcraft-runtime::java`):**
  - Profile-required Java majors (`javaVersion.majorVersion`) resolved at PLAY:
    managed runtime → configured Java if new enough → Adoptium assets lookup +
    streamed sha256-verified Temurin download extracted into
    `runtimes/java-<major>` (tar.gz and zip, zip-slip-safe, exec bits restored,
    macOS `Contents/Home` layout supported); progress streams through the shared
    job channel and failures degrade to the configured Java. Provisioned
    runtimes show up in the Java tab; live test against api.adoptium.net.

---

### Phase 10: Custom Pack Creation UI (Complete)
*Objective: Bring the SKCraft-style Pack Manager workflow into the browser-based server console, so operators can stage custom packs without host filesystem access.*

- [x] **Browser-side staging (`crates/launchcraft-server/console/index.html`):**
  - "Create Pack (browser)…" button in the Pack Manager action bar opening a modal.
  - Metadata form (pack ID, title, version, game version, summary, optional loader) validated against `BuildConfig`.
  - Multi-file and directory selection (`webkitdirectory`) plus drag-and-drop.
  - Staged file list with relative paths, sizes, and per-file removal; total size summary.
  - Unsafe path rejection (`..`, absolute, empty, NUL).
- [x] **Project archive export:**
  - Browser-side DEFLATE-compressed ZIP (`CompressionStream('deflate-raw')`, fallback to store) containing `pack.json` + staged files with relative paths.
  - Download as `${pack_id}-project.zip`.
- [x] **Connection to builder:**
  - "Upload & Build" reuses `/console/modpacks/upload` — builds the ZIP in memory and posts it; server imports, fetches loader data, and builds the manifest + CAS distribution.
  - Indeterminate progress + result reporting.
- [x] **Permissions / file handling:**
  - Static note explaining browser-only file handling; no host-folder writes; uploads land in the server data directory.
- [x] **Verification:**
  - JS syntax check, ZIP round-trip through Python `zipfile`, server smoke test (`console_html_includes_custom_pack_creator`), full server test suite green.

---

### Phase 11: Server-Side Runtime Distribution (In Progress)
*Objective: Move all Minecraft runtime provisioning (vanilla client.jar, assets, libraries, and Fabric/Quilt/Forge/NeoForge loaders) to the server, so the client only needs MS login and a file pull. Dedup runtimes shared across packs.*

- [x] **Server-side runtime fetch (`crates/launchcraft-server/src/runtime_dist.rs`):**
  - Vanilla, Fabric and Quilt: version manifest, `inheritsFrom` profile chain, client jar,
    asset index and the libraries/natives that apply to this platform are fetched into a
    throwaway `LauncherPaths` staging tree with `launchcraft-runtime::meta::MinecraftClient`,
    then packed into the runtime store; the staging tree is removed on success and failure.
  - Asset **objects** are opt-in (`LAUNCHCRAFT_RUNTIME_ASSETS`, or `assets` per request): the
    index json is always stored, the hundreds of MB of objects only on request — a first
    build costs a few jars instead of mirroring Mojang's whole asset store.
  - Forge/NeoForge are refused with an explicit message instead of half-built: their
    launch profile has to be unpacked out of the installer jar (Java itself is no longer
    the blocker — `jres` pools one JDK per major on demand). Those packs keep shipping the
    installer through `src/loader/`.
  - Store: `<data>/runtimes/registry.json` (status `building`/`ready`/`error`, sizes,
    timestamps) plus `<data>/runtimes/<id>/manifest.json` in exactly the `PackManifest`
    shape, so a client can sync a runtime with the ordinary `SyncEngine`. Stable ids:
    `{mc}--vanilla` and `{mc}--{loader}--{loader_version}`.
  - Builds run detached — `POST /console/runtimes` answers `202`, and a request for a build
    already running joins it rather than starting a second fetch.
- [x] **Dedup / cache:**
  - One shared runtime CAS (`<data>/runtimes/cas/objects/…`): blobs dedup across every
    runtime, so a Fabric and a Quilt runtime on the same version reuse the same client jar
    and library objects and a runtime record is metadata plus a manifest, not a copy.
  - Reference counting against the pack registry, with `POST /console/runtimes/gc` dropping
    runtimes no pack references plus the blobs only they used. GC skips entirely while a
    build is in flight, so it can never delete a blob about to be referenced.
    `DELETE /console/runtimes/<id>` unregisters one runtime and refuses while a pack still
    references it (`force=1` overrides). Pack deletion (Version 2 Phase 17) never touches
    runtime blobs.
- [x] **Endpoints:**
  - `GET /runtimes`, `GET /runtimes/{id}/manifest.json`,
    `GET /runtimes/{id}/cas/objects/{shard}/{hash}` — client-facing like `/packs` (no auth),
    with the same lowercase-hex + shard-prefix fencing; admin list/queue/delete/gc under
    `/console/runtimes` keep the bearer token.
- [x] **Pack integration:**
  - Modpack import (`add`, `upload`, `update`) derives the runtime from the pack's game
    version + loader, queues the build and records `runtime_id` in the pack registry; the id
    rides along in `GET /packs`, so a launcher resolves pack and runtime in one round trip.
    A runtime problem is a warning, never a lost import.
  - Recorded in the registry rather than in `manifest.json` on purpose: that manifest is the
    on-disk contract with already-deployed clients, and extending it would mean changing the
    builder too — outside this release's server-only scope.
- [x] **Multi-major Java pool (`crates/launchcraft-server/src/jres.rs`):**
  - One server, many Java eras: `1.7.10`/`1.10.2`/`1.12.2` → 8, `1.17` → 16,
    `1.18`–`1.20.4` → 17, `1.20.5`+ → 21, snapshots by year, and a profile's own
    `javaVersion.majorVersion` always wins. Java is keyed by major and pooled in
    `<data>/jres/java-<major>` (`LAUNCHCRAFT_JRES`), so eras coexist instead of
    fighting over one `JAVA_HOME`.
  - Resolution order: `LAUNCHCRAFT_JAVA_<major>` → already-provisioned managed JDK →
    host JDK (`java` on PATH, then `JAVA_HOME`; `LAUNCHCRAFT_JRES_HOST_JAVA=0` ignores
    them) → sha256-verified Temurin download through the existing
    `launchcraft-runtime::java` provisioning. Host paths are canonicalised before
    measuring, so `/usr/bin/java` reports its real JDK home (~130 MB) instead of
    `/usr` (~17 GB), and a record whose binary vanished is pruned on read.
  - Runtime records carry `required_java`; `GET /console/jres` reports per major
    (available, source `env`/`system`/`managed`, version string, size, `required_by`),
    `POST /console/jres` pre-provisions, `DELETE /console/jres/<major>` is refused while
    a runtime needs it (`force=1` overrides) and only forgets host JDKs rather than
    deleting them. Records whose binary vanished are pruned on read.
  - Java 7/6 (Minecraft ≤ 1.5.x) are listed as non-downloadable with a message pointing
    at `LAUNCHCRAFT_JAVA_7`, because Adoptium does not publish them.
  - Tests: five integration tests against a mock Adoptium API serving a real tar.gz
    "JDK" whose `bin/java` is a version-printing script — covering two eras side by side,
    reuse without re-download, persistence across restarts, refusal/force delete, the
    Java 7 override path, unknown majors and token gating; plus unit tests for the
    version table and the resolution preference order.
- [x] **Client side (`gui-client/src/runtimes.rs`):**
  - `runtimes.json` (next to `client.json`) records each applied runtime: id, install
    version, server URL, file count, size, manifest BLAKE3 and timestamp — that is what
    lets a re-sync say "unchanged since last sync" instead of re-fetching.
  - Installing a pack that names a `runtime_id` stages the runtime into the data root
    first (`versions/`, `assets/`, `libraries/`), through the same `CasDownloader` the pack
    sync uses, so one client jar is hard-linked into place for every pack that needs it.
  - **Additive by design.** The data root also holds `instances/`, `config.json` and the
    local CAS, so a runtime sync writes exactly the files its manifest names and deletes
    nothing; every manifest path must start with `versions/`, `assets/` or `libraries/` and
    has no `..`, absolute or empty segments. Files already on disk are re-verified by size
    then BLAKE3, which heals a truncated jar and picks up a server rebuild without
    downloading what is already right. The manifest's `pack_id` must equal the requested
    id, so a server that answers the wrong store is refused rather than installed.
  - PLAY no longer dead-ends: when the instance's version id is missing but the pack names
    a runtime, the launcher syncs it from the configured server and asks the user to press
    PLAY again. The Versions tab keeps a "Shared runtimes from the server" list with a
    per-entry re-sync, and manual installs stay the fallback (nothing removed).
  - Live test (`--ignored`, needs network) runs the real contract: a launchcraft-server
    stages Minecraft 1.7.10 from Mojang, this launcher installs it over HTTP, the client jar
    lands at its exact size, an unrelated file under `libraries/` survives, and the second
    pass downloads zero files.
- [~] **Docker:**
  - `docker/Dockerfile.server` ships `openjdk-17-jre-headless` as the common-case
    bootstrap; every other major is provisioned onto the `/data` volume on first use, so
    a container restart never re-downloads a JDK. `LAUNCHCRAFT_ADOPTIUM_URL` points
    provisioning at a mirror and `LAUNCHCRAFT_JAVA` / `LAUNCHCRAFT_JAVA_<major>` at a
    mounted JDK for air-gapped hosts (both documented in `compose.yaml`).
  - Still open: a JDK that can *run* the legacy Forge installer (Java 8 with the
    `unpack200` those installers shell out to) as a build-time dependency rather than a
    runtime download — the remaining prerequisite for Forge/NeoForge runtime builds.

---

## 4. Version 2 Update — Pack Studio & Game Saves (Planned)

*Target release: **v2.0.0** (workspace crates currently at `0.1.0`).*

*Scope guard: everything below is **server side only** — `crates/launchcraft-server` (console UI + JSON API). The launcher (`gui-client`), the desktop app, `launchcraft-builder`, `launchcraft-sync`, the CAS/manifest format and the public `/packs` distribution contract stay unchanged. No Node toolchain enters `cargo build` or the Docker images.*

### Phase 12: Pack Studio Console Foundation (Planned)
*Objective: Replace the single embedded HTML page with a React + TypeScript + Vite console organised around packs instead of tables and modals.*

- [ ] **Embedded SPA build (`crates/launchcraft-server/ui/` → `console/`):**
  - Vite + React + TypeScript sources under `ui/`; build output committed to `console/` and embedded with `include_dir!`, so `cargo build`, `cargo test` and `docker/Dockerfile.server` still need no Node.
  - `/console` serves the SPA with history fallback and `/` redirects to it; hashed assets under `/console/assets/*` stay public while every `/console/*` data route keeps the bearer-token check.
  - CI `ui` job runs `npm ci && npm run build` then `git diff --exit-code -- crates/launchcraft-server/console`, so the committed build can never drift.
  - Dev loop: `npm run dev` with Vite proxying `/console` to the running Rust server.
- [ ] **Pack-centric shell:** left rail of pack cards (icon, title, game-version + loader badges, build status, search/filter) and a tabbed pane — Overview · Mods · Files · Builds · Distribute — with Dashboard, Backups and Policy kept as existing views.
- [ ] **Pack detail API:** `GET /console/packs/:id` (registry row + `pack.json` + dist status) and `GET/PUT /console/packs/:id/config`.
- [ ] **Workspace files API:** list / read / write / delete / upload under `packs/<id>/src`, guarded by the same containment rules as the CAS shard check, with a 2 MB text cap, binary detection for jars and zips, and no symlink following.
- [ ] **Studio metadata:** `packs/<id>/studio.json` next to `src/` and `dist/` holds build history and mod provenance — kept outside the workspace on purpose, because the builder only skips `pack.json` and would otherwise pack metadata into every manifest.
- [ ] **Verification:** asset fallback and redirect tests, traversal and upload-cap rejection, build-history round trip, existing server suite green.

### Phase 13: Per-Pack Mod Management (Planned)
*Objective: Prism-style mod handling inside a pack workspace — search, install with dependency resolution, enable/disable, update, remove — with provider keys never leaving the server.*

- [ ] **Mod providers:** Modrinth (keyless) and CurseForge (gated by `LAUNCHCRAFT_CURSEFORGE_KEY`) behind one interface, in new modules rather than widening the pack-import pipeline; search facets derive from the open pack's game version and loader kind.
- [ ] **Installed mods:** `GET /console/packs/:id/mods` joins `src/mods/*.jar[.disabled]` with `studio.json` provenance (provider, project, version, client/server side, installed at).
- [ ] **Install with dependency resolution:** `POST /console/packs/:id/mods` walks provider dependency metadata recursively — cycle guard, already-present skip, optional and incompatible versions reported — downloads into `src/mods/`, records provenance, and returns the resolved set.
- [ ] **Lifecycle:** enable/disable by renaming to `.jar.disabled`, update to the newest provider version compatible with the pack, remove; duplicate-project and client-only markers in the response.
- [ ] **Verification:** hermetic provider mocks, dependency-graph unit tests, install / toggle / update / remove round trips on a temporary workspace.

### Phase 14: Versions, Loaders & Validation (Planned)
*Objective: Replace free-text loader strings with real pickers, and tell the operator when a pack is inconsistent.*

- [ ] `GET /console/game-versions` (Mojang manifest, cached in-process) and `GET /console/loaders/:kind/versions?game=` (Fabric/Quilt meta APIs, Forge/NeoForge promotions).
- [ ] Overview pickers write `pack.json`; changing the loader refetches loader files through the existing `loader.rs` path so `src/loader/` always matches.
- [ ] Validation warnings: loader or game-version mismatch, client-only mods in a synced pack, duplicate projects, provenance pointing at files no longer present, known overlaps (OptiFine against Sodium + Iris).
- [ ] No builder, manifest or client changes.

### Phase 15: Library & Distribution UI (Planned)
*Objective: Make the pack list a library, and surface the distribution a built pack already provides.*

- [ ] Denser library over the same registry: grouping by authored / imported / hand-built, source badges, duplicate and fork, archive, quick update from stored provenance.
- [ ] Distribute tab is read-only over today's public contract — client endpoint URLs, manifest summary, BLAKE3 check report — plus a `.mrpack` export of the workspace.
- [ ] Build dialog with version bump; build history kept in `studio.json`; Check reuses `verify_pack_dist`.

### Phase 16: Game Saves (Planned)
*Objective: Turn the Backups and Policy views into a world-centric save browser.*

- [ ] Console navigation becomes Dashboard · Studio · Game Saves; the upload-policy switch moves into the Game Saves header and keeps driving the same `policy.json` and the `403` behaviour for `POST /backups/upload`.
- [ ] `GET /console/saves` groups `BackupStore` entries by world (archive count, total size, latest upload, linked pack); the detail pane lists every archive with timestamp, type, size and checksum.
- [ ] Per-archive download and delete reuse the existing endpoints; world-level deletion lands in Phase 17.
- [ ] Restore stays a client-side operation (`WorldArchiver::restore_backup`), so the server UI offers **Download** and marks a version as current only; a true restore needs client work and is out of this release.

### Phase 17: Lifecycle & Destructive Actions (Planned)
*Objective: Make deletion real, explicit, and safe for shared storage.*

- [ ] **Confirm dialogs for anything that removes data from disk:** pack deletion requires typing the pack id and offers "Unregister only" as the non-destructive alternative; world deletion requires acknowledging the exact archive count and size; single-archive deletion is a plain confirm.
- [ ] **`DELETE /console/packs/:id?files=1`:** removes `src/`, `dist/`, `studio.json` and the build history **only** when both resolved paths live under `<data>/packs/<id>/`. "Add Existing" may register arbitrary server paths, so anything outside the managed tree degrades to unregister-only with an explicit warning. Default (no flag) keeps today's unregister-only behaviour.
- [ ] **`DELETE /console/saves/:world`:** removes every archive and sidecar belonging to one world and clears its current pointer.
- [ ] **Audit trail:** one log line per destructive action recording actor token presence, target, mode (unregister vs delete) and reclaimed bytes; unregistering is documented in the UI as *not* revocation — clients that already synced keep their copy.
- [ ] **Phase 11 integration:** pack deletion must never remove runtime blobs — runtimes
    live in their own shared CAS and are reference-counted, with
    `POST /console/runtimes/gc` (already shipped) dropping runtimes no pack references plus
    the blobs only they used. Deleting a pack therefore only has to clear its own
    `src/`, `dist/` and `studio.json`.
- [ ] **Verification:** path-containment tests including symlinked `src`/`dist`, traversal rejection, cascade versus unregister-only behaviour, world grouping, console dialog smoke tests.

**Version 2 non-goals:** running Minecraft servers (no process, RCON or world control), any launcher/desktop/builder/sync change, an auth model beyond the current bearer token, and per-world backup retention or quota enforcement (still unclaimed work).

---

## 5. Architectural Safeguards
- **Zero-trust verification:** Every file is verified by cryptographic hash before execution.
- **Fail-safe updates:** Sync changes stage files in a temporary location before atomic rename.
- **Offline resilience:** Validated instances remain playable offline when connectivity is unavailable.
- **Session safety:** World backup skips temporary lock files (`session.lock`) to avoid corrupting running games.
